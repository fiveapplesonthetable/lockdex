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

//! Human-readable + JSON + DOT reporting, with guard refinement applied.

use crate::analyze::{Analysis, Edge};
use crate::graph::LockGraph;
use crate::source::esc;
use serde::Serialize;
use std::collections::HashSet;
use std::fmt::Write as _;

/// An SCC larger than this is reported as a "lock tangle" summary rather than a
/// full per-edge dump — it reflects a globally interconnected lock hierarchy, not
/// a single actionable inversion. Shared so the report body, the cycles DOT, and
/// the stdout summary all classify cycles identically.
pub const TANGLE: usize = 12;

/// How many members of a large tangle to print before eliding the rest.
const TANGLE_SAMPLE: usize = 12;

#[derive(Serialize)]
pub struct CycleReport {
    pub locks: Vec<String>,
    pub edges: Vec<EdgeJson>,
}

#[derive(Serialize)]
pub struct EdgeJson {
    pub from: String,
    pub to: String,
    pub count: usize,
    pub interproc: bool,
    pub sample: Option<String>,
}

#[derive(Serialize)]
pub struct SuppressedReport {
    pub locks: Vec<String>,
    pub guard: Vec<String>,
}

#[derive(Serialize)]
pub struct JsonReport {
    pub node_count: usize,
    pub edge_count: usize,
    pub method_count: usize,
    pub scc_count: usize,
    pub cycles: Vec<CycleReport>,
    /// candidate cycles dropped by guard refinement (kept for transparency).
    pub suppressed: Vec<SuppressedReport>,
    pub nodes: Vec<String>,
}

fn sample(edges: &[Edge]) -> Option<String> {
    edges.iter().find_map(|e| {
        let line = e.line?;
        let file = e.file.clone().unwrap_or_else(|| e.method.clone());
        Some(format!("{file}:{line}{}", if e.interproc { " (interproc)" } else { "" }))
    })
}

pub fn build_json(an: &Analysis, g: &LockGraph) -> JsonReport {
    let sccs = g.deadlock_sccs();
    let mut cycles = Vec::new();
    let mut suppressed = Vec::new();

    for comp in &sccs {
        let guard = g.common_guard(comp);
        let set: HashSet<usize> = comp.iter().copied().collect();
        let mut edges = Vec::new();
        for (a, b, ev) in g.sorted_evidence() {
            if set.contains(&a) && set.contains(&b) && ev.iter().any(|e| !e.nonblocking) {
                edges.push(EdgeJson {
                    from: g.nodes[a].name(),
                    to: g.nodes[b].name(),
                    count: ev.len(),
                    interproc: ev.iter().all(|e| e.interproc),
                    sample: sample(ev),
                });
            }
        }
        edges.sort_by(|x, y| x.from.cmp(&y.from).then(x.to.cmp(&y.to)));
        let mut locks: Vec<String> = comp.iter().map(|&i| g.nodes[i].name()).collect();
        locks.sort();
        if guard.is_empty() {
            cycles.push(CycleReport { locks, edges });
        } else {
            let mut gv: Vec<String> = guard.into_iter().collect();
            gv.sort();
            suppressed.push(SuppressedReport { locks, guard: gv });
        }
    }
    // smallest cycles first: a 2–3 lock inversion is the actionable finding; the
    // large strongly-connected tangles go last. Break ties on the (already sorted)
    // lock list so equal-size cycles have a stable, reproducible order.
    cycles.sort_by(|a, b| a.locks.len().cmp(&b.locks.len()).then_with(|| a.locks.cmp(&b.locks)));
    suppressed.sort_by(|a, b| a.locks.cmp(&b.locks));

    JsonReport {
        node_count: g.nodes.len(),
        edge_count: an.edges.len(),
        method_count: an.method_count,
        scc_count: cycles.len(),
        cycles,
        suppressed,
        nodes: g.nodes.iter().map(|l| l.name()).collect(),
    }
}

pub fn text(rep: &JsonReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "lockdex — static lock-order / deadlock report");
    let _ = writeln!(
        s,
        "{} methods, {} lock nodes, {} order edges, {} deadlock cycles ({} suppressed by guard)\n",
        rep.method_count,
        rep.node_count,
        rep.edge_count,
        rep.cycles.len(),
        rep.suppressed.len()
    );
    if rep.cycles.is_empty() {
        let _ = writeln!(s, "No lock-order deadlock cycles found.");
    }
    let small: Vec<&CycleReport> = rep.cycles.iter().filter(|c| c.locks.len() <= TANGLE).collect();
    let large: Vec<&CycleReport> = rep.cycles.iter().filter(|c| c.locks.len() > TANGLE).collect();

    for (i, c) in small.iter().enumerate() {
        let _ = writeln!(s, "=== DEADLOCK #{}: {} locks ===", i + 1, c.locks.len());
        for l in &c.locks {
            let _ = writeln!(s, "   {l}");
        }
        let _ = writeln!(s, "   conflicting order edges:");
        for e in &c.edges {
            let kind = if e.interproc { "interproc" } else { "nested" };
            let samp = e.sample.clone().unwrap_or_else(|| kind.to_string());
            let _ = writeln!(s, "     {}  ->  {}   [{}x]  {}", e.from, e.to, e.count, samp);
        }
        let _ = writeln!(s);
    }
    for (i, c) in large.iter().enumerate() {
        let _ = writeln!(
            s,
            "=== LOCK TANGLE #{}: {} mutually-out-of-order locks (large SCC) ===",
            i + 1,
            c.locks.len()
        );
        let _ = writeln!(s, "   a globally interconnected lock hierarchy; sample members:");
        for l in c.locks.iter().take(TANGLE_SAMPLE) {
            let _ = writeln!(s, "     {l}");
        }
        let _ = writeln!(s, "     … and {} more", c.locks.len().saturating_sub(TANGLE_SAMPLE));
        let _ = writeln!(s);
    }
    if !rep.suppressed.is_empty() {
        let _ = writeln!(s, "--- {} candidate(s) suppressed by a common guard lock ---", rep.suppressed.len());
        for sp in &rep.suppressed {
            let _ = writeln!(s, "   {{{}}}  guarded by  {}", sp.locks.join(", "), sp.guard.join(", "));
        }
    }
    s
}

/// A small DOT of only the *small* deadlock cycles (the actionable AB-BA ones).
/// Large strongly-connected "lock tangles" are excluded: they are near-dense, so
/// Graphviz layout on them is pathologically slow and the picture is unreadable
/// anyway. This is the part worth viewing.
pub fn dot_cycles(g: &LockGraph) -> String {
    let mut in_cycle: HashSet<usize> = HashSet::new();
    for comp in g.deadlock_sccs() {
        if comp.len() <= TANGLE && g.common_guard(&comp).is_empty() {
            in_cycle.extend(comp);
        }
    }
    let mut s = String::from("digraph cycles {\n  rankdir=LR; node [shape=box,fontsize=9,style=filled,fillcolor=\"#ffe0e0\"];\n");
    let mut cycle_nodes: Vec<usize> = in_cycle.iter().copied().collect();
    cycle_nodes.sort();
    for &i in &cycle_nodes {
        let _ = writeln!(s, "  \"{}\";", esc(&g.nodes[i].name()));
    }
    for (a, b, ev) in g.sorted_evidence() {
        if in_cycle.contains(&a) && in_cycle.contains(&b) && ev.iter().any(|e| !e.nonblocking) {
            let _ = writeln!(s, "  \"{}\" -> \"{}\" [label=\"{}\"];", esc(&g.nodes[a].name()), esc(&g.nodes[b].name()), ev.len());
        }
    }
    s.push_str("}\n");
    s
}

pub fn dot(g: &LockGraph) -> String {
    let mut in_cycle: HashSet<usize> = HashSet::new();
    for comp in g.deadlock_sccs() {
        if g.common_guard(&comp).is_empty() {
            in_cycle.extend(comp);
        }
    }
    let mut s = String::from("digraph locks {\n  rankdir=LR; node [shape=box,fontsize=9];\n");
    let mut cycle_nodes: Vec<usize> = in_cycle.iter().copied().collect();
    cycle_nodes.sort();
    for &i in &cycle_nodes {
        let _ = writeln!(
            s,
            "  \"{}\" [color=red,style=filled,fillcolor=\"#ffe0e0\"];",
            esc(&g.nodes[i].name())
        );
    }
    for (a, b, ev) in g.sorted_evidence() {
        let col = if in_cycle.contains(&a) && in_cycle.contains(&b) {
            ",color=red"
        } else {
            ""
        };
        let style = if ev.iter().all(|e| e.nonblocking) { ",style=dashed" } else { "" };
        let _ = writeln!(
            s,
            "  \"{}\" -> \"{}\" [label=\"{}\"{}{}];",
            esc(&g.nodes[a].name()),
            esc(&g.nodes[b].name()),
            ev.len(),
            col,
            style
        );
    }
    s.push_str("}\n");
    s
}
