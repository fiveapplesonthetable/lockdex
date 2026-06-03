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
    field: String,
    write: bool,
    line: Option<u32>,
    held: Vec<Lock>,
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
}

/// Enough of the call graph to reconstruct, for an order edge `A -> B`, the
/// shortest call chain from the method holding `A` to the method that acquires
/// `B`. Built from the resolved (RTA) call graph and the mayAcquire fixpoint.
pub struct PathIndex {
    /// method key -> its (non-async) resolved callee method keys.
    callees: HashMap<String, Vec<String>>,
    /// method key -> lock names it may (transitively) acquire.
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

    // --- global indices + call graph (parallel resolution) -------------------
    let methods_by_namesig = index_namesig(&by_key);
    let instantiated: HashSet<String> =
        by_key.values().flat_map(|s| s.allocs.iter().map(|(_, t)| t.clone())).collect();
    let ctor_captures = index_ctor_captures(&by_key);
    let capture_map = build_capture_map(&by_key, &ctor_captures);
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
        // (b) constructor-parameter assignments, resolved at construction sites
        for s in by_key.values() {
            for (site, ctor_key, args) in &s.alloc_inits {
                let _ = site;
                let Some(caps) = ctor_captures.get(ctor_key) else { continue };
                for (key, formal) in caps {
                    let arg = args.get(*formal as usize).and_then(|o| o.clone());
                    let v = arg.filter(|v| {
                        matches!(v.root, Root::Recv(_) | Root::Static(_)) && &v.name() != key
                    });
                    note(key.clone(), v, &mut seen);
                }
            }
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

    Analysis { edges, all_locks, method_count: by_key.len(), method_edges, paths, binder, races }
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


