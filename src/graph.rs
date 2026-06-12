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

//! Lock-order graph, Tarjan SCC, and false-positive refinement.
//!
//! Two views of the same edges:
//!   * the *full* graph (every edge, incl. non-blocking) for export/visualization;
//!   * the *deadlock* graph (blocking edges only) for cycle detection.
//!
//! Candidate cycles are then refined by the guard/gate rule (a cycle whose
//! conflicting acquisitions share a common always-held outer lock is not a real
//! deadlock) — RacerD's key insight.

use crate::analyze::Edge;
use crate::model::Lock;
use std::collections::{HashMap, HashSet};

pub struct LockGraph {
    pub nodes: Vec<Lock>,
    pub index_of: HashMap<Lock, usize>,
    /// adjacency over ALL edges (full graph).
    pub adj: Vec<HashSet<usize>>,
    pub evidence: HashMap<(usize, usize), Vec<Edge>>,
}

impl LockGraph {
    pub fn build(edges: &[Edge], extra_nodes: &HashSet<Lock>) -> Self {
        let mut g = LockGraph {
            nodes: Vec::new(),
            index_of: HashMap::new(),
            adj: Vec::new(),
            evidence: HashMap::new(),
        };
        // Intern every node (edge endpoints + isolated/extra locks) in name-sorted
        // order *before* recording edges, so node indices — and every index-derived
        // artifact (JSON node list, DOT, sorted_evidence) — are reproducible
        // regardless of the parallel edge-collection order.
        let mut all: Vec<&Lock> =
            edges.iter().flat_map(|e| [&e.from, &e.to]).chain(extra_nodes.iter()).collect();
        all.sort_by_key(|l| l.name());
        for l in all {
            g.intern(l);
        }
        for e in edges {
            let a = g.index_of[&e.from];
            let b = g.index_of[&e.to];
            g.adj[a].insert(b);
            g.evidence.entry((a, b)).or_default().push(e.clone());
        }
        g
    }

    /// Evidence edges in a stable, sorted order (for reproducible exports).
    pub fn sorted_evidence(&self) -> Vec<(usize, usize, &Vec<Edge>)> {
        let mut v: Vec<(usize, usize, &Vec<Edge>)> =
            self.evidence.iter().map(|(&(a, b), ev)| (a, b, ev)).collect();
        v.sort_by_key(|&(a, b, _)| (a, b));
        v
    }

    fn intern(&mut self, l: &Lock) -> usize {
        if let Some(&i) = self.index_of.get(l) {
            return i;
        }
        let i = self.nodes.len();
        self.nodes.push(l.clone());
        self.index_of.insert(l.clone(), i);
        self.adj.push(HashSet::new());
        i
    }

    /// Does edge (a,b) have at least one *blocking* acquisition behind it?
    fn blocking(&self, a: usize, b: usize) -> bool {
        self.evidence
            .get(&(a, b))
            .map(|ev| ev.iter().any(|e| !e.nonblocking))
            .unwrap_or(false)
    }

    /// SCCs (>= 2 nodes) over the blocking subgraph — raw deadlock candidates.
    pub fn deadlock_sccs(&self) -> Vec<Vec<usize>> {
        let n = self.nodes.len();
        let mut index = vec![usize::MAX; n];
        let mut low = vec![0usize; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut counter = 0usize;
        let mut out: Vec<Vec<usize>> = Vec::new();

        for start in 0..n {
            if index[start] != usize::MAX {
                continue;
            }
            let mut work: Vec<(usize, Vec<usize>, usize)> = {
                let succ: Vec<usize> = self.adj[start]
                    .iter()
                    .copied()
                    .filter(|&w| self.blocking(start, w))
                    .collect();
                vec![(start, succ, 0)]
            };
            index[start] = counter;
            low[start] = counter;
            counter += 1;
            stack.push(start);
            on_stack[start] = true;

            while let Some((v, succ, ci)) = work.last_mut() {
                let v = *v;
                if *ci < succ.len() {
                    let w = succ[*ci];
                    *ci += 1;
                    if index[w] == usize::MAX {
                        index[w] = counter;
                        low[w] = counter;
                        counter += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        let wsucc: Vec<usize> = self.adj[w]
                            .iter()
                            .copied()
                            .filter(|&x| self.blocking(w, x))
                            .collect();
                        work.push((w, wsucc, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(index[w]);
                    }
                } else {
                    if low[v] == index[v] {
                        let mut comp = Vec::new();
                        loop {
                            let x = stack.pop().unwrap();
                            on_stack[x] = false;
                            comp.push(x);
                            if x == v {
                                break;
                            }
                        }
                        if comp.len() > 1 {
                            out.push(comp);
                        }
                    }
                    let vlow = low[v];
                    work.pop();
                    if let Some((p, _, _)) = work.last() {
                        low[*p] = low[*p].min(vlow);
                    }
                }
            }
        }
        out
    }

    /// The minimal simple cycles inside one SCC: for every blocking edge `(a, b)`
    /// of the component, the shortest blocking path `b -> a` (BFS, restricted to
    /// the component) closes the tightest cycle that uses that edge. Cycles are
    /// deduped by rotation-canonical form and returned smallest-first, so a large
    /// "tangle" SCC decomposes into its individual AB-BA inversions instead of an
    /// opaque member list. One BFS per edge keeps this polynomial — unlike full
    /// elementary-cycle enumeration (Johnson), which is exponential on dense SCCs.
    pub fn minimal_cycles(&self, comp: &[usize]) -> Vec<Vec<usize>> {
        let members: HashSet<usize> = comp.iter().copied().collect();
        // Per-node blocking successors inside the component, computed once and
        // sorted (indices are name-sorted at interning, so this is the
        // deterministic order for both the edge list and the BFS tie-break).
        let mut comp_sorted = comp.to_vec();
        comp_sorted.sort();
        let succ: HashMap<usize, Vec<usize>> = comp_sorted
            .iter()
            .map(|&a| {
                let mut s: Vec<usize> = self.adj[a]
                    .iter()
                    .copied()
                    .filter(|&b| members.contains(&b) && self.blocking(a, b))
                    .collect();
                s.sort();
                (a, s)
            })
            .collect();
        let edges: Vec<(usize, usize)> = comp_sorted
            .iter()
            .flat_map(|&a| succ[&a].iter().map(move |&b| (a, b)))
            .collect();
        let mut seen: HashSet<Vec<usize>> = HashSet::new();
        let mut out: Vec<Vec<usize>> = Vec::new();
        for &(a, b) in &edges {
            // BFS shortest path b -> a inside the component.
            use std::collections::VecDeque;
            let mut prev: HashMap<usize, usize> = HashMap::new();
            let mut q: VecDeque<usize> = VecDeque::new();
            q.push_back(b);
            prev.insert(b, b);
            let mut found = false;
            'bfs: while let Some(v) = q.pop_front() {
                for &w in &succ[&v] {
                    if w == a {
                        prev.insert(a, v);
                        found = true;
                        break 'bfs;
                    }
                    if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(w) {
                        e.insert(v);
                        q.push_back(w);
                    }
                }
            }
            if !found {
                continue;
            }
            // Cycle node sequence a -> b -> … -> (pred of a), each node once.
            let mut tail = Vec::new();
            let mut cur = a;
            while cur != b {
                cur = prev[&cur];
                tail.push(cur);
            }
            let mut cyc = vec![a];
            cyc.extend(tail.into_iter().rev());
            // Rotation-canonical form: the same cycle reached via different edges
            // (or a 2-cycle from both directions) dedups to one entry.
            let min_pos = cyc
                .iter()
                .enumerate()
                .min_by_key(|&(_, &n)| n)
                .map(|(i, _)| i)
                .unwrap_or(0);
            cyc.rotate_left(min_pos);
            if seen.insert(cyc.clone()) {
                out.push(cyc);
            }
        }
        // Smallest inversions first; tie-break on the node sequence for stability.
        out.sort_by(|x, y| x.len().cmp(&y.len()).then_with(|| x.cmp(y)));
        out
    }

    /// Guard refinement for one specific cycle (nodes in cycle order): the outer
    /// locks held on *every* blocking acquisition along its consecutive edges,
    /// excluding the cycle's own members. Same rule as [`common_guard`], applied
    /// to exactly the edges of this cycle — a single inversion inside a larger
    /// SCC can be individually gated even when the SCC as a whole is not.
    pub fn cycle_guard(&self, cyc: &[usize]) -> HashSet<String> {
        let members: HashSet<String> = cyc.iter().map(|&i| self.nodes[i].name()).collect();
        let keys = (0..cyc.len()).map(|k| (cyc[k], cyc[(k + 1) % cyc.len()]));
        self.guard_over(&members, keys)
    }

    /// Guard/gate refinement: a component is a *false* deadlock if every blocking
    /// edge inside it was taken while some common outer lock (not itself a member
    /// of the component) was held. Returns the set of common guard lock names, or
    /// empty if the cycle survives (a real candidate).
    pub fn common_guard(&self, comp: &[usize]) -> HashSet<String> {
        let members: HashSet<String> = comp.iter().map(|&i| self.nodes[i].name()).collect();
        let keys: Vec<(usize, usize)> = self
            .evidence
            .keys()
            .filter(|&&(a, b)| {
                members.contains(&self.nodes[a].name()) && members.contains(&self.nodes[b].name())
            })
            .copied()
            .collect();
        self.guard_over(&members, keys.into_iter())
    }

    /// The RacerD guard rule, shared by [`common_guard`] (all edges of an SCC)
    /// and [`cycle_guard`] (the consecutive edges of one cycle): intersect, over
    /// every blocking acquisition behind the given edges, the locks held that
    /// are not themselves members.
    fn guard_over(
        &self,
        members: &HashSet<String>,
        keys: impl Iterator<Item = (usize, usize)>,
    ) -> HashSet<String> {
        let mut common: Option<HashSet<String>> = None;
        for key in keys {
            let Some(ev) = self.evidence.get(&key) else { continue };
            for e in ev.iter().filter(|e| !e.nonblocking) {
                let g: HashSet<String> = e
                    .guard
                    .iter()
                    .map(|l| l.name())
                    .filter(|n| !members.contains(n))
                    .collect();
                common = Some(match common {
                    None => g,
                    Some(c) => c.intersection(&g).cloned().collect(),
                });
            }
        }
        common.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Root;

    fn lock(n: &str) -> Lock {
        Lock::new(Root::Static(n.to_string()))
    }

    fn edge(from: &str, to: &str, guard: &[&str]) -> Edge {
        Edge {
            from: lock(from),
            to: lock(to),
            method: "test.M.m:()V".to_string(),
            file: None,
            line: None,
            interproc: false,
            guard: guard.iter().map(|g| lock(g)).collect(),
            nonblocking: false,
        }
    }

    fn graph(edges: &[Edge]) -> LockGraph {
        LockGraph::build(edges, &HashSet::new())
    }

    fn names(g: &LockGraph, cyc: &[usize]) -> Vec<String> {
        cyc.iter().map(|&i| g.nodes[i].name()).collect()
    }

    /// One SCC expected; return it.
    fn only_scc(g: &LockGraph) -> Vec<usize> {
        let sccs = g.deadlock_sccs();
        assert_eq!(sccs.len(), 1, "expected one SCC, got {}", sccs.len());
        sccs.into_iter().next().unwrap()
    }

    #[test]
    fn ring_decomposes_to_single_cycle() {
        let e: Vec<Edge> = ["A", "B", "C", "D", "E"]
            .windows(2)
            .map(|w| edge(w[0], w[1], &[]))
            .chain([edge("E", "A", &[])])
            .collect();
        let g = graph(&e);
        let comp = only_scc(&g);
        let cycles = g.minimal_cycles(&comp);
        // every edge's shortest cycle is the whole ring — dedup leaves one.
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 5);
    }

    #[test]
    fn two_abba_pairs_in_one_scc_decompose_separately() {
        // A <-> B and B <-> C form one 3-node SCC with two distinct inversions.
        let e = [edge("A", "B", &[]), edge("B", "A", &[]), edge("B", "C", &[]), edge("C", "B", &[])];
        let g = graph(&e);
        let comp = only_scc(&g);
        let cycles = g.minimal_cycles(&comp);
        let got: Vec<Vec<String>> = cycles.iter().map(|c| names(&g, c)).collect();
        assert_eq!(got, vec![vec!["A", "B"], vec!["B", "C"]]);
    }

    #[test]
    fn chord_yields_tighter_cycle() {
        // 4-ring A->B->C->D->A plus chord B->D: the cycle through the chord is
        // the 3-cycle B->D->A(->B), tighter than the ring.
        let e = [
            edge("A", "B", &[]),
            edge("B", "C", &[]),
            edge("C", "D", &[]),
            edge("D", "A", &[]),
            edge("B", "D", &[]),
        ];
        let g = graph(&e);
        let comp = only_scc(&g);
        let cycles = g.minimal_cycles(&comp);
        let lens: Vec<usize> = cycles.iter().map(|c| c.len()).collect();
        assert_eq!(lens, vec![3, 4], "chord 3-cycle first, ring second: {cycles:?}");
        assert_eq!(names(&g, &cycles[0]), vec!["A", "B", "D"]);
    }

    #[test]
    fn every_blocking_edge_is_covered_by_some_cycle() {
        // No data hiding: each blocking edge of the SCC appears in >= 1 cycle.
        let e = [
            edge("A", "B", &[]),
            edge("B", "C", &[]),
            edge("C", "A", &[]),
            edge("B", "A", &[]),
            edge("C", "B", &[]),
        ];
        let g = graph(&e);
        let comp = only_scc(&g);
        let cycles = g.minimal_cycles(&comp);
        let mut covered: HashSet<(usize, usize)> = HashSet::new();
        for c in &cycles {
            for k in 0..c.len() {
                covered.insert((c[k], c[(k + 1) % c.len()]));
            }
        }
        for (&(a, b), ev) in &g.evidence {
            if ev.iter().any(|e| !e.nonblocking) {
                assert!(covered.contains(&(a, b)), "edge {a}->{b} not covered by any minimal cycle");
            }
        }
    }

    #[test]
    fn cycle_guard_detects_common_outer_lock() {
        // Both directions of A<->B taken under outer G: the inversion is gated.
        // B<->C has no common guard: it survives.
        let e = [
            edge("A", "B", &["G", "A", "B"]),
            edge("B", "A", &["G", "B", "A"]),
            edge("B", "C", &["B", "C"]),
            edge("C", "B", &["C", "B"]),
        ];
        let g = graph(&e);
        let comp = only_scc(&g);
        let cycles = g.minimal_cycles(&comp);
        let by_names: Vec<(Vec<String>, HashSet<String>)> =
            cycles.iter().map(|c| (names(&g, c), g.cycle_guard(c))).collect();
        let ab = by_names.iter().find(|(n, _)| n == &vec!["A", "B"]).expect("A<->B cycle");
        let bc = by_names.iter().find(|(n, _)| n == &vec!["B", "C"]).expect("B<->C cycle");
        assert_eq!(ab.1, HashSet::from(["G".to_string()]));
        assert!(bc.1.is_empty());
    }

    #[test]
    fn minimal_cycles_are_deterministic() {
        let e = [
            edge("A", "B", &[]),
            edge("B", "C", &[]),
            edge("C", "A", &[]),
            edge("B", "A", &[]),
            edge("C", "B", &[]),
            edge("A", "C", &[]),
        ];
        let g1 = graph(&e);
        let g2 = graph(&e);
        let c1: Vec<Vec<String>> =
            g1.minimal_cycles(&only_scc(&g1)).iter().map(|c| names(&g1, c)).collect();
        let c2: Vec<Vec<String>> =
            g2.minimal_cycles(&only_scc(&g2)).iter().map(|c| names(&g2, c)).collect();
        assert_eq!(c1, c2);
        assert!(!c1.is_empty());
    }
}
