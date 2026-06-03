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

//! Per-method extraction: a single forward pass that resolves each acquire to a
//! lock identity, tracks the held-lock stack, and records the method summary.

use super::{ground, subst_or_self, Edge, FieldAccess, RawCall, Summary};
use crate::juc::{self, LockCall};
use crate::model::*;
use std::collections::HashMap;

pub(super) fn extract(m: &Method, value_summaries: &HashMap<String, Lock>, cfg: &juc::AsyncConfig) -> Summary {
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
                record_field(&mut s, m, class, field, false, &held, line_at(insn.offset));
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
                // `field = new T(...)`: remember the store so a singleton's monitor
                // can be unified with `owner.field` later.
                if let Some(Root::Alloc(site)) = regs.get(src).map(|l| l.root.clone()) {
                    if let Some(ty) = alloc_ty.get(&site) {
                        s.alloc_stores.push((format!("{class}.{field}"), ty.clone()));
                    }
                }
                record_field(&mut s, m, class, field, true, &held, line_at(insn.offset));
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
                    Some(call @ (LockCall::Acquire | LockCall::TryAcquire)) => {
                        let nonblocking = matches!(call, LockCall::TryAcquire);
                        let lock = inv.args.first().and_then(|r| regs.get(r)).cloned()
                            .unwrap_or_else(|| Lock::new(Root::Opaque(format!("{}+{:04x}", m.key(), insn.offset))));
                        acquire(&mut s, m, &mut held, lock, line_at(insn.offset), nonblocking);
                    }
                    Some(LockCall::Release) => {
                        if let Some(lock) = inv.args.first().and_then(|r| regs.get(r)).cloned() {
                            release(&mut held, &ground(&lock, &m.class, &m.key()));
                        } else if !held.is_empty() {
                            held.pop();
                        }
                    }
                    Some(LockCall::AsyncDispatch) => {
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

/// Record one field read/write with the locks held at that point, for the
/// field-race analysis. Skipped for constructors (`<init>`/`<clinit>` writes are
/// pre-publication) and compiler-synthesized methods (lambda bodies, nest-access
/// bridges — their held-set is an artifact of desugaring, not real locking).
fn record_field(s: &mut Summary, m: &Method, class: &str, field: &str, write: bool, held: &[Lock], line: Option<u32>) {
    let synthetic = m.name.starts_with("lambda$")
        || m.name.starts_with("access$")
        || m.name.starts_with("-$$")
        || m.name.contains("$$Nest");
    if m.name == "<init>" || m.name == "<clinit>" || synthetic {
        return;
    }
    s.field_access.push(FieldAccess { field: format!("{class}.{field}"), write, line, held: held.to_vec() });
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
