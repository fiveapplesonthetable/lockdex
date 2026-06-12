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

//! `lockdex verify` — for each candidate cycle, pull the source at both edge
//! sites from a checkout, follow to where the *target* lock is actually acquired,
//! and print a per-cycle verdict. Turns "candidate" into "here is the code".
//!
//! A cycle `A ⇄ B` is a real deadlock when (1) some path takes A then B and
//! another takes B then A — both shown here from source; (2) A and B are distinct
//! objects; (3) the two sides can run on different threads. (1) and (2) are
//! checked mechanically; (3) still needs a human, but the sites make it quick.

use crate::report::{EdgeJson, JsonReport};
use crate::source::{acquire_sites, esc, print_ctx, rel, short_lock, short_method, Source};
use std::fmt::Write as _;
use std::path::Path;

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

/// One verification candidate: a small SCC, or one minimal inversion pulled out
/// of a large tangle (so tangles are verified piecewise instead of skipped).
struct Cand<'a> {
    locks: &'a [String],
    edges: &'a [EdgeJson],
    /// `Some(n)`: this is an inversion inside a tangle of `n` locks.
    tangle: Option<usize>,
}

/// The method graph for one deadlock: every call edge `(caller, heldLock, callee)`
/// along the paths of the cycle's order edges. Feeds the per-candidate pprof/hprof.
fn candidate_method_edges(c: &Cand, paths: &crate::analyze::PathIndex) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for e in c.edges {
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

/// One candidate cycle as a Graphviz DAG: lock nodes (red) joined by the actual
/// call path of each order edge (held → … → acquires). Top-to-bottom layout, so
/// the two paths form a readable loop between the shared lock nodes rather than a
/// wide horizontal strip.
fn cycle_dot(c: &Cand, paths: &crate::analyze::PathIndex) -> String {
    let mut s = String::from(
        "digraph cycle {\n  \
         rankdir=TB; bgcolor=\"white\";\n  \
         graph [nodesep=0.35, ranksep=0.5];\n  \
         node [fontname=\"Helvetica\", fontsize=11];\n  \
         edge [fontname=\"Helvetica\", fontsize=9, color=\"#64748b\", arrowsize=0.8];\n",
    );
    // Accumulate unique node decls and edges. 3-lock cycles often route two order
    // edges through the same call chain, which would otherwise draw the chain (and
    // the "held in" arrow) twice; dedup keeps each arrow once.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let emit = |line: String, s: &mut String, seen: &mut std::collections::HashSet<String>| {
        if seen.insert(line.clone()) {
            s.push_str(&line);
        }
    };
    for l in c.locks {
        emit(
            format!(
                "  \"{}\" [shape=box, style=\"filled,rounded\", fillcolor=\"#fee2e2\", color=\"#dc2626\", penwidth=2, fontsize=13, label=\"{}\"];\n",
                esc(l), esc(&short_lock(l))
            ),
            &mut s, &mut seen,
        );
    }
    for e in c.edges {
        let holder = e.sample.as_deref().and_then(parse_sample).map(|(mk, _, _)| mk);
        let path = holder.as_deref().and_then(|h| paths.path_to(h, &e.to, 16));
        match path {
            Some(p) if !p.is_empty() => {
                let ids: Vec<String> = p.iter().map(|m| format!("m::{m}")).collect();
                for (i, m) in p.iter().enumerate() {
                    emit(
                        format!(
                            "  \"{}\" [shape=box, style=\"filled,rounded\", fillcolor=\"#eef2ff\", color=\"#a5b4fc\", label=\"{}\"];\n",
                            esc(&ids[i]), esc(&short_method(m))
                        ),
                        &mut s, &mut seen,
                    );
                }
                emit(
                    format!(
                        "  \"{}\" -> \"{}\" [label=\" held in\", color=\"#dc2626\", fontcolor=\"#dc2626\", penwidth=1.6];\n",
                        esc(&e.from), esc(&ids[0])
                    ),
                    &mut s, &mut seen,
                );
                for w in ids.windows(2) {
                    emit(format!("  \"{}\" -> \"{}\";\n", esc(&w[0]), esc(&w[1])), &mut s, &mut seen);
                }
                emit(
                    format!(
                        "  \"{}\" -> \"{}\" [label=\" acquires\", color=\"#dc2626\", fontcolor=\"#dc2626\", penwidth=1.6];\n",
                        esc(ids.last().unwrap()), esc(&e.to)
                    ),
                    &mut s, &mut seen,
                );
            }
            _ => {
                emit(
                    format!(
                        "  \"{}\" -> \"{}\" [label=\" held → acquires [{}x]\", color=\"#dc2626\", fontcolor=\"#dc2626\", style=dashed, penwidth=1.6];\n",
                        esc(&e.from), esc(&e.to), e.count
                    ),
                    &mut s, &mut seen,
                );
            }
        }
    }
    s.push_str("}\n");
    s
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
    // Small SCCs verify whole; a tangle (SCC above `max_locks`) contributes each
    // of its small, unguarded minimal inversions as a separate candidate — a big
    // cluster is verified piecewise rather than skipped.
    let mut cands: Vec<Cand> = Vec::new();
    let (mut gated, mut oversize) = (0usize, 0usize);
    for c in &report.cycles {
        if c.locks.len() <= max_locks {
            cands.push(Cand { locks: &c.locks, edges: &c.edges, tangle: None });
        } else {
            for v in &c.inversions {
                if !v.guard.is_empty() {
                    gated += 1; // outer-lock gated: in report.txt marked [gated by …]
                } else if v.locks.len() > max_locks {
                    oversize += 1; // raise --max-locks to verify these too
                } else {
                    cands.push(Cand { locks: &v.locks, edges: &v.edges, tangle: Some(c.locks.len()) });
                }
            }
        }
    }
    let from_tangles = cands.iter().filter(|c| c.tangle.is_some()).count();
    let _ = writeln!(
        out,
        "lockdex verify — {} candidate(s) with <= {} locks ({} small cycles, {} inversions from larger tangles; {} SCC(s) total)\n\
         not verified here: {} guard-gated inversion(s) (see report.txt), {} inversion(s) over --max-locks\n\
         source: {}\n",
        cands.len(),
        max_locks,
        cands.len() - from_tangles,
        from_tangles,
        report.cycles.len(),
        gated,
        oversize,
        root.display()
    );

    for (ci, c) in cands.iter().enumerate() {
        let tag = match c.tangle {
            Some(n) => format!(" (inversion inside a {n}-lock tangle)"),
            None => String::new(),
        };
        let _ = writeln!(out, "================ CANDIDATE {} : {} locks{} ================", ci + 1, c.locks.len(), tag);
        for l in c.locks {
            let _ = writeln!(out, "   lock  {l}");
        }
        let mut edges_ok = 0;
        for e in c.edges {
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
                        // Show the synchronized site in the final (acquiring)
                        // method's own class — which is authoritative — so we
                        // ignore the lock's nominal target class here.
                        if let (Some(last), Some((_tclass, field))) =
                            (p.last(), lock_target(&e.to))
                        {
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
