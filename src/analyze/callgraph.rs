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

//! RTA call graph: virtual/interface dispatch resolved by CHA pruned to
//! instantiated types, plus receiver-type refinement and megamorphic drop.

use super::{RawCall, Summary};
use crate::model::*;
use std::collections::{HashMap, HashSet};


/// A virtual/interface site with more than this many instantiated candidate
/// targets is treated as *megamorphic*: we resolve it to the declared target only
/// (if that has a body), otherwise to nothing. Dropping the fan-out is sound — an
/// unresolved call adds no edges, so we miss rather than fabricate — and it stops
/// one spurious dispatch target from welding unrelated lock clusters into a single
/// giant SCC. Small (mono/poly-morphic) sites are resolved precisely.
pub(super) const POLY_LIMIT: usize = 4;

pub(super) struct CallGraph {
    pub(super) methods_by_namesig: HashMap<String, Vec<String>>,
    pub(super) instantiated: HashSet<String>,
    pub(super) supertypes: HashMap<String, HashSet<String>>,
}

impl CallGraph {
    fn is_subtype(&self, sub: &str, sup: &str) -> bool {
        sub == sup || self.supertypes.get(sub).map(|s| s.contains(sup)).unwrap_or(false)
    }

    pub(super) fn resolve(&self, c: &RawCall, by_key: &HashMap<String, Summary>) -> Vec<String> {
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

pub(super) fn class_of_key(key: &str) -> &str {
    // key = "a.b.C.method:sig"  -> class "a.b.C"
    let pre = key.split(':').next().unwrap_or(key);
    match pre.rfind('.') {
        Some(i) => &pre[..i],
        None => pre,
    }
}

pub(super) fn build_supertypes(dex: &Dex) -> HashMap<String, HashSet<String>> {
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

pub(super) fn index_namesig(by_key: &HashMap<String, Summary>) -> HashMap<String, Vec<String>> {
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
