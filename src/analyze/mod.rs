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

//! Compositional lock-order analysis.
//!
//! Per-method extraction (parallel) records *raw* facts; a global phase builds an
//! RTA call graph, resolves lambda captures, runs the mayAcquire fixpoint, and
//! assembles the lock-order edges. Lock identity is an access path
//! (receiver-sensitive); virtual/interface dispatch is resolved by CHA pruned to
//! instantiated types (RTA) plus receiver-type refinement; lambda capture fields
//! are resolved to the values passed to the synthetic constructor.

use crate::juc;
use crate::model::*;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

mod binder;
mod callgraph;
mod extract;
mod fixpoint;
mod races;
pub use binder::{BinderReport, IncomingFinding, OutgoingFinding};
pub use races::{FieldRace, RaceReport, Violation};
use callgraph::{build_supertypes, index_namesig, CallGraph, POLY_LIMIT};
use fixpoint::may_acquire;

/// A call-graph edge for export: (caller key, callee key, lock held across the call).
type MethodEdge = (String, String, String);
/// One method's contribution to the graph: lock-order edges, call edges, locks touched.
type MethodParts = (Vec<Edge>, Vec<MethodEdge>, Vec<Lock>);

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: Lock,
    pub to: Lock,
    pub method: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub interproc: bool,
    pub guard: Vec<Lock>,
    pub nonblocking: bool,
}

#[derive(Debug, Clone)]
struct RawCall {
    kind: InvokeKind,
    dclass: String,
    name: String,
    sig: String,
    /// concrete receiver type if known (from an allocation), for dispatch refinement.
    recv_type: Option<String>,
    args: Vec<Option<Lock>>,
    held: Vec<Lock>,
    line: Option<u32>,
    is_async: bool,
}

impl RawCall {
    fn namesig(&self) -> String {
        format!("{}:{}", self.name, self.sig)
    }
    fn declared_key(&self) -> String {
        format!("{}.{}:{}", self.dclass, self.name, self.sig)
    }
}

/// One read or write of an instance field, with the locks held at that point
/// (relative to the enclosing method). Feeds the field-race analysis.
#[derive(Debug, Clone)]
struct FieldAccess {
    /// `DeclaringClass.field` — used for the `final`/`volatile` exclusion.
    field: String,
    write: bool,
    line: Option<u32>,
    held: Vec<Lock>,
    /// the access path of the base object when it is a stable named field
    /// (`Foo.mInstaller`), else `None`. Distinguishes one guarded instance of a shared
    /// data class from the many other instances accessed lock-free elsewhere, so a
    /// single guarded write doesn't project its guard onto every `ResolveInfo.field`.
    inst: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Summary {
    key: String,
    class: String,
    /// public or protected: callable from outside the analyzed component, so it has
    /// unknown callers — a root for the must-hold-on-entry analysis.
    external: bool,
    intra_edges: Vec<Edge>,
    first_acquire: Vec<Lock>,
    acquires: Vec<Lock>,
    /// every `monitor-enter` in this method: (grounded lock, source line).
    /// Unlike `intra_edges`, this keeps top-level acquisitions (nothing held
    /// yet) too — the common shape a contention site lands on.
    acq_sites: Vec<(Lock, Option<u32>)>,
    calls: Vec<RawCall>,
    field_access: Vec<FieldAccess>,
    value_summary: Option<Lock>,
    /// allocation sites created here: site -> type.
    allocs: Vec<(String, String)>,
    /// `field = new T(...)`: a freshly allocated object stored into a field
    /// (`field key`, type). A type allocated once and stored once is a singleton,
    /// whose `this`-monitor is the same lock as `owner.field`.
    alloc_stores: Vec<(String, String)>,
    /// `new T(...)` followed by `<init>`: (site, ctor_key, arg values).
    alloc_inits: Vec<(String, String, Vec<Option<Lock>>)>,
    /// if this method is a `<init>`, captured fields: field -> formal it stores.
    ctor_captures: Vec<(String, u32)>,
    /// if this `<init>` chains to another (`super(...)` / `this(...)`): the callee
    /// `<init>` key and, per callee formal, which of *this* ctor's formals is passed
    /// (so a field the super ctor captures from formal i is, here, captured from
    /// whatever this ctor forwards into i). Threads injected locks down a class
    /// hierarchy.
    super_init: Option<(String, Vec<Option<u32>>)>,
    /// lock-field aliases learned from a `<init>`: `Class.field` is assigned a
    /// lock that lives elsewhere (`this.mLock = service.getLock()`), so the two
    /// name one object. Used to collapse a singleton lock split across fields.
    field_aliases: Vec<(String, Lock)>,
    /// `this.field = formal` in ANY method (declaring-class field key so it
    /// threads through `super`, formal index with receiver `this` = 0). Makes the
    /// field an alias of that formal, to be resolved interprocedurally.
    param_stores: Vec<(String, u32)>,
    /// resolved actual arguments of every invoke that passes an object:
    /// (callee key, actuals). Actual index lines up with the callee's formal
    /// index (receiver at 0), so it binds a call site's args to the callee's
    /// formals for parameter propagation.
    arg_bindings: Vec<(String, Vec<Option<Lock>>)>,
}

pub struct Analysis {
    pub edges: Vec<Edge>,
    pub all_locks: HashSet<Lock>,
    pub method_count: usize,
    /// method dependency graph: (caller method, lock held at the call, callee).
    /// Powers the Perfetto/pprof method-graph view (a call made while holding L).
    pub method_edges: Vec<(String, String, String)>,
    /// call graph + reachability, for `verify` to show the path of an edge.
    pub paths: PathIndex,
    /// locks held across Binder IPC boundaries (a cross-process hazard, distinct
    /// from same-process deadlock cycles).
    pub binder: BinderReport,
    /// fields whose inferred guard lock is applied inconsistently, with the
    /// accesses that violate it.
    pub races: RaceReport,
    /// every monitor-enter site, canonically named. Powers `lockdex resolve`:
    /// map a contention FILE:LINE to the lock actually taken there.
    pub acquisitions: Vec<Acquisition>,
}

/// A resolved `synchronized` / `monitor-enter` site: the canonical lock taken,
/// the holder class, and the source line. `class` may be a nested class
/// (`Outer$Inner`); the top-level class fixes the source file.
pub struct Acquisition {
    pub class: String,
    /// holder method key — the stable anchor when line numbers drift.
    pub method: String,
    pub line: Option<u32>,
    pub lock: String,
}

/// If `l` is a directly-locked object parameter, name it by its declared class
/// (the same instance-monitor rendering `this` gets). Returns None for non-param
/// or primitive-typed params, so the caller falls back to normal grounding.
fn param_type_name(l: &Lock, m: Option<&Method>) -> Option<String> {
    let crate::model::Root::Param(j) = l.root else { return None };
    if !l.fields.is_empty() {
        return None;
    }
    let m = m?;
    let reg = m.registers.saturating_sub(m.ins) + j;
    m.object_param_regs().into_iter().find(|(r, _)| *r == reg).map(|(_, t)| t)
}

/// Enough of the call graph to reconstruct, for an order edge `A -> B`, the
/// shortest call chain from the method holding `A` to the method that acquires
/// `B`. Built from the resolved (RTA) call graph and the mayAcquire fixpoint.
pub struct PathIndex {
    /// method key -> its (non-async) resolved callee method keys.
    callees: HashMap<String, Vec<String>>,
    /// method key -> locks it may (transitively) acquire.
    may: HashMap<String, Vec<Lock>>,
    /// method key -> lock names it acquires *directly*.
    direct: HashMap<String, HashSet<String>>,
}

impl PathIndex {
    /// Shortest call chain `[holder, …, acquirer]` such that the last method
    /// directly acquires `target`, walking only callees that can reach it.
    pub fn path_to(&self, start: &str, target: &str, max_depth: usize) -> Option<Vec<String>> {
        if self.direct.get(start).map(|s| s.contains(target)).unwrap_or(false) {
            return Some(vec![start.to_string()]);
        }
        use std::collections::VecDeque;
        let mut q: VecDeque<(String, usize)> = VecDeque::new();
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        q.push_back((start.to_string(), 0));
        seen.insert(start.to_string());
        while let Some((m, d)) = q.pop_front() {
            if d >= max_depth {
                continue;
            }
            let Some(cs) = self.callees.get(&m) else { continue };
            for c in cs {
                if seen.contains(c) {
                    continue;
                }
                if self.direct.get(c).map(|s| s.contains(target)).unwrap_or(false) {
                    prev.insert(c.clone(), m.clone());
                    let mut path = vec![c.clone()];
                    let mut cur = c.clone();
                    while let Some(p) = prev.get(&cur) {
                        path.push(p.clone());
                        cur = p.clone();
                    }
                    path.reverse();
                    return Some(path);
                }
                if self.may.get(c).map(|v| v.iter().any(|l| l.name() == target)).unwrap_or(false) {
                    seen.insert(c.clone());
                    prev.insert(c.clone(), m.clone());
                    q.push_back((c.clone(), d + 1));
                }
            }
        }
        None
    }
}

pub fn analyze(dex: &Dex, cfg: &juc::AsyncConfig) -> Analysis {
    let t = Instant::now();
    let methods: Vec<&Method> = dex.classes.iter().flat_map(|c| c.methods.iter()).collect();
    let supertypes = build_supertypes(dex);

    // --- per-method summaries (parallel: value summaries, then full) ---------
    let empty: HashMap<String, Lock> = HashMap::new();
    let value_summaries: HashMap<String, Lock> = methods
        .par_iter()
        .filter_map(|m| extract::extract(m, &empty, cfg).value_summary.map(|v| (m.key(), v)))
        .collect();
    let summaries: Vec<Summary> = methods.par_iter().map(|m| extract::extract(m, &value_summaries, cfg)).collect();
    let mut by_key: HashMap<String, Summary> = HashMap::new();
    for s in summaries {
        by_key.entry(s.key.clone()).or_insert(s);
    }
    let ncalls: usize = by_key.values().map(|s| s.calls.len()).sum();
    eprintln!(
        "[lockdex] summarized {} methods, {} call sites, {} getters in {:.1}s",
        by_key.len(), ncalls, value_summaries.len(), t.elapsed().as_secs_f64()
    );

    // --- hierarchy-aware async dispatch ---------------------------------------
    // Extraction classifies dispatch by *name* only (`Handler.post`, `*Executor.
    // execute`); here the dex type hierarchy is known, so a call on any subtype of
    // a dispatcher base — a custom Handler, an Executor implementation with an
    // arbitrary name — is reclassified as async too. The posted runnable does not
    // run under the caller's locks, so every downstream consumer (order edges,
    // must-hold guard credit, binder reach, verify paths) must sever there.
    let nasync = mark_async_by_hierarchy(&mut by_key, &supertypes, cfg);
    eprintln!("[lockdex] {nasync} call site(s) reclassified as async dispatch via the type hierarchy");

    // --- global indices + call graph (parallel resolution) -------------------
    let methods_by_namesig = index_namesig(&by_key);
    let instantiated: HashSet<String> =
        by_key.values().flat_map(|s| s.allocs.iter().map(|(_, t)| t.clone())).collect();
    let ctor_captures = index_ctor_captures(&by_key);
    let capture_map = build_capture_map(&by_key, &ctor_captures);
    // Interprocedural injection resolver (parameter/copy propagation over the call
    // graph). Seed the fixpoint with the fields it must resolve AND every
    // `synchronized(param)` operand, solve once, and reuse the solution for both
    // the alias map (b) below and for naming parameter-locks in the acquisitions.
    let injector = Injector::build(&by_key);
    let inj_solution = {
        let mut seeds: Vec<(&str, u32)> =
            injector.field_stores.values().flatten().copied().collect();
        for s in by_key.values() {
            for (l, _) in &s.acq_sites {
                if let Root::Param(j) = l.root {
                    seeds.push((s.key.as_str(), j));
                }
            }
        }
        injector.solve(&seeds)
    };
    // lock-field aliases: `Class.field` -> the shared lock it actually names,
    // learned from how the field is assigned. Two sources:
    //   (a) direct, in the field's `<init>`: `this.f = service.getLock()` / another
    //       field / a static (recorded as `field_aliases`);
    //   (b) constructor parameter: `this.f = param_i`, resolved to the actual
    //       argument across all construction sites.
    // A field assigned different objects at different sites is left distinct
    // (sound). This collapses a singleton lock handed to several classes.
    let alias: HashMap<String, Lock> = {
        let mut seen: HashMap<String, Option<Lock>> = HashMap::new();
        let note = |key: String, v: Option<Lock>, seen: &mut HashMap<String, Option<Lock>>| {
            match (seen.get(&key), &v) {
                (None, _) => { seen.insert(key, v); }
                (Some(None), _) => {}                          // already ambiguous
                (Some(Some(e)), Some(nv)) if e == nv => {}     // consistent
                _ => { seen.insert(key, None); }               // conflict -> distinct
            }
        };
        // (a) direct assignments
        for s in by_key.values() {
            for (k, v) in &s.field_aliases {
                note(k.clone(), Some(v.clone()), &mut seen);
            }
        }
        // (b) interprocedural injection: a field stored from a formal
        // (`this.f = param_i`, in *any* method) aliases the object bound to that
        // formal. Resolve it by parameter/copy propagation over all call sites
        // (`Injector`). Constructors, setters and `super(...)` are just call sites,
        // so ctor/setter/inheritance injection all fall out of one algorithm.
        for (field, v) in injector.resolve_fields(&inj_solution) {
            note(field, Some(v), &mut seen);
        }
        // (c) singleton self-monitor: a class allocated exactly once and stored in a
        // single field is `owner.field` from outside and `this` from inside. Unify
        // them so a field guarded by `synchronized(this)` and by
        // `synchronized(owner.field)` is recognised as one lock. Restricted to
        // classes that actually synchronize on `this` — otherwise a plain
        // `new Object()` lock would be misread as its (meaningless) type identity.
        let self_sync: HashSet<&str> = by_key
            .values()
            .filter(|s| s.acquires.iter().any(|l| matches!(l.root, Root::This)))
            .map(|s| s.class.as_str())
            .collect();
        let mut stored_in: HashMap<&str, HashSet<&str>> = HashMap::new();
        for s in by_key.values() {
            for (field, ty) in &s.alloc_stores {
                stored_in.entry(ty.as_str()).or_default().insert(field.as_str());
            }
        }
        // A self-synchronizing class held in exactly one field across the component
        // is that field's singleton — its `this`-monitor and `owner.field` are one.
        for (ty, fields) in &stored_in {
            if self_sync.contains(ty) && fields.len() == 1 {
                let field = (*fields.iter().next().expect("len == 1")).to_string();
                if !seen.contains_key(&field) {
                    note(field, Some(Lock::new(Root::Recv((*ty).to_string()))), &mut seen);
                }
            }
        }
        seen.into_iter().filter_map(|(k, v)| v.map(|l| (k, l))).collect()
    };
    eprintln!("[lockdex] {} lock-field aliases resolved", alias.len());
    let cg = CallGraph { methods_by_namesig, instantiated, supertypes };

    let tcg = Instant::now();
    let resolved: HashMap<String, Vec<Vec<String>>> = by_key
        .par_iter()
        .map(|(k, s)| (k.clone(), s.calls.iter().map(|c| cg.resolve(c, &by_key)).collect()))
        .collect();
    eprintln!(
        "[lockdex] resolved call graph ({} instantiated types, poly<= {}) in {:.1}s",
        cg.instantiated.len(), POLY_LIMIT, tcg.elapsed().as_secs_f64()
    );

    // --- lock-propagation fixpoint (parallel per round) ----------------------
    let tfp = Instant::now();
    let (may, iters) = may_acquire(&by_key, &resolved);
    eprintln!("[lockdex] lock-propagation fixpoint: {} rounds in {:.1}s", iters, tfp.elapsed().as_secs_f64());

    // Locks guaranteed held on entry to each method (a meet over its callers). Two
    // variants: the deadlock view treats externally-callable methods as roots (sound
    // reentrancy suppression — never hide a real edge); the race view credits the
    // meet over the callers we can see (precise guard reconstruction — a helper whose
    // every caller holds L is guarded by L, no naming convention needed).
    let must = races::must_entry(&by_key, &resolved, &alias, true);
    let must_races = races::must_entry(&by_key, &resolved, &alias, false);

    // --- edge assembly (parallel per method) ---------------------------------
    let tea = Instant::now();
    let mut asm_keys: Vec<&String> = by_key.keys().collect();
    asm_keys.sort();
    let parts: Vec<MethodParts> = asm_keys
        .par_iter()
        .map(|k| assemble_one(k, &by_key[*k], &resolved, &may, &capture_map, &alias, &must))
        .collect();
    let mut edges: Vec<Edge> = Vec::new();
    let mut method_edges: Vec<MethodEdge> = Vec::new();
    let mut all_locks: HashSet<Lock> = HashSet::new();
    for (e, me, locks) in parts {
        edges.extend(e);
        method_edges.extend(me);
        all_locks.extend(locks);
    }
    // total order so the method-graph exports (hprof / per-cycle pprof) are
    // reproducible; within a method the edge order is otherwise unspecified.
    method_edges.sort();
    eprintln!(
        "[lockdex] assembled {} order edges over {} locks in {:.1}s",
        edges.len(), all_locks.len(), tea.elapsed().as_secs_f64()
    );

    // --- path index (call graph + direct/transitive acquires) for `verify` ---
    let mut callees: HashMap<String, Vec<String>> = HashMap::new();
    let mut direct: HashMap<String, HashSet<String>> = HashMap::new();
    for (k, s) in &by_key {
        let mut cs: Vec<String> = Vec::new();
        if let Some(rv) = resolved.get(k) {
            for (ci, call) in s.calls.iter().enumerate() {
                if call.is_async {
                    continue;
                }
                if let Some(c) = rv.get(ci) {
                    for x in c {
                        if !cs.contains(x) {
                            cs.push(x.clone());
                        }
                    }
                }
            }
        }
        callees.insert(k.clone(), cs);
        let d: HashSet<String> = s
            .acquires
            .iter()
            .filter(|l| !l.is_opaque())
            .map(|l| canonicalize(&ground(l, &s.class, k), &alias).name())
            .collect();
        direct.insert(k.clone(), d);
    }
    let may_canon: HashMap<String, Vec<Lock>> = may
        .into_iter()
        .map(|(k, v)| (k, v.iter().map(|l| canonicalize(l, &alias)).collect()))
        .collect();
    let paths = PathIndex { callees, may: may_canon, direct };

    // --- binder IPC boundaries (locks across a cross-process call) ------------
    let tb = Instant::now();
    let binder = binder::compute(dex, &cg.supertypes, &by_key, &resolved, &paths, &alias);
    eprintln!(
        "[lockdex] binder boundaries: {} outgoing hold-sites, {} incoming entries in {:.1}s",
        binder.outgoing.len(), binder.incoming.len(), tb.elapsed().as_secs_f64()
    );

    // --- field-race detection (guard reconstruction) -------------------------
    let tr = Instant::now();
    let races = races::compute(&by_key, &alias, &must_races, &dex.final_or_volatile_fields);
    eprintln!(
        "[lockdex] field races: {} inconsistently-guarded field(s) in {:.1}s",
        races.fields.len(), tr.elapsed().as_secs_f64()
    );

    // Canonicalize every monitor-enter site (apply the lock-field alias map, same
    // as edges) so a contention FILE:LINE resolves to lockdex's canonical lock.
    let method_by_key: HashMap<String, &Method> =
        methods.iter().map(|m| (m.key(), *m)).collect();
    let mut acquisitions: Vec<Acquisition> = Vec::new();
    for s in by_key.values() {
        for (l, line) in &s.acq_sites {
            // Naming a `synchronized(param)`: first try to resolve the parameter to
            // the concrete object bound at the method's call sites (same fixpoint);
            // else fall back to the parameter's declared class (its instance
            // monitor); else ground+canonicalize. This is naming-only — the graph
            // keeps Param roots distinct, so its soundness is unaffected.
            let lock = injector
                .resolve_param_lock(s.key.as_str(), l, &inj_solution)
                .map(|obj| canonicalize(&obj, &alias).name())
                .or_else(|| param_type_name(l, method_by_key.get(&s.key).copied()))
                .unwrap_or_else(|| canonicalize(&ground(l, &s.class, &s.key), &alias).name());
            acquisitions.push(Acquisition {
                class: s.class.clone(),
                method: s.key.clone(),
                line: *line,
                lock,
            });
        }
    }

    Analysis { edges, all_locks, method_count: by_key.len(), method_edges, paths, binder, races, acquisitions }
}

/// Mark calls that hit an async-dispatch method on a subtype of a dispatcher
/// base (`android.os.Handler`, the `java.util.concurrent` executors, `Thread`,
/// ... — see [`juc::ASYNC_BASES`], adjustable via `--async-dispatch`). Matching
/// is on the *declared* receiver class of the call: it is async iff the declared
/// class is, or transitively inherits from, a base and the method name is in
/// that base's dispatch set. `cfg.remove` entries veto, same as for the
/// name-based built-ins. Returns how many calls were reclassified.
fn mark_async_by_hierarchy(
    by_key: &mut HashMap<String, Summary>,
    supertypes: &HashMap<String, HashSet<String>>,
    cfg: &juc::AsyncConfig,
) -> usize {
    // class -> the dispatch method names that apply to it, through every base it
    // equals or (transitively) inherits from. The walk seeds with the dex
    // supertype closure and lets `base_hit` carry each name through the *known*
    // JDK/Android chain, so `MyPool extends ThreadPoolExecutor` reaches
    // `Executor.execute` even though no `java.util.concurrent` class is in the
    // dex. Memoized per class; calls then test membership without cloning.
    // every method name any base (built-in or user) could contribute.
    let candidates: HashSet<&str> = juc::ASYNC_BASES
        .iter()
        .flat_map(|(_, ms)| ms.iter().copied())
        .chain(cfg.add_base.values().flatten().map(|m| m.as_str()))
        .collect();
    let names_for = |class: &str| -> HashSet<String> {
        let roots = std::iter::once(class).chain(
            supertypes.get(class).into_iter().flat_map(|s| s.iter().map(|x| x.as_str())),
        );
        let mut names = HashSet::new();
        for root in roots {
            for m in &candidates {
                if !names.contains(*m) && cfg.base_hit(root, m) {
                    names.insert((*m).to_string());
                }
            }
        }
        names
    };
    let mut memo: HashMap<String, HashSet<String>> = HashMap::new();
    let mut n = 0;
    for s in by_key.values_mut() {
        for c in &mut s.calls {
            if c.is_async {
                continue;
            }
            if !memo.contains_key(&c.dclass) {
                let names = names_for(&c.dclass);
                memo.insert(c.dclass.clone(), names);
            }
            if !memo[&c.dclass].contains(&c.name) {
                continue;
            }
            let simple = c.dclass.rsplit('.').next().unwrap_or(&c.dclass);
            if juc::AsyncConfig::hit(&cfg.remove, &c.dclass, simple, &c.name) {
                continue;
            }
            c.is_async = true;
            n += 1;
        }
    }
    n
}

/// Assemble one method's contribution to the lock-order graph (pure / parallel).
fn assemble_one(
    k: &str,
    s: &Summary,
    resolved: &HashMap<String, Vec<Vec<String>>>,
    may: &HashMap<String, Vec<Lock>>,
    capture_map: &HashMap<String, HashMap<String, Lock>>,
    canon: &HashMap<String, Lock>,
    must: &HashMap<String, HashSet<String>>,
) -> MethodParts {
    // locks held on every entry to this method, plus whatever it holds locally — a
    // re-acquisition of any of these is reentrant and imposes no ordering.
    let entry_held = must.get(k);
    let mut edges: Vec<Edge> = Vec::new();
    let mut method_edges: Vec<MethodEdge> = Vec::new();
    let mut locks: Vec<Lock> = Vec::new();

    for e in &s.intra_edges {
        let from = resolve_lock(&e.from, s, capture_map, canon);
        let to = resolve_lock(&e.to, s, capture_map, canon);
        locks.push(from.clone());
        locks.push(to.clone());
        let reentrant = entry_held.is_some_and(|e| e.contains(&to.name()));
        if from != to && !from.is_opaque() && !to.is_opaque() && !reentrant {
            let guard = e.guard.iter().map(|g| resolve_lock(g, s, capture_map, canon)).collect();
            edges.push(Edge { from, to, guard, ..e.clone() });
        }
    }
    for l in &s.acquires {
        locks.push(resolve_lock(&ground(l, &s.class, k), s, capture_map, canon));
    }
    let rk = resolved.get(k);
    for (ci, call) in s.calls.iter().enumerate() {
        if call.is_async {
            continue;
        }
        let callees: &[String] = rk.and_then(|v| v.get(ci)).map(Vec::as_slice).unwrap_or(&[]);
        let held: Vec<Lock> = call.held.iter().map(|h| resolve_lock(h, s, capture_map, canon)).collect();
        if let Some(inner) = held.last() {
            for callee in callees {
                method_edges.push((k.to_string(), inner.name(), callee.clone()));
            }
        }
        for callee in callees {
            let Some(callee_may) = may.get(callee) else { continue };
            for cl in callee_may {
                let Some(sub) = subst_or_self(cl, &call.args) else { continue };
                let g = resolve_lock(&ground(&sub, &s.class, k), s, capture_map, canon);
                locks.push(g.clone());
                // The callee re-acquiring a lock already held — locally or on every
                // entry to this method — is reentrant, so it imposes no new ordering.
                if held.iter().any(|h| h == &g) || entry_held.is_some_and(|e| e.contains(&g.name())) {
                    continue;
                }
                for h in &held {
                    if h != &g && !h.is_opaque() && !g.is_opaque() {
                        let mut guard = held.clone();
                        guard.push(g.clone());
                        edges.push(Edge {
                            from: h.clone(),
                            to: g.clone(),
                            method: k.to_string(),
                            file: None,
                            line: call.line,
                            interproc: true,
                            guard,
                            nonblocking: false,
                        });
                    }
                }
            }
        }
    }
    (edges, method_edges, locks)
}


/// Per-`<init>` captured fields (`Class.field` -> formal), with super/this-ctor
/// chains threaded in: a field captured by a super constructor from its formal `i` is
/// also captured by a subclass constructor from whatever formal it forwards into `i`.
/// This carries an injected lock (`super(service)` -> `this.mService = service`) down
/// to the construction site, where the alias resolver can ground it.
fn index_ctor_captures(by_key: &HashMap<String, Summary>) -> HashMap<String, Vec<(String, u32)>> {
    let mut caps: HashMap<String, Vec<(String, u32)>> = by_key
        .iter()
        .filter(|(_, s)| !s.ctor_captures.is_empty() || s.super_init.is_some())
        .map(|(k, s)| (k.clone(), s.ctor_captures.clone()))
        .collect();
    let supers: Vec<(String, String, Vec<Option<u32>>)> = by_key
        .iter()
        .filter_map(|(k, s)| s.super_init.as_ref().map(|(sk, m)| (k.clone(), sk.clone(), m.clone())))
        .collect();
    // Fixpoint: pull each super ctor's captures down through the formal remapping.
    loop {
        let mut changed = false;
        for (k, sk, map) in &supers {
            let Some(super_caps) = caps.get(sk).cloned() else { continue };
            let add: Vec<(String, u32)> = super_caps
                .iter()
                .filter_map(|(fkey, sf)| match map.get(*sf as usize) {
                    Some(Some(mf)) => Some((fkey.clone(), *mf)),
                    _ => None,
                })
                .collect();
            let entry = caps.entry(k.clone()).or_default();
            for a in add {
                if !entry.contains(&a) {
                    entry.push(a);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    caps.retain(|_, v| !v.is_empty());
    caps
}

/// alloc site -> { captured field -> value (parametric in the allocating method) }.
fn build_capture_map(
    by_key: &HashMap<String, Summary>,
    ctor_captures: &HashMap<String, Vec<(String, u32)>>,
) -> HashMap<String, HashMap<String, Lock>> {
    let mut out: HashMap<String, HashMap<String, Lock>> = HashMap::new();
    for s in by_key.values() {
        for (site, ctor_key, args) in &s.alloc_inits {
            let Some(caps) = ctor_captures.get(ctor_key) else { continue };
            let mut fields = HashMap::new();
            for (field_key, formal) in caps {
                // capture paths key on the bare field name (`f$0`), not `Class.f$0`.
                let field = field_key.rsplit('.').next().unwrap_or(field_key);
                if let Some(Some(v)) = args.get(*formal as usize) {
                    fields.insert(field.to_string(), v.clone());
                }
            }
            if !fields.is_empty() {
                out.insert(site.clone(), fields);
            }
        }
    }
    out
}

/// Rewrite a lock whose access path passes through a captured lambda field
/// (`new@site.f$0.mLock` -> `<captured value>.mLock`), then ground it.
fn resolve_lock(
    lock: &Lock,
    s: &Summary,
    capture_map: &HashMap<String, HashMap<String, Lock>>,
    canon: &HashMap<String, Lock>,
) -> Lock {
    let mut cur = lock.clone();
    for _ in 0..MAX_AP {
        let Root::Alloc(site) = &cur.root else { break };
        let Some(fields) = capture_map.get(site) else { break };
        let Some(first) = cur.fields.first() else { break };
        let Some(cap) = fields.get(first) else { break };
        // replace alloc.first + rest with cap + rest
        let rest = cur.fields[1..].to_vec();
        cur = cap.append(&rest, cur.mode);
    }
    canonicalize(&ground(&cur, &s.class, &s.key), canon)
}

/// The compiler-synthesized field that holds an inner class's enclosing instance.
const OUTER_THIS: &str = "this$0";

/// Rewrite a lock rooted at an inner class's outer-instance field (`Inner.this$0`,
/// i.e. `Outer.this`) to the outer instance itself, so `synchronized(Outer.this)` in
/// an inner class names the same monitor as `synchronized(this)` in the outer class.
/// Sound: `this$0` *is* the enclosing instance, and the outer type is the inner type
/// minus its trailing `$Inner` segment. Used by the race analysis only — it does not
/// rewrite the deadlock edge graph, where two distinct enclosing instances of the
/// same outer type must stay separable.
pub(super) fn strip_outer_this(lock: &Lock) -> Lock {
    let Root::Recv(inner) = &lock.root else { return lock.clone() };
    let Some(rest) = lock.fields.split_first().filter(|(f, _)| *f == OUTER_THIS) else {
        return lock.clone();
    };
    let Some((outer, _)) = inner.rsplit_once('$') else { return lock.clone() };
    Lock { root: Root::Recv(outer.to_string()), fields: rest.1.to_vec(), mode: lock.mode }
}

/// Follow lock-field aliases: a field assigned a shared lock is canonicalized to
/// that lock's identity, so a singleton lock split across fields collapses to one.
fn canonicalize(lock: &Lock, canon: &HashMap<String, Lock>) -> Lock {
    if canon.is_empty() {
        return lock.clone();
    }
    let mut cur = lock.clone();
    for _ in 0..6 {
        if !matches!(cur.root, Root::Recv(_) | Root::Static(_)) {
            break;
        }
        let base = Lock { mode: Mode::Plain, ..cur.clone() }.name();
        match canon.get(&base) {
            Some(t) => cur = t.with_mode(cur.mode),
            None => break,
        }
    }
    cur
}

// ---------------------------------------------------------------------------
// Interprocedural injection resolution
// ---------------------------------------------------------------------------
// Resolve each lock field to the concrete object it names, by parameter/copy
// propagation over the call graph. `this.f = param_i` (in ANY method) makes the
// field an alias of formal `i`; a formal is the meet, over every call site that
// binds it, of the resolved actual argument. This is a monotone dataflow system
// solved by a worklist to its LEAST fixpoint: cycles converge naturally (a
// grounded cycle settles on its grounded value, an ungrounded one stays
// unresolved), and constructors, setters and `super(...)` are all just call
// sites, so ctor / setter / inheritance injection fall out of one algorithm.
// Sound: a conflict, or an actual that does not resolve to a single object,
// yields Top and produces no alias.

// A resolution in the meet-semilattice  Bottom (no info) < Val(x) < Top (conflict).
#[derive(Clone, PartialEq)]
enum Res {
    Bottom,
    Val(Lock),
    Top,
}

impl Res {
    fn meet(self, other: Res) -> Res {
        match (self, other) {
            (Res::Top, _) | (_, Res::Top) => Res::Top,
            (Res::Bottom, x) | (x, Res::Bottom) => x,
            (Res::Val(a), Res::Val(b)) => {
                if a == b { Res::Val(a) } else { Res::Top }
            }
        }
    }
}

/// A `(method key, formal index)` variable of the analysis (receiver = 0).
type Formal<'a> = (&'a str, u32);

/// One observed call site of a method: who called it and with what actuals.
struct CallSite<'a> {
    class: &'a str,               // caller's class
    key: &'a str,                 // caller's method key
    actuals: &'a [Option<Lock>],  // actuals[i] binds the callee's formal i
}

struct Injector<'a> {
    sites: HashMap<&'a str, Vec<CallSite<'a>>>,
    field_stores: HashMap<&'a str, Vec<Formal<'a>>>,
}

impl<'a> Injector<'a> {
    fn build(by_key: &'a HashMap<String, Summary>) -> Self {
        let mut sites: HashMap<&str, Vec<CallSite>> = HashMap::new();
        let mut field_stores: HashMap<&str, Vec<Formal>> = HashMap::new();
        for s in by_key.values() {
            for (callee, actuals) in &s.arg_bindings {
                sites.entry(callee.as_str()).or_default().push(CallSite {
                    class: s.class.as_str(),
                    key: s.key.as_str(),
                    actuals: actuals.as_slice(),
                });
            }
            for (field, formal) in &s.param_stores {
                field_stores.entry(field.as_str()).or_default().push((s.key.as_str(), *formal));
            }
        }
        Injector { sites, field_stores }
    }

    /// The value of one actual argument under the current partial solution `f`.
    fn eval(&self, actual: Option<&Lock>, cs: &CallSite<'a>, f: &HashMap<Formal<'a>, Res>) -> Res {
        let Some(l) = actual else { return Res::Top };
        match &l.root {
            // the caller's own formal: take its current value, re-appending this
            // actual's field path (an actual of `param.mLock`).
            Root::Param(j) => match f.get(&(cs.key, *j)).cloned().unwrap_or(Res::Bottom) {
                Res::Val(base) => Res::Val(base.append(&l.fields, l.mode)),
                other => other,
            },
            // `this` / a field-of-this / a static: ground in the caller's frame.
            _ => {
                let g = ground(l, cs.class, cs.key);
                match g.root {
                    Root::Recv(_) | Root::Static(_) => Res::Val(g),
                    _ => Res::Top,
                }
            }
        }
    }

    /// Transfer: a formal is the meet of its actuals over all call sites. A method
    /// with no observed call site is unconstrained -> Top.
    fn transfer(&self, (m, i): Formal<'a>, f: &HashMap<Formal<'a>, Res>) -> Res {
        let Some(css) = self.sites.get(m).filter(|v| !v.is_empty()) else {
            return Res::Top;
        };
        let mut acc = Res::Bottom;
        for cs in css {
            let a = cs.actuals.get(i as usize).and_then(|o| o.as_ref());
            acc = acc.meet(self.eval(a, cs, f));
            if acc == Res::Top {
                break;
            }
        }
        acc
    }

    /// Least-fixpoint solve: gather the reachable formals (the field stores plus
    /// any `extra_seeds`, e.g. `synchronized(param)` operands) and their
    /// dependency edges, then run a monotone worklist to convergence.
    fn solve(&self, extra_seeds: &[Formal<'a>]) -> HashMap<Formal<'a>, Res> {
        let mut vars: HashSet<Formal> = HashSet::new();
        let mut rev: HashMap<Formal, Vec<Formal>> = HashMap::new(); // dep -> readers
        let mut stack: Vec<Formal> = self.field_stores.values().flatten().copied().collect();
        stack.extend_from_slice(extra_seeds);
        while let Some((m, i)) = stack.pop() {
            if !vars.insert((m, i)) {
                continue;
            }
            for cs in self.sites.get(m).into_iter().flatten() {
                if let Some(Some(a)) = cs.actuals.get(i as usize) {
                    if let Root::Param(j) = a.root {
                        rev.entry((cs.key, j)).or_default().push((m, i));
                        stack.push((cs.key, j));
                    }
                }
            }
        }
        let mut f: HashMap<Formal, Res> = vars.iter().map(|&v| (v, Res::Bottom)).collect();
        let mut wl: Vec<Formal> = vars.into_iter().collect();
        while let Some(v) = wl.pop() {
            let nv = self.transfer(v, &f);
            if f.get(&v) != Some(&nv) {
                f.insert(v, nv);
                if let Some(deps) = rev.get(&v) {
                    wl.extend(deps.iter().copied());
                }
            }
        }
        f
    }

    /// Resolve a `synchronized(param)` operand to the concrete object bound at the
    /// method's call sites, if it resolves to one. `None` for a non-parameter lock
    /// or one that does not resolve (the caller then names it by its type).
    fn resolve_param_lock(&self, method: &'a str, l: &Lock, f: &HashMap<Formal<'a>, Res>) -> Option<Lock> {
        let Root::Param(j) = l.root else { return None };
        match f.get(&(method, j)) {
            Some(Res::Val(base)) => Some(base.append(&l.fields, l.mode)),
            _ => None,
        }
    }

    /// Every field that resolves to a single concrete object other than itself.
    fn resolve_fields(&self, f: &HashMap<Formal<'a>, Res>) -> Vec<(String, Lock)> {
        let mut out = Vec::new();
        for (field, stores) in &self.field_stores {
            let mut acc = Res::Bottom;
            for &(m, i) in stores {
                // Residual Bottom (ungrounded) is unresolved -> Top.
                let r = match f.get(&(m, i)) {
                    Some(Res::Val(v)) => Res::Val(v.clone()),
                    _ => Res::Top,
                };
                acc = acc.meet(r);
                if acc == Res::Top {
                    break;
                }
            }
            if let Res::Val(v) = acc {
                if v.name().as_str() != *field {
                    out.push(((*field).to_string(), v));
                }
            }
        }
        out
    }
}
// ---------------------------------------------------------------------------
// mayAcquire fixpoint
// ---------------------------------------------------------------------------

fn subst_or_self(lock: &Lock, args: &[Option<Lock>]) -> Option<Lock> {
    if lock.is_parametric() {
        subst(lock, args)
    } else {
        Some(lock.clone())
    }
}

fn ground(lock: &Lock, class: &str, key: &str) -> Lock {
    if lock.is_parametric() {
        lock.ground(class, key)
    } else {
        lock.clone()
    }
}

/// A lock with no cross-thread identity: an unresolved monitor (`Opaque`) or one
/// taken on a freshly allocated object (`Alloc`). It cannot guard shared state, so
/// the binder and race analyses ignore it.
pub(super) fn is_local_lock(lock: &Lock) -> bool {
    matches!(lock.root, Root::Opaque(_) | Root::Alloc(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(dclass: &str, name: &str) -> RawCall {
        RawCall {
            kind: InvokeKind::Virtual,
            dclass: dclass.to_string(),
            name: name.to_string(),
            sig: "(Ljava/lang/Runnable;)V".to_string(),
            recv_type: None,
            args: Vec::new(),
            held: Vec::new(),
            line: None,
            is_async: false,
        }
    }

    fn fixture(
        calls: Vec<RawCall>,
    ) -> (HashMap<String, Summary>, HashMap<String, HashSet<String>>) {
        let mut by_key = HashMap::new();
        by_key.insert(
            "test.M.m:()V".to_string(),
            Summary { key: "test.M.m:()V".to_string(), class: "test.M".to_string(), calls, ..Default::default() },
        );
        let mut supertypes = HashMap::new();
        supertypes.insert(
            "test.MyHandler".to_string(),
            HashSet::from(["android.os.Handler".to_string(), "java.lang.Object".to_string()]),
        );
        supertypes.insert(
            "test.WorkerPool".to_string(),
            HashSet::from(["java.util.concurrent.Executor".to_string(), "java.lang.Object".to_string()]),
        );
        supertypes.insert(
            "test.Plain".to_string(),
            HashSet::from(["java.lang.Object".to_string()]),
        );
        (by_key, supertypes)
    }

    fn async_flags(by_key: &HashMap<String, Summary>) -> Vec<bool> {
        by_key["test.M.m:()V"].calls.iter().map(|c| c.is_async).collect()
    }

    #[test]
    fn handler_and_executor_subtypes_are_marked_async() {
        let (mut by_key, sup) = fixture(vec![
            call("test.MyHandler", "post"),
            call("test.MyHandler", "sendMessageAtFrontOfQueue"),
            call("test.WorkerPool", "execute"),
            call("test.Plain", "execute"),       // not a dispatcher subtype
            call("test.MyHandler", "handleMessage"), // not a dispatch method
        ]);
        let n = mark_async_by_hierarchy(&mut by_key, &sup, &juc::AsyncConfig::default());
        assert_eq!(n, 3);
        assert_eq!(async_flags(&by_key), vec![true, true, true, false, false]);
    }

    #[test]
    fn declared_base_class_itself_matches() {
        // framework not in the dex: the declared class IS the base, with no
        // supertypes entry at all.
        let (mut by_key, _) = fixture(vec![call("android.os.Handler", "postAtFrontOfQueue")]);
        let n = mark_async_by_hierarchy(&mut by_key, &HashMap::new(), &juc::AsyncConfig::default());
        assert_eq!(n, 1);
    }

    #[test]
    fn remove_entry_vetoes_hierarchy_match() {
        let (mut by_key, sup) = fixture(vec![call("test.MyHandler", "post")]);
        let cfg = juc::AsyncConfig {
            remove: HashSet::from(["MyHandler.post".to_string()]),
            ..Default::default()
        };
        assert_eq!(mark_async_by_hierarchy(&mut by_key, &sup, &cfg), 0);
        assert_eq!(async_flags(&by_key), vec![false]);
    }

    #[test]
    fn remove_base_disables_a_builtin_base() {
        let (mut by_key, sup) = fixture(vec![
            call("test.MyHandler", "post"),
            call("test.WorkerPool", "execute"),
        ]);
        let cfg = juc::AsyncConfig {
            remove_base: HashSet::from(["android.os.Handler".to_string()]),
            ..Default::default()
        };
        assert_eq!(mark_async_by_hierarchy(&mut by_key, &sup, &cfg), 1);
        assert_eq!(async_flags(&by_key), vec![false, true]);
    }

    #[test]
    fn add_base_extends_the_table() {
        let (mut by_key, sup) = fixture(vec![call("test.Plain", "enqueue")]);
        let mut sup = sup;
        sup.insert(
            "test.Plain".to_string(),
            HashSet::from(["com.example.MyQueue".to_string()]),
        );
        let cfg = juc::AsyncConfig {
            add_base: HashMap::from([(
                "com.example.MyQueue".to_string(),
                HashSet::from(["enqueue".to_string()]),
            )]),
            ..Default::default()
        };
        assert_eq!(mark_async_by_hierarchy(&mut by_key, &sup, &cfg), 1);
    }
}


