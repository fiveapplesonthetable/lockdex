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

//! Binder-boundary contract test. Each `tests/binder/<name>.java` declares what the
//! analyzer must find about locks across Binder IPC, in header comments:
//!
//!   // OUT:    <lock>   — held across an outgoing transaction
//!   // NO_OUT: <lock>   — must NOT be reported held across an outgoing transaction
//!   // IN:     <lock>   — acquired by an incoming server entry
//!   // HIGH:   <substr> — an incoming entry (matching substr) flagged high-risk
//!
//! Fixtures compile against a fake `android.os` package (see `tests/binder/support`)
//! and are committed as prebuilt `.dex`; regenerate with
//! `cargo run --example regen_dex -- binder`. `dexdump` must be discoverable.

use lockdex::analyze::BinderReport;
use lockdex::juc::AsyncConfig;
use std::path::{Path, PathBuf};

fn headers(src: &str, key: &str) -> Vec<String> {
    let tag = format!("// {key}:");
    src.lines()
        .filter_map(|l| l.trim_start().strip_prefix(&tag).map(|v| v.trim().to_string()))
        .collect()
}

fn held_across(b: &BinderReport, lock: &str) -> bool {
    b.outgoing.iter().any(|f| f.held.iter().any(|h| h == lock))
}

fn check(name: &str, src: &str, b: &BinderReport, fail: &mut Vec<String>) {
    for lock in headers(src, "OUT") {
        if !held_across(b, &lock) {
            fail.push(format!("{name}: expected '{lock}' held across an outgoing txn; outgoing={:?}", dump_out(b)));
        }
    }
    for lock in headers(src, "NO_OUT") {
        if held_across(b, &lock) {
            fail.push(format!("{name}: '{lock}' must NOT be held across an outgoing txn; outgoing={:?}", dump_out(b)));
        }
    }
    for lock in headers(src, "IN") {
        if !b.incoming.iter().any(|f| f.locks.iter().any(|l| l == &lock)) {
            fail.push(format!("{name}: expected incoming entry acquiring '{lock}'; incoming={:?}", dump_in(b)));
        }
    }
    for sub in headers(src, "HIGH") {
        if !b.incoming.iter().any(|f| f.high && f.entry.contains(&sub)) {
            fail.push(format!("{name}: expected a high-risk incoming entry matching '{sub}'; incoming={:?}", dump_in(b)));
        }
    }
}

fn dump_out(b: &BinderReport) -> Vec<(String, Vec<String>)> {
    b.outgoing.iter().map(|f| (f.holder.clone(), f.held.clone())).collect()
}
fn dump_in(b: &BinderReport) -> Vec<(String, bool, Vec<String>)> {
    b.incoming.iter().map(|f| (f.entry.clone(), f.high, f.locks.clone())).collect()
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/binder");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read binder dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    v.sort();
    v
}

#[test]
fn binder_contracts() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no binder fixtures found");
    let async_cfg = AsyncConfig::default();

    let mut fail = Vec::new();
    for java in &fixtures {
        let name = java.file_stem().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(java).expect("read fixture");
        let dex = java.with_extension("dex");
        assert!(dex.exists(), "{name}: missing {} — run `cargo run --example regen_dex -- binder`", dex.display());
        let an = lockdex::analyze_dex(&dex, &async_cfg).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        check(&name, &src, &an.binder, &mut fail);
    }

    assert!(fail.is_empty(), "{} binder contract(s) failed:\n{}", fail.len(), fail.join("\n"));
}
