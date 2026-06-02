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

//! Binder IPC boundaries: locks held across a cross-process call.
//!
//! This is not deadlock detection — the peer process is outside our graph. It
//! flags the hazard class instead: a lock held while a thread crosses a Binder
//! boundary invites cross-process deadlock, priority inversion, and ANRs.
//!
//!   * OUTGOING — a method holds a lock at a call site that (transitively) reaches
//!     `IBinder.transact`. The lock is held for the whole duration of the IPC.
//!   * INCOMING — a public method of a Binder server (a class extending
//!     `android.os.Binder`) acquires a lock; a remote caller blocks on it. If that
//!     entry also holds the lock across an outgoing transaction, it is the nested
//!     cross-process pattern that actually deadlocks — flagged `high`.

use super::{canonicalize, ground, PathIndex, RawCall, Summary};
use crate::model::{Dex, Lock, Method, Root};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// A lock with no cross-thread identity: an unresolved monitor (`Opaque`) or one
/// taken on a freshly allocated object (`Alloc`). No other thread — let alone
/// another process — can contend it, so it is not a Binder hazard.
fn is_local(l: &Lock) -> bool {
    matches!(l.root, Root::Opaque(_) | Root::Alloc(_))
}

/// A lock held at a call site that crosses (or reaches) a Binder transaction.
#[derive(Serialize, Clone)]
pub struct OutgoingFinding {
    /// method making the call (a method key `a.b.C.m:(...)V`).
    pub holder: String,
    /// source line of the call site, if known.
    pub line: Option<u32>,
    /// locks held when the boundary is crossed (canonical display names).
    pub held: Vec<String>,
    /// the local call that leads to the transaction (a `Class.method`).
    pub via: String,
    /// call chain from the holder to the method that performs the transaction.
    pub path: Vec<String>,
}

/// A Binder server entry method and the locks a remote caller can make it take.
#[derive(Serialize, Clone)]
pub struct IncomingFinding {
    /// the server method key (callable from another process).
    pub entry: String,
    /// locks it may acquire (canonical display names).
    pub locks: Vec<String>,
    /// true if this entry holds a lock across its *own* outgoing transaction
    /// (the nested cross-process pattern that genuinely deadlocks).
    pub high: bool,
}

#[derive(Serialize, Default)]
pub struct BinderReport {
    pub outgoing: Vec<OutgoingFinding>,
    pub incoming: Vec<IncomingFinding>,
}

/// The classes a Binder server transitively extends. Any subtype of this is an IPC
/// server whose public methods can be invoked from another process.
const BINDER_BASE: &str = "android.os.Binder";

/// Callee classes whose `transact` is the outgoing IPC primitive.
fn is_transact(c: &RawCall) -> bool {
    c.name == "transact"
        && matches!(
            c.dclass.as_str(),
            "android.os.IBinder" | "android.os.BinderProxy" | "android.os.Binder"
        )
}

/// Methods that perform an outgoing `transact` directly.
fn direct_callers(by_key: &HashMap<String, Summary>) -> HashSet<String> {
    by_key
        .iter()
        .filter(|(_, s)| s.calls.iter().any(is_transact))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Methods that reach an outgoing `transact`, directly or through (non-async)
/// callees. A lock held at such a call site is held across the transaction,
/// regardless of depth — so this is a transitive least-fixed-point.
fn reaches_binder(
    seed: &HashSet<String>,
    by_key: &HashMap<String, Summary>,
    resolved: &HashMap<String, Vec<Vec<String>>>,
) -> HashSet<String> {
    let mut reach = seed.clone();
    loop {
        let mut grew = false;
        for (k, s) in by_key {
            if reach.contains(k) {
                continue;
            }
            let Some(rv) = resolved.get(k) else { continue };
            let hit = s.calls.iter().enumerate().any(|(ci, call)| {
                !call.is_async
                    && rv.get(ci).is_some_and(|cs| cs.iter().any(|c| reach.contains(c)))
            });
            if hit {
                reach.insert(k.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    reach
}

/// Shortest call chain `[holder, …, transactor]` to a method that directly performs
/// a transaction, walking only callees that can reach one. Bounded depth.
fn path_to_binder(
    start: &str,
    by_key: &HashMap<String, Summary>,
    resolved: &HashMap<String, Vec<Vec<String>>>,
    reach: &HashSet<String>,
    seed: &HashSet<String>,
) -> Vec<String> {
    use std::collections::VecDeque;
    if seed.contains(start) {
        return vec![start.to_string()];
    }
    let mut q: VecDeque<(String, usize)> = VecDeque::from([(start.to_string(), 0)]);
    let mut prev: HashMap<String, String> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
    while let Some((m, depth)) = q.pop_front() {
        if depth >= 24 {
            continue;
        }
        let (Some(s), Some(rv)) = (by_key.get(&m), resolved.get(&m)) else { continue };
        for (ci, call) in s.calls.iter().enumerate() {
            if call.is_async {
                continue;
            }
            let Some(cs) = rv.get(ci) else { continue };
            for c in cs {
                if !reach.contains(c) || seen.contains(c) {
                    continue;
                }
                prev.insert(c.clone(), m.clone());
                if seed.contains(c) {
                    let mut path = vec![c.clone()];
                    let mut cur = c.clone();
                    while let Some(p) = prev.get(&cur) {
                        path.push(p.clone());
                        cur = p.clone();
                    }
                    path.reverse();
                    return path;
                }
                seen.insert(c.clone());
                q.push_back((c.clone(), depth + 1));
            }
        }
    }
    vec![start.to_string()]
}

/// Canonical display names of the locks held at a call site (drops opaque locks,
/// which carry no identity worth reporting).
fn held_names(call: &RawCall, holder: &Summary, key: &str, alias: &HashMap<String, Lock>) -> Vec<String> {
    let mut names: Vec<String> = call
        .held
        .iter()
        .filter(|l| !is_local(l))
        .map(|l| canonicalize(&ground(l, &holder.class, key), alias).name())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn short_method(key: &str) -> String {
    let cm = key.split(':').next().unwrap_or(key);
    match cm.rfind('.') {
        Some(dot) => {
            let cls = cm[..dot].rsplit('.').next().unwrap_or(&cm[..dot]);
            format!("{cls}.{}", &cm[dot + 1..])
        }
        None => cm.to_string(),
    }
}

/// Find the locks-across-IPC findings for the whole component.
pub(super) fn compute(
    dex: &Dex,
    supertypes: &HashMap<String, HashSet<String>>,
    by_key: &HashMap<String, Summary>,
    resolved: &HashMap<String, Vec<Vec<String>>>,
    paths: &PathIndex,
    alias: &HashMap<String, Lock>,
) -> BinderReport {
    let seed = direct_callers(by_key);
    let reach = reaches_binder(&seed, by_key, resolved);

    // --- outgoing: a lock held at a call site that reaches a transaction --------
    let mut outgoing: Vec<OutgoingFinding> = Vec::new();
    let mut keys: Vec<&String> = by_key.keys().collect();
    keys.sort();
    for k in keys {
        let s = &by_key[k];
        for (ci, call) in s.calls.iter().enumerate() {
            if call.is_async || call.held.is_empty() {
                continue;
            }
            let callees = resolved.get(k).and_then(|rv| rv.get(ci));
            let via_reach = callees.and_then(|cs| cs.iter().find(|c| reach.contains(*c)));
            let crosses = is_transact(call) || via_reach.is_some();
            if !crosses {
                continue;
            }
            let held = held_names(call, s, k, alias);
            if held.is_empty() {
                continue;
            }
            let via = via_reach
                .map(|c| short_method(c))
                .unwrap_or_else(|| "IBinder.transact".to_string());
            let path = path_to_binder(k, by_key, resolved, &reach, &seed);
            outgoing.push(OutgoingFinding { holder: k.clone(), line: call.line, held, via, path });
        }
    }
    outgoing.sort_by(|a, b| {
        (&a.holder, a.line, &a.held).cmp(&(&b.holder, b.line, &b.held))
    });
    outgoing.dedup_by(|a, b| a.holder == b.holder && a.line == b.line && a.held == b.held);

    let holds_across: HashSet<&str> = outgoing.iter().map(|f| f.holder.as_str()).collect();

    // --- incoming: public methods of Binder servers that take locks -------------
    let mut incoming: Vec<IncomingFinding> = Vec::new();
    for class in &dex.classes {
        let is_server = supertypes
            .get(&class.descriptor)
            .is_some_and(|s| s.contains(BINDER_BASE));
        if !is_server {
            continue;
        }
        for m in &class.methods {
            if !is_entry(m) {
                continue;
            }
            let key = m.key();
            let mut locks: Vec<String> = paths
                .may
                .get(&key)
                .map(|ls| ls.iter().filter(|l| !is_local(l)).map(|l| l.name()).collect())
                .unwrap_or_default();
            locks.sort();
            locks.dedup();
            if locks.is_empty() {
                continue;
            }
            let high = holds_across.contains(key.as_str());
            incoming.push(IncomingFinding { entry: key, locks, high });
        }
    }
    incoming.sort_by(|a, b| (!a.high, &a.entry).cmp(&(!b.high, &b.entry)));

    BinderReport { outgoing, incoming }
}

/// A method a remote process can invoke: public, instance, not a constructor, the
/// `onTransact` dispatcher, or a compiler-synthesized helper (lambda bodies, access
/// thunks, nest bridges — these are not part of the AIDL surface).
fn is_entry(m: &Method) -> bool {
    const ACC_PUBLIC: u32 = 0x1;
    const ACC_STATIC: u32 = 0x8;
    let synthetic = m.name.starts_with("lambda$")
        || m.name.starts_with("access$")
        || m.name.starts_with("-$$")
        || m.name.contains("$$Nest");
    m.access & ACC_PUBLIC != 0
        && m.access & ACC_STATIC == 0
        && m.name != "<init>"
        && m.name != "<clinit>"
        && m.name != "onTransact"
        && !synthetic
}
