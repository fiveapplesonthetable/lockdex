//! Compositional lock-order analysis.
//!
//! Per-method extraction (parallel) records *raw* facts; a global phase builds an
//! RTA call graph, resolves lambda captures, runs the mayAcquire fixpoint, and
//! assembles the lock-order edges. Lock identity is an access path
//! (receiver-sensitive); virtual/interface dispatch is resolved by CHA pruned to
//! instantiated types (RTA) plus receiver-type refinement; lambda capture fields
//! are resolved to the values passed to the synthetic constructor.

use crate::juc::{self, LockCall};
use crate::model::*;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

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

#[derive(Debug, Clone, Default)]
struct Summary {
    key: String,
    class: String,
    intra_edges: Vec<Edge>,
    first_acquire: Vec<Lock>,
    acquires: Vec<Lock>,
    calls: Vec<RawCall>,
    value_summary: Option<Lock>,
    /// allocation sites created here: site -> type.
    allocs: Vec<(String, String)>,
    /// `new T(...)` followed by `<init>`: (site, ctor_key, arg values).
    alloc_inits: Vec<(String, String, Vec<Option<Lock>>)>,
    /// if this method is a `<init>`, captured fields: field -> formal it stores.
    ctor_captures: Vec<(String, u32)>,
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

pub fn analyze(dex: &Dex, cfg: &juc::SinkConfig) -> Analysis {
    let t = Instant::now();
    let methods: Vec<&Method> = dex.classes.iter().flat_map(|c| c.methods.iter()).collect();
    let supertypes = build_supertypes(dex);

    // --- per-method summaries (parallel: value summaries, then full) ---------
    let empty: HashMap<String, Lock> = HashMap::new();
    let value_summaries: HashMap<String, Lock> = methods
        .par_iter()
        .filter_map(|m| extract(m, &empty, cfg).value_summary.map(|v| (m.key(), v)))
        .collect();
    let summaries: Vec<Summary> = methods.par_iter().map(|m| extract(m, &value_summaries, cfg)).collect();
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
                let cclass = class_of_key(ctor_key);
                for (field, formal) in caps {
                    let key = format!("{cclass}.{field}");
                    let arg = args.get(*formal as usize).and_then(|o| o.clone());
                    let v = arg.filter(|v| {
                        matches!(v.root, Root::Recv(_) | Root::Static(_))
                            && !v.fields.is_empty()
                            && v.name() != key
                    });
                    note(key, v, &mut seen);
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

    // --- edge assembly (parallel per method) ---------------------------------
    let tea = Instant::now();
    let mut asm_keys: Vec<&String> = by_key.keys().collect();
    asm_keys.sort();
    let parts: Vec<(Vec<Edge>, Vec<(String, String, String)>, Vec<Lock>)> = asm_keys
        .par_iter()
        .map(|k| assemble_one(k, &by_key[*k], &resolved, &may, &capture_map, &alias))
        .collect();
    let mut edges: Vec<Edge> = Vec::new();
    let mut method_edges: Vec<(String, String, String)> = Vec::new();
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

    Analysis { edges, all_locks, method_count: by_key.len(), method_edges, paths }
}

/// Assemble one method's contribution to the lock-order graph (pure / parallel).
fn assemble_one(
    k: &str,
    s: &Summary,
    resolved: &HashMap<String, Vec<Vec<String>>>,
    may: &HashMap<String, Vec<Lock>>,
    capture_map: &HashMap<String, HashMap<String, Lock>>,
    canon: &HashMap<String, Lock>,
) -> (Vec<Edge>, Vec<(String, String, String)>, Vec<Lock>) {
    let mut edges: Vec<Edge> = Vec::new();
    let mut method_edges: Vec<(String, String, String)> = Vec::new();
    let mut locks: Vec<Lock> = Vec::new();

    for e in &s.intra_edges {
        let from = resolve_lock(&e.from, s, capture_map, canon);
        let to = resolve_lock(&e.to, s, capture_map, canon);
        locks.push(from.clone());
        locks.push(to.clone());
        if from != to && !from.is_opaque() && !to.is_opaque() {
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

// ---------------------------------------------------------------------------
// call graph (CHA + RTA + receiver refinement)
// ---------------------------------------------------------------------------

/// A virtual/interface site with more than this many instantiated candidate
/// targets is treated as *megamorphic*: we resolve it to the declared target only
/// (if that has a body), otherwise to nothing. Dropping the fan-out is sound — an
/// unresolved call adds no edges, so we miss rather than fabricate — and it stops
/// one spurious dispatch target from welding unrelated lock clusters into a single
/// giant SCC. Small (mono/poly-morphic) sites are resolved precisely.
const POLY_LIMIT: usize = 4;

struct CallGraph {
    methods_by_namesig: HashMap<String, Vec<String>>,
    instantiated: HashSet<String>,
    supertypes: HashMap<String, HashSet<String>>,
}

impl CallGraph {
    fn is_subtype(&self, sub: &str, sup: &str) -> bool {
        sub == sup || self.supertypes.get(sub).map(|s| s.contains(sup)).unwrap_or(false)
    }

    fn resolve(&self, c: &RawCall, by_key: &HashMap<String, Summary>) -> Vec<String> {
        match c.kind {
            InvokeKind::Static | InvokeKind::Direct | InvokeKind::Super => {
                let k = c.declared_key();
                if by_key.contains_key(&k) { vec![k] } else { vec![] }
            }
            InvokeKind::Virtual | InvokeKind::Interface => {
                // receiver-type refinement: exact dispatch to a known concrete type.
                if let Some(t) = &c.recv_type {
                    let k = format!("{}.{}", t, c.namesig());
                    if by_key.contains_key(&k) {
                        return vec![k];
                    }
                }
                let dk = c.declared_key();
                let has_body = by_key.contains_key(&dk);
                // RTA: instantiated subtypes of the declared class with this name:sig.
                let mut subs: Vec<String> = Vec::new();
                let mut megamorphic = false;
                if let Some(cands) = self.methods_by_namesig.get(&c.namesig()) {
                    for cand in cands {
                        let cclass = class_of_key(cand);
                        if self.instantiated.contains(cclass)
                            && self.is_subtype(cclass, &c.dclass)
                            && *cand != dk
                        {
                            subs.push(cand.clone());
                            if subs.len() > POLY_LIMIT {
                                megamorphic = true;
                                break;
                            }
                        }
                    }
                }
                if megamorphic {
                    // drop the fan-out; keep only a concrete declared body if any.
                    if has_body { vec![dk] } else { vec![] }
                } else {
                    if has_body {
                        subs.push(dk);
                    }
                    subs
                }
            }
        }
    }
}

fn class_of_key(key: &str) -> &str {
    // key = "a.b.C.method:sig"  -> class "a.b.C"
    let pre = key.split(':').next().unwrap_or(key);
    match pre.rfind('.') {
        Some(i) => &pre[..i],
        None => pre,
    }
}

fn build_supertypes(dex: &Dex) -> HashMap<String, HashSet<String>> {
    let mut direct: HashMap<String, Vec<String>> = HashMap::new();
    for c in &dex.classes {
        let mut sups = Vec::new();
        if let Some(s) = &c.super_class {
            sups.push(s.clone());
        }
        sups.extend(c.interfaces.iter().cloned());
        direct.insert(c.descriptor.clone(), sups);
    }
    // transitive closure.
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for c in direct.keys() {
        let mut seen = HashSet::new();
        let mut stack = direct.get(c).cloned().unwrap_or_default();
        while let Some(t) = stack.pop() {
            if seen.insert(t.clone()) {
                if let Some(more) = direct.get(&t) {
                    stack.extend(more.iter().cloned());
                }
            }
        }
        out.insert(c.clone(), seen);
    }
    out
}

fn index_namesig(by_key: &HashMap<String, Summary>) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for k in by_key.keys() {
        // namesig = everything after the class.
        if let Some(ns) = namesig_of_key(k) {
            m.entry(ns).or_default().push(k.clone());
        }
    }
    // deterministic candidate order so the RTA fan-out cap is reproducible.
    for v in m.values_mut() {
        v.sort();
    }
    m
}

fn namesig_of_key(key: &str) -> Option<String> {
    let colon = key.find(':')?;
    let dot = key[..colon].rfind('.')?;
    Some(key[dot + 1..].to_string())
}

fn index_ctor_captures(by_key: &HashMap<String, Summary>) -> HashMap<String, Vec<(String, u32)>> {
    by_key
        .iter()
        .filter(|(_, s)| !s.ctor_captures.is_empty())
        .map(|(k, s)| (k.clone(), s.ctor_captures.clone()))
        .collect()
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
            for (field, formal) in caps {
                if let Some(Some(v)) = args.get(*formal as usize) {
                    fields.insert(field.clone(), v.clone());
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

/// Per-method cap on the mayAcquire set. Beyond this a method is "saturated" and
/// stops growing — sound (under-approximation) and keeps the fixpoint near-linear.
const MAY_CAP: usize = 96;

fn may_acquire(
    by_key: &HashMap<String, Summary>,
    resolved: &HashMap<String, Vec<Vec<String>>>,
) -> (HashMap<String, Vec<Lock>>, usize) {
    // seed with concrete (non-opaque) acquired locks only; opaque locks can never
    // form a cycle, and propagating them (esp. truncated paths) never converges.
    let mut may: HashMap<String, HashSet<Lock>> = by_key
        .iter()
        .map(|(k, s)| (k.clone(), s.acquires.iter().filter(|l| !l.is_opaque()).cloned().collect()))
        .collect();
    let mut keys: Vec<String> = by_key.keys().cloned().collect();
    keys.sort(); // stable index order -> reproducible saturation under MAY_CAP

    let mut iters = 0;
    while iters < 25 {
        // Jacobi round: each method's additions computed in parallel against the
        // previous round's `may` (read-only), then applied deterministically.
        let additions: Vec<(usize, Vec<Lock>)> = keys
            .par_iter()
            .enumerate()
            .filter_map(|(ki, k)| {
                if may[k].len() >= MAY_CAP {
                    return None;
                }
                let s = &by_key[k];
                let mut add: HashSet<Lock> = HashSet::new();
                for (ci, call) in s.calls.iter().enumerate() {
                    if call.is_async {
                        continue;
                    }
                    let Some(cands) = resolved.get(k).and_then(|v| v.get(ci)) else { continue };
                    for callee in cands {
                        let Some(set) = may.get(callee) else { continue };
                        for cl in set {
                            if let Some(sub) = subst_or_self(cl, &call.args) {
                                if !sub.is_opaque() && !may[k].contains(&sub) {
                                    add.insert(sub);
                                }
                            }
                        }
                    }
                }
                if add.is_empty() {
                    None
                } else {
                    let mut v: Vec<Lock> = add.into_iter().collect();
                    v.sort_by(|a, b| a.name().cmp(&b.name()));
                    Some((ki, v))
                }
            })
            .collect();
        iters += 1;
        if additions.is_empty() {
            break;
        }
        let mut additions = additions;
        additions.sort_by_key(|(ki, _)| *ki);
        for (ki, v) in additions {
            let set = may.get_mut(&keys[ki]).unwrap();
            for l in v {
                if set.len() >= MAY_CAP {
                    break;
                }
                set.insert(l);
            }
        }
    }
    (may.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(), iters)
}

// ---------------------------------------------------------------------------
// per-method extraction
// ---------------------------------------------------------------------------

fn extract(m: &Method, value_summaries: &HashMap<String, Lock>, cfg: &juc::SinkConfig) -> Summary {
    let mut s = Summary { key: m.key(), class: m.class.clone(), ..Default::default() };
    let mut regs: HashMap<Reg, Lock> = HashMap::new();
    let mut alloc_ty: HashMap<String, String> = HashMap::new(); // alloc site -> type
    let mut held: Vec<Lock> = Vec::new();
    let mut last_ret: Option<Lock> = None;
    let mut returns: Vec<Option<Lock>> = Vec::new();
    let is_ctor = m.name == "<init>";

    if let Some(t) = m.this_reg() {
        regs.insert(t, Lock::new(Root::This));
        for j in 1..m.ins {
            regs.insert(t + j, Lock::new(Root::Param(j)));
        }
    } else if m.ins > 0 {
        let base = m.registers - m.ins;
        for j in 0..m.ins {
            regs.insert(base + j, Lock::new(Root::Param(j)));
        }
    }

    let line_at = |off: u32| m.line_at(off);

    for insn in &m.insns {
        match &insn.op {
            Op::Sget { dst, class, field } => {
                regs.insert(*dst, Lock::field(Root::Static(class.clone()), field.clone()));
            }
            Op::Iget { dst, base, class, field } => {
                // Concrete, type-based identity: a field lock is keyed by the
                // field's DECLARING class + name (straight from the iget), so every
                // alias of a singleton (`a.mPm.mLock`, `b.mPm.mLock`) collapses to
                // one node (`PackageManagerService.mLock`). This both merges
                // aliases correctly and keeps access paths depth-1 (no blow-up).
                let _ = base;
                regs.insert(*dst, Lock::field(Root::Recv(class.clone()), field.clone()));
            }
            Op::Iput { src, base, class, field } => {
                if is_ctor {
                    // lambda capture: `iput pSrc, this, f` captures the formal.
                    if let Some(Root::Param(i)) = regs.get(src).map(|l| l.root.clone()) {
                        s.ctor_captures.push((field.clone(), i));
                    } else if regs.get(src).map(|l| matches!(l.root, Root::This)).unwrap_or(false) {
                        s.ctor_captures.push((field.clone(), 0));
                    }
                    // lock-field alias: `this.f = <a lock that lives elsewhere>`
                    // (e.g. service.getLock(), already inlined to a concrete
                    // field/static lock in the source register). Records that
                    // `class.field` is the same object as that lock.
                    if let Some(v) = regs.get(src) {
                        if matches!(v.root, Root::Recv(_) | Root::Static(_)) && !v.fields.is_empty() {
                            let key = format!("{}.{}", class, field);
                            if v.name() != key {
                                s.field_aliases.push((key, v.clone()));
                            }
                        }
                    }
                }
                let _ = base;
            }
            Op::ConstClass { dst, class } => {
                regs.insert(*dst, Lock::new(Root::ClassConst(class.clone())));
            }
            Op::NewInstance { dst, class } => {
                let site = format!("{}+{:04x}", m.key(), insn.offset);
                regs.insert(*dst, Lock::new(Root::Alloc(site.clone())));
                alloc_ty.insert(site.clone(), class.clone());
                s.allocs.push((site, class.clone()));
            }
            Op::Move { dst, src } => match regs.get(src).cloned() {
                Some(v) => { regs.insert(*dst, v); }
                None => { regs.remove(dst); }
            },
            Op::MoveResult { dst } => match last_ret.take() {
                Some(v) => { regs.insert(*dst, v); }
                None => { regs.remove(dst); }
            },
            Op::Def(dst) => { regs.remove(dst); }
            Op::Return(r) => {
                returns.push(r.and_then(|rr| regs.get(&rr).cloned()));
            }
            Op::Invoke(inv) => {
                last_ret = None;
                // record alloc<-init binding for capture summaries.
                if inv.name == "<init>" {
                    if let Some(r0) = inv.args.first() {
                        if let Some(Lock { root: Root::Alloc(site), .. }) = regs.get(r0) {
                            s.alloc_inits.push((site.clone(), inv.key(), arg_vals(&regs, inv)));
                        }
                    }
                }
                match juc::classify(&inv.class, &inv.name, cfg) {
                    Some(LockCall::ReadView) => {
                        last_ret = inv.args.first().and_then(|r| regs.get(r)).map(|l| l.with_mode(Mode::Read));
                    }
                    Some(LockCall::WriteView) => {
                        last_ret = inv.args.first().and_then(|r| regs.get(r)).map(|l| l.with_mode(Mode::Write));
                    }
                    Some(LockCall::Acquire) | Some(LockCall::TryAcquire) => {
                        let nb = matches!(juc::classify(&inv.class, &inv.name, cfg), Some(LockCall::TryAcquire));
                        let lock = inv.args.first().and_then(|r| regs.get(r)).cloned()
                            .unwrap_or_else(|| Lock::new(Root::Opaque(format!("{}+{:04x}", m.key(), insn.offset))));
                        acquire(&mut s, m, &mut held, lock, line_at(insn.offset), nb);
                    }
                    Some(LockCall::Release) => {
                        if let Some(lock) = inv.args.first().and_then(|r| regs.get(r)).cloned() {
                            release(&mut held, &ground(&lock, &m.class, &m.key()));
                        } else if !held.is_empty() {
                            held.pop();
                        }
                    }
                    Some(LockCall::AsyncSink) => {
                        record_call(&mut s, &regs, inv, &held, line_at(insn.offset), true, &alloc_ty);
                    }
                    None => {
                        last_ret = value_summaries.get(&inv.key()).and_then(|vs| {
                            subst_or_self(vs, &arg_vals(&regs, inv))
                        });
                        record_call(&mut s, &regs, inv, &held, line_at(insn.offset), false, &alloc_ty);
                    }
                }
            }
            Op::MonitorEnter(r) => {
                let lock = regs.get(r).cloned()
                    .unwrap_or_else(|| Lock::new(Root::Opaque(format!("{}+{:04x}", m.key(), insn.offset))));
                acquire(&mut s, m, &mut held, lock, line_at(insn.offset), false);
            }
            Op::MonitorExit(r) => match regs.get(r).cloned() {
                Some(l) => release(&mut held, &ground(&l, &m.class, &m.key())),
                None => { held.pop(); }
            },
            Op::Other => {}
        }
    }

    if let Some(Some(first)) = returns.first().cloned() {
        if returns.iter().all(|r| r.as_ref() == Some(&first)) && simple_value(&first) {
            s.value_summary = Some(first);
        }
    }
    s
}

fn simple_value(l: &Lock) -> bool {
    // a trivial getter's return is inlinable if it names a concrete, stable lock.
    matches!(
        l.root,
        Root::This | Root::Param(_) | Root::Recv(_) | Root::Static(_) | Root::ClassConst(_)
    )
}

fn arg_vals(regs: &HashMap<Reg, Lock>, inv: &Invoke) -> Vec<Option<Lock>> {
    inv.args.iter().map(|r| regs.get(r).cloned()).collect()
}

fn record_call(s: &mut Summary, regs: &HashMap<Reg, Lock>, inv: &Invoke, held: &[Lock], line: Option<u32>, is_async: bool, alloc_ty: &HashMap<String, String>) {
    let recv_type = if matches!(inv.kind, InvokeKind::Virtual | InvokeKind::Interface) {
        inv.args.first().and_then(|r| regs.get(r)).and_then(|l| alloc_type(l, alloc_ty))
    } else {
        None
    };
    s.calls.push(RawCall {
        kind: inv.kind,
        dclass: inv.class.clone(),
        name: inv.name.clone(),
        sig: inv.sig.clone(),
        recv_type,
        args: arg_vals(regs, inv),
        held: held.to_vec(),
        line,
        is_async,
    });
}

/// The concrete type of a receiver that is a bare, freshly-allocated object, so a
/// virtual/interface call on it can be dispatched exactly to that type instead of
/// going through RTA. Only a bare `Alloc` root (no field deref) is a known object.
fn alloc_type(l: &Lock, alloc_ty: &HashMap<String, String>) -> Option<String> {
    match &l.root {
        Root::Alloc(site) if l.fields.is_empty() => alloc_ty.get(site).cloned(),
        _ => None,
    }
}

fn acquire(s: &mut Summary, m: &Method, held: &mut Vec<Lock>, lock: Lock, line: Option<u32>, nonblocking: bool) {
    let grounded = ground(&lock, &m.class, &m.key());
    if held.iter().any(|h| h == &grounded) {
        held.push(grounded);
        return;
    }
    if held.is_empty() {
        s.first_acquire.push(lock.clone());
    }
    s.acquires.push(lock.clone());
    if !nonblocking {
        for h in held.iter() {
            if h != &grounded && !h.is_opaque() && !grounded.is_opaque() {
                let mut guard = held.clone();
                guard.push(grounded.clone());
                s.intra_edges.push(Edge {
                    from: h.clone(),
                    to: grounded.clone(),
                    method: m.key(),
                    file: m.source_file.clone(),
                    line,
                    interproc: false,
                    guard,
                    nonblocking: false,
                });
            }
        }
    }
    held.push(grounded);
}

fn release(held: &mut Vec<Lock>, lock: &Lock) {
    if let Some(pos) = held.iter().rposition(|h| h.name() == lock.name() || h == lock) {
        held.remove(pos);
    } else {
        held.pop();
    }
}
