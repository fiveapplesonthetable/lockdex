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
}

pub struct Analysis {
    pub edges: Vec<Edge>,
    pub all_locks: HashSet<Lock>,
    pub method_count: usize,
    /// method dependency graph: (caller method, lock held at the call, callee).
    /// Powers the Perfetto/pprof method-graph view (a call made while holding L).
    pub method_edges: Vec<(String, String, String)>,
}

pub fn analyze(dex: &Dex) -> Analysis {
    let methods: Vec<&Method> = dex.classes.iter().flat_map(|c| c.methods.iter()).collect();

    // class hierarchy (for RTA subtype filtering).
    let supertypes = build_supertypes(dex);

    // --- pass A: value summaries --------------------------------------------
    let empty: HashMap<String, Lock> = HashMap::new();
    let value_summaries: HashMap<String, Lock> = methods
        .par_iter()
        .filter_map(|m| extract(m, &empty).value_summary.map(|v| (m.key(), v)))
        .collect();

    // --- pass B: full extraction --------------------------------------------
    let summaries: Vec<Summary> = methods.par_iter().map(|m| extract(m, &value_summaries)).collect();
    let mut by_key: HashMap<String, Summary> = HashMap::new();
    for s in summaries {
        by_key.entry(s.key.clone()).or_insert(s);
    }

    // --- global indices ------------------------------------------------------
    let methods_by_namesig = index_namesig(&by_key);
    let instantiated: HashSet<String> = by_key
        .values()
        .flat_map(|s| s.allocs.iter().map(|(_, t)| t.clone()))
        .collect();
    let ctor_captures = index_ctor_captures(&by_key);
    let capture_map = build_capture_map(&by_key, &ctor_captures);

    let cg = CallGraph { methods_by_namesig, instantiated, supertypes };

    // resolve every call's candidate callees once.
    let resolved: HashMap<String, Vec<Vec<String>>> = by_key
        .par_iter()
        .map(|(k, s)| (k.clone(), s.calls.iter().map(|c| cg.resolve(c, &by_key)).collect()))
        .collect();

    // --- mayAcquire fixpoint -------------------------------------------------
    let may = may_acquire(&by_key, &resolved);

    // --- edge assembly -------------------------------------------------------
    let mut edges: Vec<Edge> = Vec::new();
    let mut all_locks: HashSet<Lock> = HashSet::new();
    let mut method_edges: Vec<(String, String, String)> = Vec::new();
    let mut asm_keys: Vec<&String> = by_key.keys().collect();
    asm_keys.sort();
    for k in asm_keys {
        let s = &by_key[k];
        for e in &s.intra_edges {
            let from = resolve_lock(&e.from, s, &capture_map);
            let to = resolve_lock(&e.to, s, &capture_map);
            all_locks.insert(from.clone());
            all_locks.insert(to.clone());
            if from != to && !from.is_opaque() && !to.is_opaque() {
                let guard = e.guard.iter().map(|g| resolve_lock(g, s, &capture_map)).collect();
                edges.push(Edge { from, to, guard, ..e.clone() });
            }
        }
        for l in &s.acquires {
            all_locks.insert(resolve_lock(&ground(l, &s.class, k), s, &capture_map));
        }
        for (ci, call) in s.calls.iter().enumerate() {
            if call.is_async {
                continue;
            }
            let held: Vec<Lock> = call.held.iter().map(|h| resolve_lock(h, s, &capture_map)).collect();
            // method dependency edge: caller -> callee, labelled by innermost held lock.
            if let Some(inner) = held.last() {
                for callee in &resolved[k][ci] {
                    method_edges.push((k.clone(), inner.name(), callee.clone()));
                }
            }
            for callee in &resolved[k][ci] {
                let Some(callee_may) = may.get(callee) else { continue };
                for cl in callee_may {
                    let Some(sub) = subst_or_self(cl, &call.args) else { continue };
                    let g = resolve_lock(&ground(&sub, &s.class, k), s, &capture_map);
                    all_locks.insert(g.clone());
                    for h in &held {
                        if h != &g && !h.is_opaque() && !g.is_opaque() {
                            let mut guard = held.clone();
                            guard.push(g.clone());
                            edges.push(Edge {
                                from: h.clone(),
                                to: g.clone(),
                                method: k.clone(),
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
    }

    Analysis { edges, all_locks, method_count: by_key.len(), method_edges }
}

// ---------------------------------------------------------------------------
// call graph (CHA + RTA + receiver refinement)
// ---------------------------------------------------------------------------

/// Cap on resolved targets for a virtual/interface call (megamorphic sites).
const RTA_FANOUT_CAP: usize = 24;

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
                // declared target, if it has a body.
                let mut out: Vec<String> = Vec::new();
                let dk = c.declared_key();
                if by_key.contains_key(&dk) {
                    out.push(dk);
                }
                // RTA: instantiated subtypes of the declared class with this name:sig.
                if let Some(cands) = self.methods_by_namesig.get(&c.namesig()) {
                    for cand in cands {
                        let cclass = class_of_key(cand);
                        if self.instantiated.contains(cclass) && self.is_subtype(cclass, &c.dclass) {
                            if !out.contains(cand) {
                                out.push(cand.clone());
                            }
                        }
                        // megamorphic call site: stop resolving (precision lost
                        // anyway, and unbounded fan-out wrecks scale).
                        if out.len() >= RTA_FANOUT_CAP {
                            break;
                        }
                    }
                }
                out
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
fn resolve_lock(lock: &Lock, s: &Summary, capture_map: &HashMap<String, HashMap<String, Lock>>) -> Lock {
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
    ground(&cur, &s.class, &s.key)
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
) -> HashMap<String, Vec<Lock>> {
    // seed with concrete (non-opaque) acquired locks only; opaque locks can never
    // form a cycle, and propagating them (esp. truncated paths) never converges.
    let mut may: HashMap<String, HashSet<Lock>> = by_key
        .iter()
        .map(|(k, s)| (k.clone(), s.acquires.iter().filter(|l| !l.is_opaque()).cloned().collect()))
        .collect();
    let mut keys: Vec<String> = by_key.keys().cloned().collect();
    keys.sort(); // stable processing order -> reproducible saturation under MAY_CAP
    let mut changed = true;
    let mut iters = 0;
    while changed && iters < 25 {
        changed = false;
        iters += 1;
        for k in &keys {
            if may[k].len() >= MAY_CAP {
                continue;
            }
            let s = &by_key[k];
            let mut add: HashSet<Lock> = HashSet::new();
            for (ci, call) in s.calls.iter().enumerate() {
                if call.is_async {
                    continue;
                }
                for callee in &resolved[k][ci] {
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
            if !add.is_empty() {
                changed = true;
                // deterministic order so the MAY_CAP truncation is reproducible.
                let mut add: Vec<Lock> = add.into_iter().collect();
                add.sort_by(|a, b| a.name().cmp(&b.name()));
                let set = may.get_mut(k).unwrap();
                for l in add {
                    if set.len() >= MAY_CAP {
                        break;
                    }
                    set.insert(l);
                }
            }
        }
    }
    may.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect()
}

// ---------------------------------------------------------------------------
// per-method extraction
// ---------------------------------------------------------------------------

fn extract(m: &Method, value_summaries: &HashMap<String, Lock>) -> Summary {
    let mut s = Summary { key: m.key(), class: m.class.clone(), ..Default::default() };
    let mut regs: HashMap<Reg, Lock> = HashMap::new();
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
            Op::Iput { src, base, class: _, field } => {
                // lambda capture: in a <init>, `iput pSrc, this, f` captures the formal.
                if is_ctor {
                    if let Some(Root::Param(i)) = regs.get(src).map(|l| l.root.clone()) {
                        s.ctor_captures.push((field.clone(), i));
                    } else if regs.get(src).map(|l| matches!(l.root, Root::This)).unwrap_or(false) {
                        s.ctor_captures.push((field.clone(), 0));
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
                match juc::classify(&inv.class, &inv.name) {
                    Some(LockCall::ReadView) => {
                        last_ret = inv.args.first().and_then(|r| regs.get(r)).map(|l| l.with_mode(Mode::Read));
                    }
                    Some(LockCall::WriteView) => {
                        last_ret = inv.args.first().and_then(|r| regs.get(r)).map(|l| l.with_mode(Mode::Write));
                    }
                    Some(LockCall::Acquire) | Some(LockCall::TryAcquire) => {
                        let nb = matches!(juc::classify(&inv.class, &inv.name), Some(LockCall::TryAcquire));
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
                        record_call(&mut s, &regs, inv, &held, line_at(insn.offset), true);
                    }
                    None => {
                        last_ret = value_summaries.get(&inv.key()).and_then(|vs| {
                            subst_or_self(vs, &arg_vals(&regs, inv))
                        });
                        record_call(&mut s, &regs, inv, &held, line_at(insn.offset), false);
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

fn record_call(s: &mut Summary, regs: &HashMap<Reg, Lock>, inv: &Invoke, held: &[Lock], line: Option<u32>, is_async: bool) {
    let recv_type = if matches!(inv.kind, InvokeKind::Virtual | InvokeKind::Interface) {
        inv.args.first().and_then(|r| regs.get(r)).and_then(alloc_type)
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

/// The concrete type behind an allocation-site value, if the path is the bare
/// alloc (so dispatch can be refined to it). The site string encodes nothing
/// about the type, so we keep type via the summary `allocs` table instead — here
/// we only treat a bare `Alloc` root with no fields as a concrete receiver. Type
/// is recovered in the global phase; this returns None and RTA handles dispatch.
fn alloc_type(_l: &Lock) -> Option<String> {
    None
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
