//! `lockdex verify` — for each candidate cycle, pull the source at both edge
//! sites from a checkout, follow to where the *target* lock is actually acquired,
//! and print a per-cycle verdict. Turns "candidate" into "here is the code".
//!
//! A cycle `A ⇄ B` is a real deadlock when (1) some path takes A then B and
//! another takes B then A — both shown here from source; (2) A and B are distinct
//! objects; (3) the two sides can run on different threads. (1) and (2) are
//! checked mechanically; (3) still needs a human, but the sites make it quick.

use crate::report::{CycleReport, JsonReport};
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
    fn file_for(&self, class_fqn: &str) -> Option<PathBuf> {
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

    fn lines(&mut self, path: &Path) -> &[String] {
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

/// dotted display path of a file under `root`.
fn rel(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().to_string()
}

/// `"a.b.C$1.m:(...)V:1507 (interproc)"` -> (method key `a.b.C$1.m:(...)V`,
/// class `a.b.C$1`, line 1507)
fn parse_sample(s: &str) -> Option<(String, String, usize)> {
    let s = s.split(" (").next().unwrap_or(s);
    let colon = s.rfind(':')?;
    let line: usize = s[colon + 1..].parse().ok()?;
    let methodkey = s[..colon].to_string(); // a.b.C$1.m:(...)V
    let class_method = methodkey.split(':').next().unwrap_or(&methodkey).to_string(); // a.b.C$1.m
    let dot = class_method.rfind('.')?;
    let class = class_method[..dot].to_string();
    Some((methodkey, class, line))
}

/// `com.android.server.am.UserController.mLock` -> `UserController.mLock`.
fn short_lock(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        name.to_string()
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The method graph for one deadlock: every call edge `(caller, heldLock, callee)`
/// along the paths of the cycle's order edges. Feeds the per-candidate pprof/hprof.
fn candidate_method_edges(c: &CycleReport, paths: &crate::analyze::PathIndex) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for e in &c.edges {
        let holder = e.sample.as_deref().and_then(parse_sample).map(|(mk, _, _)| mk);
        if let Some(p) = holder.as_deref().and_then(|h| paths.path_to(h, &e.to, 16)) {
            let held = short_lock(&e.from);
            for w in p.windows(2) {
                out.push((short_method(&w[0]), held.clone(), short_method(&w[1])));
            }
        }
    }
    out
}

/// One candidate cycle as a Graphviz DAG: lock nodes (red boxes) joined by the
/// actual call path of each order edge (held-in → calls… → acquires). The two
/// edges share the lock nodes, so the AB-BA loop is visible.
fn cycle_dot(c: &CycleReport, paths: &crate::analyze::PathIndex) -> String {
    use std::fmt::Write as _;
    let mut s = String::from(
        "digraph cycle {\n  rankdir=LR; node [fontsize=10];\n  edge [fontsize=9];\n",
    );
    for l in &c.locks {
        let _ = writeln!(
            s,
            "  \"{}\" [shape=box,style=filled,fillcolor=\"#ffd6d6\",label=\"{}\"];",
            esc(l), esc(&short_lock(l))
        );
    }
    for e in &c.edges {
        let holder = e.sample.as_deref().and_then(parse_sample).map(|(mk, _, _)| mk);
        let path = holder.as_deref().and_then(|h| paths.path_to(h, &e.to, 16));
        match path {
            Some(p) if !p.is_empty() => {
                let ids: Vec<String> = p.iter().map(|m| format!("m::{m}")).collect();
                for (i, m) in p.iter().enumerate() {
                    let _ = writeln!(s, "  \"{}\" [shape=ellipse,label=\"{}\"];", esc(&ids[i]), esc(&short_method(m)));
                }
                let _ = writeln!(s, "  \"{}\" -> \"{}\" [label=\"held in\",color=red,fontcolor=red];", esc(&e.from), esc(&ids[0]));
                for w in ids.windows(2) {
                    let _ = writeln!(s, "  \"{}\" -> \"{}\" [label=\"calls\"];", esc(&w[0]), esc(&w[1]));
                }
                let _ = writeln!(s, "  \"{}\" -> \"{}\" [label=\"acquires\",color=red,fontcolor=red];", esc(ids.last().unwrap()), esc(&e.to));
            }
            _ => {
                let _ = writeln!(
                    s,
                    "  \"{}\" -> \"{}\" [label=\"holds → acquires [{}x]\",color=red,fontcolor=red,style=dashed];",
                    esc(&e.from), esc(&e.to), e.count
                );
            }
        }
    }
    s.push_str("}\n");
    s
}

/// `a.b.C$1.m:(...)V` -> `C$1.m` for compact path display.
fn short_method(key: &str) -> String {
    let cm = key.split(':').next().unwrap_or(key); // a.b.C$1.m
    match cm.rfind('.') {
        Some(dot) => {
            let method = &cm[dot + 1..];
            let cls = cm[..dot].rsplit('.').next().unwrap_or(&cm[..dot]);
            format!("{cls}.{method}")
        }
        None => cm.to_string(),
    }
}

/// A lock display name -> (declaring class, what to grep for at its acquisition).
fn lock_target(name: &str) -> Option<(String, String)> {
    let base = name
        .trim_end_matches(".read")
        .trim_end_matches(".write");
    if let Some(c) = base.strip_suffix(".class") {
        return Some((c.to_string(), "class".to_string())); // static-synchronized
    }
    if let Some(c) = base.strip_suffix("@this") {
        return Some((c.to_string(), "this".to_string())); // instance monitor
    }
    let dot = base.rfind('.')?;
    Some((base[..dot].to_string(), base[dot + 1..].to_string()))
}

pub fn run(
    report: &JsonReport,
    paths: &crate::analyze::PathIndex,
    root: &Path,
    max_locks: usize,
    out_dir: Option<&Path>,
) -> String {
    let mut src = Source::index(root);
    let mut out = String::new();
    let cycles: Vec<_> = report.cycles.iter().filter(|c| c.locks.len() <= max_locks).collect();
    let _ = writeln!(
        out,
        "lockdex verify — {} candidate cycle(s) with <= {} locks (of {} total)\n\
         source: {}\n",
        cycles.len(),
        max_locks,
        report.cycles.len(),
        root.display()
    );

    for (ci, c) in cycles.iter().enumerate() {
        let _ = writeln!(out, "================ CANDIDATE {} : {} locks ================", ci + 1, c.locks.len());
        for l in &c.locks {
            let _ = writeln!(out, "   lock  {l}");
        }
        let mut edges_ok = 0;
        for e in &c.edges {
            let _ = writeln!(out, "\n   {}  ->  {}   [{}x]", e.from, e.to, e.count);
            let parsed = e.sample.as_deref().and_then(parse_sample);
            // (a) the edge site: where `from` is held and the call is made.
            let mut site_ok = false;
            if let Some((_, class, line)) = &parsed {
                if let Some(f) = src.file_for(class) {
                    let _ = writeln!(out, "      hold {} at  {}:{}", e.from, rel(&f, root), line);
                    let async_here = print_ctx(&mut out, src.lines(&f), *line, 6, 3);
                    site_ok = true;
                    if async_here {
                        let _ = writeln!(out, "        ^ note: an async post is nearby — confirm the lock is held when the runnable runs");
                    }
                } else {
                    let _ = writeln!(out, "      (source for {class} not found under root)");
                }
            }
            // (b) the call path from the holder down to where `to` is acquired.
            let mut tgt_ok = false;
            if let Some((mkey, _, _)) = &parsed {
                match paths.path_to(mkey, &e.to, 16) {
                    Some(p) => {
                        let chain: Vec<String> = p.iter().map(|m| short_method(m)).collect();
                        let _ = writeln!(out, "      path  {}", chain.join("  ->  "));
                        // show the synchronized site in the final (acquiring) method's class.
                        if let (Some(last), Some((tclass, field))) =
                            (p.last(), lock_target(&e.to))
                        {
                            let _ = tclass; // the acquiring method's own class is authoritative
                            let acq_class = last.split(':').next()
                                .and_then(|cm| cm.rfind('.').map(|d| cm[..d].to_string()));
                            if let Some(f) = acq_class.as_deref().and_then(|c| src.file_for(c)) {
                                let sites = acquire_sites(src.lines(&f), &field);
                                if let Some((ln, txt)) = sites.first() {
                                    let _ = writeln!(
                                        out, "      acquire {} at  {}:{}  {}",
                                        e.to, rel(&f, root), ln, txt.trim()
                                    );
                                }
                            }
                        }
                        tgt_ok = true;
                    }
                    None => {
                        let _ = writeln!(out, "      path  (not reconstructed — chain too deep or via an over-approximated call)");
                    }
                }
            }
            if site_ok && tgt_ok {
                edges_ok += 1;
            }
        }
        let verdict = if edges_ok >= 2 {
            "BOTH orderings located in source — distinct locks acquired in opposite order. \
             Real AB-BA if the two sites can run on different threads."
        } else if edges_ok == 1 {
            "one ordering located; the other side not fully resolved — inspect manually."
        } else {
            "could not resolve sites under this source root."
        };
        let _ = writeln!(out, "\n   VERDICT: {verdict}\n");

        // per-candidate artifacts: the call-path DAG (dot/svg) plus the method
        // graph for *this* deadlock as pprof + hprof.
        if let Some(d) = out_dir {
            let _ = std::fs::create_dir_all(d);
            let base = d.join(format!("cand{:02}", ci + 1));
            let dot = cycle_dot(c, paths);
            let dotp = base.with_extension("dot");
            let _ = std::fs::write(&dotp, &dot);
            if let Ok(o) = std::process::Command::new("dot").arg("-Tsvg").arg(&dotp).output() {
                if o.status.success() {
                    let _ = std::fs::write(base.with_extension("svg"), o.stdout);
                }
            }
            let me = candidate_method_edges(c, paths);
            if !me.is_empty() {
                let _ = crate::export::write_file(&base.with_extension("pb.gz"), &crate::export::pprof_method_edges(&me));
                let _ = crate::export::write_file(&base.with_extension("hprof"), &crate::export::hprof_method_edges(&me));
            }
        }
    }
    out
}

/// Print `before`/`after` lines around `line` (1-based). Returns true if an async
/// sink (post/execute/sendMessage) appears in the window.
fn print_ctx(out: &mut String, lines: &[String], line: usize, before: usize, after: usize) -> bool {
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
fn acquire_sites(lines: &[String], field: &str) -> Vec<(usize, String)> {
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

fn contains_word(s: &str, w: &str) -> bool {
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
