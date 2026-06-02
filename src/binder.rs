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

//! `lockdex binder` — render the locks-across-IPC findings: a Markdown report, a
//! per-finding call-path diagram (the chain from the lock holder to the Binder
//! transaction), and, with a source checkout, the holding site in context.

use crate::analyze::{Analysis, IncomingFinding, OutgoingFinding};
use crate::source::{class_path, esc, print_ctx, rel, short_lock, short_method, Source};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// Which side(s) to report.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Direction> {
        match s {
            "out" | "outgoing" => Some(Direction::Outgoing),
            "in" | "incoming" => Some(Direction::Incoming),
            "both" => Some(Direction::Both),
            _ => None,
        }
    }
    fn wants_out(self) -> bool {
        self != Direction::Incoming
    }
    fn wants_in(self) -> bool {
        self != Direction::Outgoing
    }
}

/// Diagrams beyond this many per side are skipped (the Markdown still lists them).
const MAX_DIAGRAMS: usize = 40;

/// Build the Markdown report. When `out_dir` is set, also write a call-path SVG for
/// each of the first [`MAX_DIAGRAMS`] findings per side; when `src` is set, inline
/// the source at each holding site.
pub fn report(an: &Analysis, dir: Direction, src_root: Option<&Path>, out_dir: Option<&Path>) -> String {
    let b = &an.binder;
    let mut src = src_root.map(Source::index);
    if let Some(d) = out_dir {
        let _ = std::fs::create_dir_all(d);
    }

    let mut md = String::new();
    let _ = writeln!(md, "# lockdex — locks held across Binder IPC\n");
    let _ = writeln!(
        md,
        "These are not deadlock cycles. Each is a place where a lock is held while a \
         thread crosses a process boundary — a cross-process deadlock / priority-inversion / \
         ANR hazard, because the peer process is outside this analysis.\n"
    );
    let _ = writeln!(
        md,
        "- **{}** outgoing hold-sites (a lock held across an outgoing transaction)\n\
         - **{}** incoming server entries that take locks ({} high-risk)\n",
        b.outgoing.len(),
        b.incoming.len(),
        b.incoming.iter().filter(|f| f.high).count()
    );

    if dir.wants_out() {
        outgoing_section(&mut md, &b.outgoing, src.as_mut(), src_root, out_dir);
    }
    if dir.wants_in() {
        incoming_section(&mut md, an, out_dir);
    }
    md
}

/// Call-graph edges for pprof/hprof export: every step of every finding's path,
/// labelled by the lock involved, terminating at a `Binder IPC` pseudo-node
/// (outgoing) or the acquired lock (incoming). Lets `go tool pprof` walk the
/// holder → … → boundary chains.
pub fn method_edges(an: &Analysis, dir: Direction) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    if dir.wants_out() {
        for f in &an.binder.outgoing {
            let held = f.held.first().map(|l| short_lock(l)).unwrap_or_default();
            for w in f.path.windows(2) {
                out.push((short_method(&w[0]), held.clone(), short_method(&w[1])));
            }
            if let Some(last) = f.path.last() {
                out.push((short_method(last), held.clone(), "Binder IPC".to_string()));
            }
        }
    }
    if dir.wants_in() {
        for f in &an.binder.incoming {
            for lock in &f.locks {
                let Some(p) = an.paths.path_to(&f.entry, lock, 16) else { continue };
                let l = short_lock(lock);
                for w in p.windows(2) {
                    out.push((short_method(&w[0]), l.clone(), short_method(&w[1])));
                }
                if let Some(last) = p.last() {
                    out.push((short_method(last), l.clone(), short_lock(lock)));
                }
            }
        }
    }
    out
}

/// Sample holder sites shown per lock group before the rest are summarized.
const SAMPLE: usize = 6;

/// Outgoing findings grouped by the held lock and ranked by frequency — so the
/// global locks that are most often held across IPC rise to the top. A site whose
/// finding holds two locks appears under each.
fn outgoing_section(
    md: &mut String,
    out: &[OutgoingFinding],
    mut src: Option<&mut Source>,
    root: Option<&Path>,
    out_dir: Option<&Path>,
) {
    let _ = writeln!(md, "## Outgoing — a lock held across an outgoing Binder transaction\n");
    if out.is_empty() {
        let _ = writeln!(md, "_none found._\n");
        return;
    }
    let mut by_lock: HashMap<&str, Vec<&OutgoingFinding>> = HashMap::new();
    for f in out {
        for h in &f.held {
            by_lock.entry(h).or_default().push(f);
        }
    }
    let mut groups: Vec<(&str, Vec<&OutgoingFinding>)> = by_lock.into_iter().collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    let _ = writeln!(
        md,
        "{} hold-site(s) across {} distinct lock(s), ranked by how often each lock is held across IPC.\n",
        out.len(), groups.len()
    );

    let mut diagrams = 0usize;
    for (lock, mut sites) in groups {
        sites.sort_by(|a, b| (&a.holder, a.line).cmp(&(&b.holder, b.line)));
        sites.dedup_by(|a, b| a.holder == b.holder && a.line == b.line);
        let _ = writeln!(md, "### `{}` — held across {} outgoing transaction(s)\n", short_lock(lock), sites.len());
        for f in sites.iter().take(SAMPLE) {
            let loc = f.line.map(|l| format!(" (line {l})")).unwrap_or_default();
            let mut bullet = format!("- `{}`{loc} → via `{}` → **Binder IPC**", short_method(&f.holder), f.via);
            // The top sites get a call-path diagram and an inlined source snippet.
            if diagrams < MAX_DIAGRAMS {
                if let Some(d) = out_dir {
                    if let Some(name) = render(d, &format!("out{:03}", diagrams + 1), &outgoing_dot(f)) {
                        let _ = write!(bullet, "  — `{name}`");
                    }
                }
                let _ = writeln!(md, "{bullet}");
                if let (Some(s), Some(line)) = (src.as_deref_mut(), f.line) {
                    inline_source(md, s, root, &f.holder, line as usize);
                }
                diagrams += 1;
            } else {
                let _ = writeln!(md, "{bullet}");
            }
        }
        if sites.len() > SAMPLE {
            let _ = writeln!(md, "- _(+{} more site(s))_", sites.len() - SAMPLE);
        }
        let _ = writeln!(md);
    }
    if diagrams >= MAX_DIAGRAMS {
        let _ = writeln!(md, "_(diagrams + source inlined for the first {MAX_DIAGRAMS} sites.)_\n");
    }
}

/// Incoming entries, high-risk first (a lock held across the entry's own outgoing
/// transaction), then a compact reference list of the rest.
fn incoming_section(md: &mut String, an: &Analysis, out_dir: Option<&Path>) {
    let inc = &an.binder.incoming;
    let _ = writeln!(md, "## Incoming — Binder server entries that take locks\n");
    if inc.is_empty() {
        let _ = writeln!(md, "_none found._\n");
        return;
    }
    let highs: Vec<&IncomingFinding> = inc.iter().filter(|f| f.high).collect();
    let others: Vec<&IncomingFinding> = inc.iter().filter(|f| !f.high).collect();
    let _ = writeln!(
        md,
        "{} entr(ies) a remote caller can make take a lock; {} high-risk.\n",
        inc.len(), highs.len()
    );

    if !highs.is_empty() {
        let _ = writeln!(md, "### High-risk — a lock held across the entry's own outgoing call\n");
        for (i, f) in highs.iter().enumerate() {
            let locks = f.locks.iter().map(|l| short_lock(l)).collect::<Vec<_>>().join(", ");
            let mut bullet = format!("{}. `{}` — acquires {locks}", i + 1, short_method(&f.entry));
            if i < MAX_DIAGRAMS {
                if let Some(d) = out_dir {
                    if let Some(name) = render(d, &format!("in{:03}", i + 1), &incoming_dot(an, f)) {
                        let _ = write!(bullet, "  — `{name}`");
                    }
                }
            }
            let _ = writeln!(md, "{bullet}");
        }
        let _ = writeln!(md);
    }

    if !others.is_empty() {
        let _ = writeln!(md, "### Other entries — locks a remote caller can make them take\n");
        for f in &others {
            let locks = f.locks.iter().map(|l| short_lock(l)).collect::<Vec<_>>().join(", ");
            let _ = writeln!(md, "- `{}` — {locks}", short_method(&f.entry));
        }
        let _ = writeln!(md);
    }
}

/// Inline the source at a holding site, resolving the holder's class to a file.
fn inline_source(md: &mut String, src: &mut Source, root: Option<&Path>, holder: &str, line: usize) {
    let Some(class) = class_path(holder) else { return };
    let Some(file) = src.file_for(&class) else { return };
    let shown = root.map(|r| rel(&file, r)).unwrap_or_else(|| file.display().to_string());
    let _ = writeln!(md, "\n```text\n  {shown}:{line}");
    let lines = src.lines(&file).to_vec();
    print_ctx(md, &lines, line, 4, 2);
    let _ = writeln!(md, "```");
}

const LOCK_NODE: &str = "shape=box, style=\"filled,rounded\", fillcolor=\"#fee2e2\", color=\"#dc2626\", penwidth=2";
const METHOD_NODE: &str = "shape=box, style=\"filled,rounded\", fillcolor=\"#eef2ff\", color=\"#a5b4fc\"";
const IPC_NODE: &str =
    "shape=box, style=\"filled,rounded\", fillcolor=\"#fde68a\", color=\"#d97706\", penwidth=2, label=\"Binder IPC\\n(remote process)\"";

fn header() -> String {
    String::from(
        "digraph binder {\n  \
         rankdir=TB; bgcolor=\"white\";\n  \
         graph [nodesep=0.35, ranksep=0.5];\n  \
         node [fontname=\"Helvetica\", fontsize=11];\n  \
         edge [fontname=\"Helvetica\", fontsize=9, color=\"#64748b\", arrowsize=0.8];\n",
    )
}

/// Outgoing finding: held locks → the call chain → the Binder boundary.
fn outgoing_dot(f: &OutgoingFinding) -> String {
    let mut s = header();
    let ids: Vec<String> = f.path.iter().map(|m| format!("m::{m}")).collect();
    for (m, id) in f.path.iter().zip(&ids) {
        let _ = writeln!(s, "  \"{}\" [{METHOD_NODE}, label=\"{}\"];", esc(id), esc(&short_method(m)));
    }
    let _ = writeln!(s, "  \"__ipc__\" [{IPC_NODE}];");
    for l in &f.held {
        let _ = writeln!(s, "  \"{}\" [{LOCK_NODE}, label=\"{}\"];", esc(l), esc(&short_lock(l)));
        if let Some(first) = ids.first() {
            let _ = writeln!(
                s,
                "  \"{}\" -> \"{}\" [label=\" held in\", color=\"#dc2626\", fontcolor=\"#dc2626\", penwidth=1.6];",
                esc(l), esc(first)
            );
        }
    }
    for w in ids.windows(2) {
        let _ = writeln!(s, "  \"{}\" -> \"{}\";", esc(&w[0]), esc(&w[1]));
    }
    if let Some(last) = ids.last() {
        let _ = writeln!(
            s,
            "  \"{}\" -> \"__ipc__\" [label=\" transacts\", color=\"#d97706\", fontcolor=\"#b45309\", penwidth=1.6];",
            esc(last)
        );
    }
    s.push_str("}\n");
    s
}

/// Incoming finding: the server entry → the call chain → each lock it acquires.
fn incoming_dot(an: &Analysis, f: &IncomingFinding) -> String {
    let mut s = header();
    let _ = writeln!(s, "  \"m::{}\" [{METHOD_NODE}, color=\"#6366f1\", penwidth=2, label=\"{}\"];", esc(&f.entry), esc(&short_method(&f.entry)));
    for lock in &f.locks {
        let path = an.paths.path_to(&f.entry, lock, 16).unwrap_or_else(|| vec![f.entry.clone()]);
        let ids: Vec<String> = path.iter().map(|m| format!("m::{m}")).collect();
        for (m, id) in path.iter().zip(&ids) {
            let _ = writeln!(s, "  \"{}\" [{METHOD_NODE}, label=\"{}\"];", esc(id), esc(&short_method(m)));
        }
        for w in ids.windows(2) {
            let _ = writeln!(s, "  \"{}\" -> \"{}\";", esc(&w[0]), esc(&w[1]));
        }
        let _ = writeln!(s, "  \"{}\" [{LOCK_NODE}, label=\"{}\"];", esc(lock), esc(&short_lock(lock)));
        if let Some(last) = ids.last() {
            let _ = writeln!(
                s,
                "  \"{}\" -> \"{}\" [label=\" acquires\", color=\"#dc2626\", fontcolor=\"#dc2626\", penwidth=1.6];",
                esc(last), esc(lock)
            );
        }
    }
    if f.high {
        let _ = writeln!(s, "  \"__ipc__\" [{IPC_NODE}];");
        let _ = writeln!(
            s,
            "  \"m::{}\" -> \"__ipc__\" [label=\" transacts while holding\", color=\"#d97706\", fontcolor=\"#b45309\", penwidth=1.6, style=dashed];",
            esc(&f.entry)
        );
    }
    s.push_str("}\n");
    s
}

/// Write `<name>.dot` and, if Graphviz is present, render `<name>.svg`. Returns the
/// best artifact name to reference from the report.
fn render(dir: &Path, name: &str, dot: &str) -> Option<String> {
    let dotp = dir.join(format!("{name}.dot"));
    std::fs::write(&dotp, dot).ok()?;
    if let Ok(o) = std::process::Command::new("dot").arg("-Tsvg").arg(&dotp).output() {
        if o.status.success() {
            std::fs::write(dir.join(format!("{name}.svg")), o.stdout).ok()?;
            return Some(format!("{name}.svg"));
        }
    }
    Some(format!("{name}.dot"))
}
