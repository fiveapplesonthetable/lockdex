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

//! Field-race detection by guard reconstruction (RacerD-style).
//!
//! For each field we look at the locks held on its reads and writes. A field whose
//! writes are *consistently* guarded by one lock `L`, except at a few sites, is one
//! `L` is meant to protect — and those few sites are the suspected races.
//!
//! The held-set at an access is interprocedural: a field written inside a helper is
//! guarded if the helper is *always* reached under the lock. That "always held on
//! entry" set is a meet (intersection) over a method's callers — computed by
//! [`must_entry`] — added to the locks held locally at the access.
//!
//! `final` / `volatile` fields and constructor writes are excluded (write-once,
//! lock-free, or pre-publication — none are races).

use super::{canonicalize, ground, is_local_lock, Summary};
use crate::model::Lock;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// One access of a field that does not hold the field's inferred guard lock.
#[derive(Serialize, Clone)]
pub struct Violation {
    pub method: String,
    pub line: Option<u32>,
    pub write: bool,
}

/// A field that is usually guarded by one lock, but not always.
#[derive(Serialize, Clone)]
pub struct FieldRace {
    /// `DeclaringClass.field`.
    pub field: String,
    /// the lock that guards most of its writes — the inferred guard.
    pub guard: String,
    pub writes: usize,
    pub reads: usize,
    /// writes that do hold `guard`.
    pub guarded_writes: usize,
    /// accesses that do not hold `guard` (the suspected races), writes first.
    pub violations: Vec<Violation>,
}

#[derive(Serialize, Default)]
pub struct RaceReport {
    pub fields: Vec<FieldRace>,
}

/// Violations beyond this many per field are dropped from the report (the counts
/// stay accurate).
const MAX_VIOLATIONS: usize = 16;

/// Grounded, canonical names of the shared locks held at an access (drops
/// alloc-site / opaque locks, which cannot guard shared state).
fn guard_names(held: &[Lock], class: &str, key: &str, alias: &HashMap<String, Lock>) -> HashSet<String> {
    held.iter()
        .filter(|l| !is_local_lock(l))
        .map(|l| canonicalize(&ground(l, class, key), alias).name())
        .collect()
}

/// Locks guaranteed held whenever a method runs, by meet (intersection) over every
/// caller of `held-at-the-call ∪ must_entry(caller)`. A method with no callers in
/// the component (a public entry, a thread/Binder root) is unconstrained — `{}`.
pub(super) fn must_entry(
    by_key: &HashMap<String, Summary>,
    resolved: &HashMap<String, Vec<Vec<String>>>,
    alias: &HashMap<String, Lock>,
) -> HashMap<String, HashSet<String>> {
    // caller -> [(callee, locks held at that call site)]
    let mut edges: HashMap<&str, Vec<(String, HashSet<String>)>> = HashMap::new();
    let mut has_caller: HashSet<String> = HashSet::new();
    for (k, s) in by_key {
        let Some(rv) = resolved.get(k) else { continue };
        for (ci, call) in s.calls.iter().enumerate() {
            if call.is_async {
                continue;
            }
            let Some(callees) = rv.get(ci) else { continue };
            let held = guard_names(&call.held, &s.class, k, alias);
            for c in callees {
                if by_key.contains_key(c) {
                    edges.entry(k).or_default().push((c.clone(), held.clone()));
                    has_caller.insert(c.clone());
                }
            }
        }
    }

    // `None` = ⊤ (not yet constrained). A method is a root (held-set `{}`) if it has
    // no in-component caller OR is externally callable (public/protected) — such a
    // method can be entered from outside with no lock held, so nothing is guaranteed.
    let mut must: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    let mut work: Vec<String> = Vec::new();
    for (k, s) in by_key {
        if has_caller.contains(k) && !s.external {
            must.insert(k.clone(), None);
        } else {
            must.insert(k.clone(), Some(HashSet::new()));
            work.push(k.clone());
        }
    }

    while let Some(m) = work.pop() {
        let Some(Some(mset)) = must.get(&m).cloned() else { continue };
        let Some(out) = edges.get(m.as_str()) else { continue };
        for (callee, held) in out {
            let contrib: HashSet<String> = mset.iter().chain(held).cloned().collect();
            let (next, changed) = match must.get(callee).cloned().flatten() {
                None => (contrib, true),
                Some(existing) => {
                    let inter: HashSet<String> = existing.intersection(&contrib).cloned().collect();
                    let changed = inter.len() != existing.len();
                    (inter, changed)
                }
            };
            if changed {
                must.insert(callee.clone(), Some(next));
                work.push(callee.clone());
            }
        }
    }

    must.into_iter().map(|(k, v)| (k, v.unwrap_or_default())).collect()
}

#[derive(Default)]
struct Stat {
    writes: usize,
    reads: usize,
    write_guard: HashMap<String, usize>,
    read_guard: HashMap<String, usize>,
}

/// Reconstruct per-field guards and the accesses that violate them.
pub(super) fn compute(
    by_key: &HashMap<String, Summary>,
    alias: &HashMap<String, Lock>,
    entry: &HashMap<String, HashSet<String>>,
    excluded_fields: &HashSet<String>,
) -> RaceReport {

    // The effective guard set at an access: locks held locally ∪ guaranteed-on-entry.
    let effective = |fa_held: &[Lock], class: &str, key: &str| -> HashSet<String> {
        let mut g = guard_names(fa_held, class, key, alias);
        if let Some(e) = entry.get(key) {
            g.extend(e.iter().cloned());
        }
        g
    };

    // Pass 1: tally, per field, how often each lock guards its writes and reads.
    let mut stats: HashMap<&str, Stat> = HashMap::new();
    for (k, s) in by_key {
        for fa in &s.field_access {
            if excluded_fields.contains(&fa.field) {
                continue;
            }
            let guards = effective(&fa.held, &s.class, k);
            let st = stats.entry(fa.field.as_str()).or_default();
            let (total, by_lock) = if fa.write {
                (&mut st.writes, &mut st.write_guard)
            } else {
                (&mut st.reads, &mut st.read_guard)
            };
            *total += 1;
            for g in guards {
                *by_lock.entry(g).or_insert(0) += 1;
            }
        }
    }

    // A field is racy if one lock guards a majority of its (≥2) writes but some
    // access still misses it. Pick that dominant lock as the inferred guard.
    // Every field with a dominant write-guard and at least one access that misses it
    // is a candidate; the coverage baseline (what fraction of writes must hold the
    // guard) is a presentation choice applied by the reporter, so the user can tune
    // it. Sorted tie-breaks keep the chosen guard deterministic.
    let mut guard_of: HashMap<String, String> = HashMap::new();
    for (&field, st) in &stats {
        if st.writes == 0 {
            continue;
        }
        let Some((guard, &gcount)) =
            st.write_guard.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        else {
            continue;
        };
        let read_guarded = st.read_guard.get(guard).copied().unwrap_or(0);
        let misses = (st.writes - gcount) + (st.reads - read_guarded);
        if misses > 0 {
            guard_of.insert(field.to_string(), guard.clone());
        }
    }

    // Pass 2: collect the violating sites for the racy fields.
    let mut violations: HashMap<&str, Vec<Violation>> = HashMap::new();
    for (k, s) in by_key {
        for fa in &s.field_access {
            let Some(guard) = guard_of.get(&fa.field) else { continue };
            if !effective(&fa.held, &s.class, k).contains(guard) {
                violations
                    .entry(fa.field.as_str())
                    .or_default()
                    .push(Violation { method: k.clone(), line: fa.line, write: fa.write });
            }
        }
    }

    let mut fields: Vec<FieldRace> = guard_of
        .iter()
        .map(|(field, guard)| {
            let st = &stats[field.as_str()];
            let mut v = violations.remove(field.as_str()).unwrap_or_default();
            // writes first, then by method/line; cap the list.
            v.sort_by(|a, b| (!a.write, &a.method, a.line).cmp(&(!b.write, &b.method, b.line)));
            v.dedup_by(|a, b| a.method == b.method && a.line == b.line && a.write == b.write);
            v.truncate(MAX_VIOLATIONS);
            FieldRace {
                field: field.clone(),
                guard: guard.clone(),
                writes: st.writes,
                reads: st.reads,
                guarded_writes: *st.write_guard.get(guard).unwrap_or(&0),
                violations: v,
            }
        })
        .collect();

    // Most write-violations first (clearest write/write races), then by name.
    fields.sort_by(|a, b| {
        let wa = a.violations.iter().filter(|v| v.write).count();
        let wb = b.violations.iter().filter(|v| v.write).count();
        wb.cmp(&wa).then_with(|| a.field.cmp(&b.field))
    });
    RaceReport { fields }
}
