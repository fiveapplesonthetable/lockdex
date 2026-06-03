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

//! Field-race contract test. Each `tests/races/<name>.java` declares what the
//! analyzer must conclude about a field:
//!
//!   // RACE:    <field>  — must be flagged as inconsistently guarded
//!   // NO_RACE: <field>  — must NOT be flagged
//!
//! Committed as prebuilt `.dex`; regenerate with `cargo run --example regen_dex -- races`.

use lockdex::analyze::RaceReport;
use lockdex::juc::AsyncConfig;
use std::path::{Path, PathBuf};

fn headers(src: &str, key: &str) -> Vec<String> {
    let tag = format!("// {key}:");
    src.lines()
        .filter_map(|l| l.trim_start().strip_prefix(&tag).map(|v| v.trim().to_string()))
        .collect()
}

fn flagged(r: &RaceReport, field: &str) -> bool {
    r.fields.iter().any(|f| f.field == field)
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/races");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read races dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    v.sort();
    v
}

#[test]
fn race_contracts() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no race fixtures found");
    let async_cfg = AsyncConfig::default();

    let mut fail = Vec::new();
    for java in &fixtures {
        let name = java.file_stem().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(java).expect("read fixture");
        let dex = java.with_extension("dex");
        assert!(dex.exists(), "{name}: missing {} — run `cargo run --example regen_dex -- races`", dex.display());
        let an = lockdex::analyze_dex(&dex, &async_cfg).unwrap_or_else(|e| panic!("{name}: {e:#}"));

        let got: Vec<&str> = an.races.fields.iter().map(|f| f.field.as_str()).collect();
        for field in headers(&src, "RACE") {
            if !flagged(&an.races, &field) {
                fail.push(format!("{name}: expected RACE on '{field}'; flagged={got:?}"));
            }
        }
        for field in headers(&src, "NO_RACE") {
            if flagged(&an.races, &field) {
                fail.push(format!("{name}: '{field}' must NOT be flagged; flagged={got:?}"));
            }
        }
    }

    assert!(fail.is_empty(), "{} race contract(s) failed:\n{}", fail.len(), fail.join("\n"));
}
