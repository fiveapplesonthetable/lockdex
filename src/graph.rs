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
        for e in edges {
            let a = g.intern(&e.from);
            let b = g.intern(&e.to);
            g.adj[a].insert(b);
            g.evidence.entry((a, b)).or_default().push(e.clone());
        }
        // include isolated locks (e.g. opaque / singly-held) so exports are
        // complete; sorted so the node order — and every artifact — is reproducible.
        let mut extra: Vec<&Lock> = extra_nodes.iter().collect();
        extra.sort_by_key(|l| l.name());
        for l in extra {
            g.intern(l);
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

    /// Guard/gate refinement: a component is a *false* deadlock if every blocking
    /// edge inside it was taken while some common outer lock (not itself a member
    /// of the component) was held. Returns the set of common guard lock names, or
    /// empty if the cycle survives (a real candidate).
    pub fn common_guard(&self, comp: &[usize]) -> HashSet<String> {
        let members: HashSet<String> = comp.iter().map(|&i| self.nodes[i].name()).collect();
        let mut common: Option<HashSet<String>> = None;
        for (&(a, b), ev) in &self.evidence {
            if !members.contains(&self.nodes[a].name()) || !members.contains(&self.nodes[b].name()) {
                continue;
            }
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
