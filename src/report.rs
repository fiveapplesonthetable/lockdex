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
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// An SCC larger than this is reported as a "lock tangle" summary rather than a
/// full per-edge dump — it reflects a globally interconnected lock hierarchy, not
/// a single actionable inversion. Shared so the report body, the cycles DOT, and
/// the stdout summary all classify cycles identically.
pub const TANGLE: usize = 12;

#[derive(Serialize)]
pub struct CycleReport {
    pub locks: Vec<String>,
    pub edges: Vec<EdgeJson>,
    /// The minimal simple cycles inside this SCC — the individual AB-BA
    /// inversions. For a small SCC this is usually a single entry; for a large
    /// "lock tangle" it is the actionable decomposition (the full member/edge
    /// lists above are still complete — nothing is dropped).
    pub inversions: Vec<Inversion>,
}

/// One minimal cycle (shortest cycle through some order edge) inside an SCC.
#[derive(Serialize)]
pub struct Inversion {
    /// lock names in cycle order: locks[i] is held while locks[i+1] is acquired
    /// (wrapping around).
    pub locks: Vec<String>,
    /// the consecutive order edges of the cycle.
    pub edges: Vec<EdgeJson>,
    /// non-empty: every acquisition along this cycle holds a common outer lock,
    /// so this particular inversion cannot interleave (kept for transparency).
    pub guard: Vec<String>,
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

fn edge_json(g: &LockGraph, a: usize, b: usize, ev: &[Edge]) -> EdgeJson {
    EdgeJson {
        from: g.nodes[a].name(),
        to: g.nodes[b].name(),
        count: ev.len(),
        interproc: ev.iter().all(|e| e.interproc),
        sample: sample(ev),
    }
}

/// Decompose one SCC into its minimal inversions, each with its consecutive
/// order edges and its own guard verdict. Ordered: unguarded (real candidates)
/// first, then guard-gated, smallest first within each group — the one
/// canonical order every consumer (JSON, text, verify) shares.
fn inversions_of(g: &LockGraph, comp: &[usize]) -> Vec<Inversion> {
    let mut out: Vec<Inversion> = g
        .minimal_cycles(comp)
        .into_iter()
        .map(|cyc| {
            let mut edges = Vec::new();
            for k in 0..cyc.len() {
                let (a, b) = (cyc[k], cyc[(k + 1) % cyc.len()]);
                if let Some(ev) = g.evidence.get(&(a, b)) {
                    edges.push(edge_json(g, a, b, ev));
                }
            }
            let mut guard: Vec<String> = g.cycle_guard(&cyc).into_iter().collect();
            guard.sort();
            Inversion { locks: cyc.iter().map(|&i| g.nodes[i].name()).collect(), edges, guard }
        })
        .collect();
    // stable: minimal_cycles is already smallest-first within each group.
    out.sort_by_key(|v| !v.guard.is_empty());
    out
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
                edges.push(edge_json(g, a, b, ev));
            }
        }
        edges.sort_by(|x, y| x.from.cmp(&y.from).then(x.to.cmp(&y.to)));
        let mut locks: Vec<String> = comp.iter().map(|&i| g.nodes[i].name()).collect();
        locks.sort();
        if guard.is_empty() {
            let inversions = inversions_of(g, comp);
            cycles.push(CycleReport { locks, edges, inversions });
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

    let edge_line = |e: &EdgeJson| -> String {
        let kind = if e.interproc { "interproc" } else { "nested" };
        let samp = e.sample.as_deref().unwrap_or(kind);
        format!("     {}  ->  {}   [{}x]  {}", e.from, e.to, e.count, samp)
    };
    for (i, c) in small.iter().enumerate() {
        let _ = writeln!(s, "=== DEADLOCK #{}: {} locks ===", i + 1, c.locks.len());
        for l in &c.locks {
            let _ = writeln!(s, "   {l}");
        }
        let _ = writeln!(s, "   conflicting order edges:");
        for e in &c.edges {
            let _ = writeln!(s, "{}", edge_line(e));
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
        let _ = writeln!(s, "   a globally interconnected lock hierarchy; all {} members:", c.locks.len());
        for l in &c.locks {
            let _ = writeln!(s, "     {l}");
        }
        // The tangle decomposed: the shortest cycle through each order edge,
        // unguarded candidates first (the order is canonical — `inversion N`
        // here is `inversions[N-1]` in lockgraph.json). Guard-gated inversions
        // are kept, marked — suppressing them silently is how deadlocks get
        // missed.
        let gated = c.inversions.iter().filter(|v| !v.guard.is_empty()).count();
        let _ = writeln!(
            s,
            "   {} minimal inversion(s) inside the tangle ({} candidate, {} gated by an outer guard):",
            c.inversions.len(),
            c.inversions.len() - gated,
            gated
        );
        for (j, v) in c.inversions.iter().enumerate() {
            let gate = if v.guard.is_empty() {
                String::new()
            } else {
                format!("  [gated by {}]", v.guard.join(", "))
            };
            let _ = writeln!(s, "   --- inversion {}: {} locks{} ---", j + 1, v.locks.len(), gate);
            for e in &v.edges {
                let _ = writeln!(s, "{}", edge_line(e));
            }
        }
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

/// A DOT of the deadlock cycles worth viewing. Small SCCs (the actionable AB-BA
/// ones) are drawn whole. Large strongly-connected "lock tangles" are near-dense
/// — Graphviz layout on every edge is pathologically slow and unreadable — so a
/// tangle is drawn as the union of its *minimal inversions* (the shortest cycle
/// through each order edge, guard-gated ones excluded): a sparse subgraph that
/// still shows every deadlock shape inside it. Tangle nodes are amber, small-SCC
/// nodes red.
pub fn dot_cycles(g: &LockGraph) -> String {
    let mut in_cycle: HashSet<usize> = HashSet::new();
    // node -> small-SCC id, so only edges *within one SCC* are drawn — an
    // ordered A->B edge that happens to join two separate cycles is not part of
    // any deadlock and would only suggest one.
    let mut scc_of: HashMap<usize, usize> = HashMap::new();
    let mut tangle_nodes: HashSet<usize> = HashSet::new();
    let mut tangle_edges: HashSet<(usize, usize)> = HashSet::new();
    for (ci, comp) in g.deadlock_sccs().into_iter().enumerate() {
        if g.common_guard(&comp).is_empty() {
            if comp.len() <= TANGLE {
                for &n in &comp {
                    scc_of.insert(n, ci);
                }
                in_cycle.extend(comp);
            } else {
                for cyc in g.minimal_cycles(&comp) {
                    if !g.cycle_guard(&cyc).is_empty() {
                        continue;
                    }
                    tangle_nodes.extend(cyc.iter().copied());
                    for k in 0..cyc.len() {
                        tangle_edges.insert((cyc[k], cyc[(k + 1) % cyc.len()]));
                    }
                }
            }
        }
    }
    let mut s = String::from("digraph cycles {\n  rankdir=LR; node [shape=box,fontsize=9,style=filled,fillcolor=\"#ffe0e0\"];\n");
    let mut cycle_nodes: Vec<usize> = in_cycle.iter().copied().collect();
    cycle_nodes.sort();
    for &i in &cycle_nodes {
        let _ = writeln!(s, "  \"{}\";", esc(&g.nodes[i].name()));
    }
    let mut tn: Vec<usize> = tangle_nodes.difference(&in_cycle).copied().collect();
    tn.sort();
    for &i in &tn {
        let _ = writeln!(s, "  \"{}\" [fillcolor=\"#ffedd5\"];", esc(&g.nodes[i].name()));
    }
    for (a, b, ev) in g.sorted_evidence() {
        let small = scc_of.get(&a).is_some_and(|ca| scc_of.get(&b) == Some(ca));
        if (small || tangle_edges.contains(&(a, b))) && ev.iter().any(|e| !e.nonblocking) {
            let _ = writeln!(s, "  \"{}\" -> \"{}\" [label=\"{}\"];", esc(&g.nodes[a].name()), esc(&g.nodes[b].name()), ev.len());
        }
    }
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lock, Root};

    fn lock(n: &str) -> Lock {
        Lock::new(Root::Static(n.to_string()))
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: lock(from),
            to: lock(to),
            method: "test.M.m:()V".to_string(),
            file: None,
            line: None,
            interproc: false,
            guard: vec![lock(from), lock(to)],
            nonblocking: false,
        }
    }

    /// 13-lock ring (above TANGLE) plus the tight L01->L00 reverse edge.
    fn tangle_graph() -> LockGraph {
        let names: Vec<String> = (0..13).map(|i| format!("L{i:02}")).collect();
        let mut edges: Vec<Edge> = (0..13)
            .map(|i| edge(&names[i], &names[(i + 1) % 13]))
            .collect();
        edges.push(edge("L01", "L00"));
        LockGraph::build(&edges, &std::collections::HashSet::new())
    }

    fn tangle_report() -> JsonReport {
        let g = tangle_graph();
        let an_edges = 14;
        // build_json needs only edge/method counts from Analysis; fabricate via
        // the pieces it reads. (Analysis itself is heavyweight to construct.)
        let sccs = g.deadlock_sccs();
        assert_eq!(sccs.len(), 1);
        let mut cycles = Vec::new();
        for comp in &sccs {
            let set: std::collections::HashSet<usize> = comp.iter().copied().collect();
            let mut edges = Vec::new();
            for (a, b, ev) in g.sorted_evidence() {
                if set.contains(&a) && set.contains(&b) {
                    edges.push(edge_json(&g, a, b, ev));
                }
            }
            let mut locks: Vec<String> = comp.iter().map(|&i| g.nodes[i].name()).collect();
            locks.sort();
            cycles.push(CycleReport { locks, edges, inversions: inversions_of(&g, comp) });
        }
        JsonReport {
            node_count: g.nodes.len(),
            edge_count: an_edges,
            method_count: 1,
            scc_count: cycles.len(),
            cycles,
            suppressed: Vec::new(),
            nodes: g.nodes.iter().map(|l| l.name()).collect(),
        }
    }

    #[test]
    fn tangle_text_lists_all_members_and_inversions() {
        let rep = tangle_report();
        assert_eq!(rep.cycles.len(), 1);
        let c = &rep.cycles[0];
        assert_eq!(c.locks.len(), 13);
        // decomposition: the tight 2-cycle and the 13-ring.
        assert_eq!(c.inversions.len(), 2);
        assert_eq!(c.inversions[0].locks, vec!["L00", "L01"]);
        assert_eq!(c.inversions[1].locks.len(), 13);

        let txt = text(&rep);
        assert!(txt.contains("LOCK TANGLE"), "13-lock SCC must render as a tangle:\n{txt}");
        // every member is listed — no elision.
        for i in 0..13 {
            assert!(txt.contains(&format!("L{i:02}")), "member L{i:02} missing:\n{txt}");
        }
        assert!(!txt.contains("more"), "tangle members must not be elided:\n{txt}");
        assert!(txt.contains("minimal inversion"), "inversion breakdown missing:\n{txt}");
        assert!(txt.contains("inversion 1: 2 locks"), "tight 2-cycle must be listed first:\n{txt}");
    }

    #[test]
    fn dot_cycles_includes_tangle_inversion_subgraph() {
        let g = tangle_graph();
        let dot = dot_cycles(&g);
        // tangle nodes are drawn (amber), and the tight inversion's both edges too.
        assert!(dot.contains("\"L00\" [fillcolor=\"#ffedd5\"]"), "tangle node missing:\n{dot}");
        assert!(dot.contains("\"L00\" -> \"L01\""), "inversion edge missing:\n{dot}");
        assert!(dot.contains("\"L01\" -> \"L00\""), "inversion edge missing:\n{dot}");
        // ring edges are on the 13-ring minimal cycle, so they are drawn as well.
        assert!(dot.contains("\"L05\" -> \"L06\""), "ring edge missing:\n{dot}");
    }
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
