// Copyright (C) 2026 The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Source navigation and display helpers shared by the `verify` and `binder`
//! reporters: index a checkout by file name, resolve a class to its file, print
//! context windows, and format method/lock names compactly.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub struct Source {
    /// file basename (e.g. `HdmiControlService.java`) -> candidate paths
    by_name: HashMap<String, Vec<PathBuf>>,
    cache: HashMap<PathBuf, Vec<String>>,
}

impl Source {
    /// Index every `.java` under `root`, skipping build/VCS dirs.
    pub fn index(root: &Path) -> Self {
        let mut by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut stack = vec![root.to_path_buf()];
        let skip = ["out", ".git", ".repo", "prebuilts", "node_modules", ".gradle"];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let ft = match e.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    if !skip.contains(&name) {
                        stack.push(p);
                    }
                } else if p.extension().map(|x| x == "java").unwrap_or(false) {
                    if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                        by_name.entry(n.to_string()).or_default().push(p.clone());
                    }
                }
            }
        }
        Source { by_name, cache: HashMap::new() }
    }

    /// Resolve a dotted class FQN (`a.b.C$Inner`) to its source file.
    pub fn file_for(&self, class_fqn: &str) -> Option<PathBuf> {
        let outer = class_fqn.split('$').next().unwrap_or(class_fqn);
        let simple = outer.rsplit('.').next()?;
        let want = format!("{}.java", simple);
        let pkg_path = outer.replace('.', "/"); // a/b/C
        let cands = self.by_name.get(&want)?;
        cands
            .iter()
            .find(|p| p.to_string_lossy().contains(&pkg_path))
            .or_else(|| cands.first())
            .cloned()
    }

    pub fn lines(&mut self, path: &Path) -> &[String] {
        self.cache
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .map(|s| s.to_string())
                    .collect()
            })
    }
}

/// Display path of a file relative to `root`.
pub fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().to_string()
}

/// `a.b.C$1.m:(...)V` -> `C$1.m` for compact path display. D8 nest-access bridges
/// (`-$$Nest$mfoo`) are de-mangled to the method they forward to (`foo`).
pub fn short_method(key: &str) -> String {
    let cm = key.split(':').next().unwrap_or(key); // a.b.C$1.m
    match cm.rfind('.') {
        Some(dot) => {
            let method = cm[dot + 1..].strip_prefix("-$$Nest$m").unwrap_or(&cm[dot + 1..]);
            let cls = cm[..dot].rsplit('.').next().unwrap_or(&cm[..dot]);
            format!("{cls}.{method}")
        }
        None => cm.to_string(),
    }
}

/// Dotted declaring class of a method key: `a.b.C.m:(...)V` -> `a.b.C`.
pub fn class_path(method_key: &str) -> Option<String> {
    let cm = method_key.split(':').next().unwrap_or(method_key);
    let dot = cm.rfind('.')?;
    Some(cm[..dot].to_string())
}

/// `com.android.server.am.UserController.mLock` -> `UserController.mLock`.
pub fn short_lock(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        name.to_string()
    }
}

/// Escape a string for a Graphviz double-quoted label.
pub fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Inline a fenced source window for `class_fqn:line` into `out`, resolving the
/// class to a file under the indexed checkout. No-op if the file isn't found.
pub fn snippet(out: &mut String, src: &mut Source, root: Option<&Path>, class_fqn: &str, line: usize) {
    let Some(file) = src.file_for(class_fqn) else { return };
    let shown = root.map(|r| rel(&file, r)).unwrap_or_else(|| file.display().to_string());
    let _ = writeln!(out, "\n```text\n  {shown}:{line}");
    let lines = src.lines(&file).to_vec();
    print_ctx(out, &lines, line, 4, 2);
    let _ = writeln!(out, "```");
}

/// Print `before`/`after` lines around `line` (1-based). Returns true if an async
/// dispatch (post/execute/sendMessage) appears in the window.
pub fn print_ctx(out: &mut String, lines: &[String], line: usize, before: usize, after: usize) -> bool {
    if line == 0 || line > lines.len() {
        return false;
    }
    let lo = line.saturating_sub(before).max(1);
    let hi = (line + after).min(lines.len());
    let mut asyncy = false;
    for i in lo..=hi {
        let txt = &lines[i - 1];
        if txt.contains(".post(") || txt.contains(".postDelayed(") || txt.contains(".execute(")
            || txt.contains(".sendMessage")
        {
            asyncy = true;
        }
        let mark = if i == line { ">>" } else { "  " };
        let _ = writeln!(out, "      {mark}{i:>5}  {txt}");
    }
    asyncy
}

/// Lines in a file where `field` is the monitor of a `synchronized` (or a juc lock op).
pub fn acquire_sites(lines: &[String], field: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with('*') || t.starts_with("//") || t.starts_with("/*") {
            continue; // skip comments / javadoc that merely mention the lock
        }
        let is_sync = l.contains("synchronized");
        let is_juc = l.contains(".lock(") || l.contains(".writeLock(") || l.contains(".readLock(");
        if (is_sync || is_juc) && contains_word(l, field) {
            out.push((i + 1, l.clone()));
        }
    }
    out
}

/// Whole-word containment (so `mLock` does not match `mLockProfile`).
pub fn contains_word(s: &str, w: &str) -> bool {
    let bytes = s.as_bytes();
    let wb = w.as_bytes();
    let mut i = 0;
    while let Some(pos) = s[i..].find(w) {
        let start = i + pos;
        let end = start + wb.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
    }
    false
}

fn is_ident(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphanumeric()
}
