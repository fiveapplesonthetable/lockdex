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

//! Per-method extraction. A linear pass resolves register values, lock identities,
//! and value summaries, recording each monitor/lock *effect* and each held-set
//! consumer (acquire, call, field access) by instruction index. A control-flow
//! must-hold dataflow then computes the locks held *before* every instruction —
//! intersecting over real predecessors, so an early-exit `monitor-exit` on one
//! branch no longer leaks onto the fall-through. Edges and the held-sets attached
//! to calls and field accesses are emitted against that dataflow result.

use super::{ground, subst_or_self, Edge, FieldAccess, RawCall, Summary};
use crate::juc::{self, LockCall};
use crate::model::*;
use std::collections::HashMap;

/// What an instruction does to the held-lock multiset: enter or exit a lock.
enum Effect {
    Enter(Lock),
    Exit(Lock),
}

/// A held-set consumer, deferred until the dataflow knows the held-set at its site.
enum Event {
    /// a monitor / `Lock.lock` acquisition: emits order edges + first/all-acquires.
    Acquire { lock: Lock, grounded: Lock, line: Option<u32>, nonblocking: bool },
    /// a call whose held-set must be recorded for the global phase.
    Call(RawCall),
    /// a field read/write whose held-set feeds the race analysis.
    Field { key: String, write: bool, line: Option<u32> },
}

pub(super) fn extract(m: &Method, value_summaries: &HashMap<String, Lock>, cfg: &juc::AsyncConfig) -> Summary {
    const ACC_PUBLIC: u32 = 0x1;
    const ACC_PROTECTED: u32 = 0x4;
    let external = m.access & (ACC_PUBLIC | ACC_PROTECTED) != 0;
    let mut s = Summary { key: m.key(), class: m.class.clone(), external, ..Default::default() };
    let mut regs: HashMap<Reg, Lock> = HashMap::new();
    let mut alloc_ty: HashMap<String, String> = HashMap::new();
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

    let opaque = |off: u32| Lock::new(Root::Opaque(format!("{}+{:04x}", m.key(), off)));
    let g = |l: &Lock| ground(l, &m.class, &m.key());
    let record_field = field_recorder(m);

    // --- linear pass: register values, lock effects, deferred events ------------
    let n = m.insns.len();
    let mut effect: Vec<Option<Effect>> = (0..n).map(|_| None).collect();
    let mut events: Vec<(usize, Event)> = Vec::new();

    for (i, insn) in m.insns.iter().enumerate() {
        let line = m.line_at(insn.offset);
        match &insn.op {
            Op::Sget { dst, class, field } => {
                regs.insert(*dst, Lock::field(Root::Static(class.clone()), field.clone()));
            }
            Op::Iget { dst, base, class, field } => {
                let fresh = regs.get(base).is_some_and(|l| matches!(l.root, Root::Alloc(_)));
                regs.insert(*dst, Lock::field(Root::Recv(class.clone()), field.clone()));
                if !fresh {
                    if let Some(key) = record_field(class, field) {
                        events.push((i, Event::Field { key, write: false, line }));
                    }
                }
            }
            Op::Iput { src, base, class, field } => {
                if is_ctor {
                    // Key the capture by the field's *declaring* class (which may be a
                    // superclass), so it threads correctly through `super(...)`.
                    let fkey = format!("{class}.{field}");
                    if let Some(Root::Param(idx)) = regs.get(src).map(|l| l.root.clone()) {
                        s.ctor_captures.push((fkey, idx));
                    } else if regs.get(src).map(|l| matches!(l.root, Root::This)).unwrap_or(false) {
                        s.ctor_captures.push((fkey, 0));
                    }
                    if let Some(v) = regs.get(src) {
                        if matches!(v.root, Root::Recv(_) | Root::Static(_)) && !v.fields.is_empty() {
                            let key = format!("{}.{}", class, field);
                            if v.name() != key {
                                s.field_aliases.push((key, v.clone()));
                            }
                        }
                    }
                }
                if let Some(Root::Alloc(site)) = regs.get(src).map(|l| l.root.clone()) {
                    if let Some(ty) = alloc_ty.get(&site) {
                        s.alloc_stores.push((format!("{class}.{field}"), ty.clone()));
                    }
                }
                let fresh = regs.get(base).is_some_and(|l| matches!(l.root, Root::Alloc(_)));
                if !fresh {
                    if let Some(key) = record_field(class, field) {
                        events.push((i, Event::Field { key, write: true, line }));
                    }
                }
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
                if inv.name == "<init>" {
                    if let Some(r0) = inv.args.first() {
                        match regs.get(r0).map(|l| &l.root) {
                            Some(Root::Alloc(site)) => {
                                s.alloc_inits.push((site.clone(), inv.key(), arg_vals(&regs, inv)));
                            }
                            // `super(...)` / `this(...)`: chained ctor on the receiver.
                            Some(Root::This) if is_ctor && s.super_init.is_none() => {
                                let map = inv
                                    .args
                                    .iter()
                                    .map(|r| match regs.get(r).map(|l| &l.root) {
                                        Some(Root::This) => Some(0),
                                        Some(Root::Param(j)) => Some(*j),
                                        _ => None,
                                    })
                                    .collect();
                                s.super_init = Some((inv.key(), map));
                            }
                            _ => {}
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
                    Some(call @ (LockCall::Acquire | LockCall::TryAcquire)) => {
                        let nonblocking = matches!(call, LockCall::TryAcquire);
                        let lock = inv.args.first().and_then(|r| regs.get(r)).cloned().unwrap_or_else(|| opaque(insn.offset));
                        let grounded = g(&lock);
                        effect[i] = Some(Effect::Enter(grounded.clone()));
                        events.push((i, Event::Acquire { lock, grounded, line, nonblocking }));
                    }
                    Some(LockCall::Release) => {
                        if let Some(lock) = inv.args.first().and_then(|r| regs.get(r)) {
                            effect[i] = Some(Effect::Exit(g(lock)));
                        }
                    }
                    Some(LockCall::AsyncDispatch) => {
                        events.push((i, Event::Call(raw_call(&regs, inv, true, line, &alloc_ty))));
                    }
                    None => {
                        last_ret = value_summaries.get(&inv.key()).and_then(|vs| subst_or_self(vs, &arg_vals(&regs, inv)));
                        events.push((i, Event::Call(raw_call(&regs, inv, false, line, &alloc_ty))));
                    }
                }
            }
            Op::MonitorEnter(r) => {
                let lock = regs.get(r).cloned().unwrap_or_else(|| opaque(insn.offset));
                let grounded = g(&lock);
                effect[i] = Some(Effect::Enter(grounded.clone()));
                events.push((i, Event::Acquire { lock, grounded, line, nonblocking: false }));
            }
            Op::MonitorExit(r) => {
                if let Some(lock) = regs.get(r) {
                    effect[i] = Some(Effect::Exit(g(lock)));
                }
            }
            Op::Goto(_) | Op::Branch(_) | Op::Throw | Op::Other => {}
        }
    }

    // --- control-flow must-hold dataflow ----------------------------------------
    // No entry seed: d8 lowers `synchronized` methods to an explicit
    // `monitor-enter` on the receiver (or the class, for static), so the implicit
    // monitor is already in the instruction stream.
    let held_in = held_dataflow(m, &effect, Vec::new());

    // --- emit edges / calls / field accesses against the held-set ---------------
    for (i, ev) in events {
        let held = &held_in[i];
        match ev {
            Event::Acquire { lock, grounded, line, nonblocking } => {
                emit_acquire(&mut s, m, held, lock, grounded, line, nonblocking);
            }
            Event::Call(mut rc) => {
                rc.held = held.clone();
                s.calls.push(rc);
            }
            Event::Field { key, write, line } => {
                s.field_access.push(FieldAccess { field: key, write, line, held: held.clone() });
            }
        }
    }

    if let Some(Some(first)) = returns.first().cloned() {
        if returns.iter().all(|r| r.as_ref() == Some(&first)) && simple_value(&first) {
            s.value_summary = Some(first);
        }
    }
    s
}

/// Locks held *before* each instruction. Forward must-analysis over the CFG: a
/// block's entry held-set is the intersection over its predecessors' exit sets, so
/// a lock is held only if held on every path. Counts model reentrancy (a nested
/// `synchronized` on the same lock); `return`/`throw` end a path.
fn held_dataflow(m: &Method, effect: &[Option<Effect>], seed: Vec<Lock>) -> Vec<Vec<Lock>> {
    let n = m.insns.len();
    if n == 0 {
        return Vec::new();
    }
    let index: HashMap<u32, usize> = m.insns.iter().enumerate().map(|(i, ins)| (ins.offset, i)).collect();
    let mut succ: Vec<Vec<usize>> = (0..n)
        .map(|i| match &m.insns[i].op {
            Op::Return(_) | Op::Throw => Vec::new(),
            Op::Goto(t) => index.get(t).copied().into_iter().collect(),
            Op::Branch(t) => {
                let mut v: Vec<usize> = index.get(t).copied().into_iter().collect();
                if i + 1 < n {
                    v.push(i + 1);
                }
                v
            }
            _ if i + 1 < n => vec![i + 1],
            _ => Vec::new(),
        })
        .collect();
    // Exception edges: any instruction in a try range can branch to its handler, so
    // the handler's held-set is the meet over the try range — a lock held across the
    // whole `try` is still held in the `catch`.
    for &(start, end, handler) in &m.catches {
        let Some(&h) = index.get(&handler) else { continue };
        for i in 0..n {
            let off = m.insns[i].offset;
            if off >= start && off < end && !succ[i].contains(&h) {
                succ[i].push(h);
            }
        }
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ss) in succ.iter().enumerate() {
        for &t in ss {
            preds[t].push(i);
        }
    }

    // `None` is ⊤ (unconstrained): the identity for intersection, so an unreached
    // predecessor never lowers a block's held-set.
    type Counts = HashMap<Lock, u32>;
    let apply = |set: &Counts, eff: &Option<Effect>| -> Counts {
        let mut o = set.clone();
        match eff {
            Some(Effect::Enter(l)) => *o.entry(l.clone()).or_insert(0) += 1,
            Some(Effect::Exit(l)) => {
                if let Some(c) = o.get_mut(l) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        o.remove(l);
                    }
                }
            }
            None => {}
        }
        o
    };
    let meet = |a: Option<Counts>, b: Counts| -> Counts {
        match a {
            None => b,
            Some(a) => a.iter().filter_map(|(l, &ca)| b.get(l).map(|&cb| (l.clone(), ca.min(cb)))).collect(),
        }
    };

    let mut entry: Vec<Option<Counts>> = vec![None; n];
    let mut seeded: Counts = HashMap::new();
    for l in seed {
        *seeded.entry(l).or_insert(0) += 1;
    }
    entry[0] = Some(seeded);

    // Iterate to a fixed point (bounded; reducible CFGs converge in a few passes).
    for _ in 0..n.min(200) + 1 {
        let mut changed = false;
        for i in 1..n {
            if preds[i].is_empty() {
                continue;
            }
            let mut acc: Option<Counts> = None;
            for &p in &preds[i] {
                if let Some(set) = &entry[p] {
                    acc = Some(meet(acc, apply(set, &effect[p])));
                }
            }
            if acc.is_some() && acc != entry[i] {
                entry[i] = acc;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    entry
        .into_iter()
        .map(|set| set.map(|c| c.into_keys().collect()).unwrap_or_default())
        .collect()
}

/// A field-access recorder closed over the method's eligibility: constructor and
/// compiler-synthesized methods are skipped (pre-publication / desugaring noise).
/// Returns the `Class.field` key when an access there should be recorded.
fn field_recorder(m: &Method) -> impl Fn(&str, &str) -> Option<String> + '_ {
    let skip = m.name == "<init>"
        || m.name == "<clinit>"
        || m.name.starts_with("lambda$")
        || m.name.starts_with("access$")
        || m.name.starts_with("-$$")
        || m.name.contains("$$Nest");
    move |class: &str, field: &str| (!skip).then(|| format!("{class}.{field}"))
}

fn simple_value(l: &Lock) -> bool {
    matches!(
        l.root,
        Root::This | Root::Param(_) | Root::Recv(_) | Root::Static(_) | Root::ClassConst(_)
    )
}

fn arg_vals(regs: &HashMap<Reg, Lock>, inv: &Invoke) -> Vec<Option<Lock>> {
    inv.args.iter().map(|r| regs.get(r).cloned()).collect()
}

/// Build a `RawCall` with an empty held-set (filled in once the dataflow runs).
fn raw_call(regs: &HashMap<Reg, Lock>, inv: &Invoke, is_async: bool, line: Option<u32>, alloc_ty: &HashMap<String, String>) -> RawCall {
    let recv_type = if matches!(inv.kind, InvokeKind::Virtual | InvokeKind::Interface) {
        inv.args.first().and_then(|r| regs.get(r)).and_then(|l| alloc_type(l, alloc_ty))
    } else {
        None
    };
    RawCall {
        kind: inv.kind,
        dclass: inv.class.clone(),
        name: inv.name.clone(),
        sig: inv.sig.clone(),
        recv_type,
        args: arg_vals(regs, inv),
        held: Vec::new(),
        line,
        is_async,
    }
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

/// Emit the order edges for a lock acquired with `held` already held. Reentrant
/// re-acquisition of a held lock adds no edge. The dataflow owns the held-set, so
/// this only records facts.
fn emit_acquire(s: &mut Summary, m: &Method, held: &[Lock], lock: Lock, grounded: Lock, line: Option<u32>, nonblocking: bool) {
    if held.contains(&grounded) {
        return; // reentrant
    }
    if held.is_empty() {
        s.first_acquire.push(lock.clone());
    }
    s.acquires.push(lock);
    if nonblocking {
        return;
    }
    for h in held {
        if h != &grounded && !h.is_opaque() && !grounded.is_opaque() {
            let mut guard = held.to_vec();
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
