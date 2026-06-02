//! The mayAcquire fixpoint: transitive locks reachable through the call graph,
//! computed in parallel Jacobi rounds with a per-method saturation cap.

use super::{subst_or_self, Summary};
use crate::model::Lock;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

/// Per-method cap on the mayAcquire set. Beyond this a method is "saturated" and
/// stops growing — sound (under-approximation) and keeps the fixpoint near-linear.
const MAY_CAP: usize = 96;

pub(super) fn may_acquire(
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
