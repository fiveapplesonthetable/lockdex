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

//! Corpus contract test. Each `tests/corpus/<name>.java` is a minimal fixture that
//! declares its expected verdict in header comments:
//!
//!   // EXPECT: DEADLOCK | NO_DEADLOCK
//!   // CYCLE:  space-separated lock names that must co-occur in one reported cycle
//!
//! The `.java` is the readable source of truth; the analyzer runs against the
//! sibling prebuilt `<name>.dex`. The dex is committed so the test runs in-process
//! with no Java toolchain; lockdex reads it through `dexdump`, which must be
//! discoverable (set `$LOCKDEX_DEXDUMP` or rely on the AOSP host path). Regenerate
//! the dex after editing a fixture with `cargo run --example regen_dex`.

use lockdex::juc::SinkConfig;
use lockdex::report::JsonReport;
use std::path::{Path, PathBuf};

fn header(src: &str, key: &str) -> Option<String> {
    let tag = format!("// {key}:");
    src.lines()
        .find_map(|l| l.trim_start().strip_prefix(&tag))
        .map(|v| v.trim().to_string())
}

fn ok(expect: &str, want: &[String], rep: &JsonReport) -> bool {
    let present = |w: &[String]| rep.cycles.iter().any(|c| w.iter().all(|x| c.locks.contains(x)));
    match expect {
        // A named cycle must be present; with no name, any cycle satisfies.
        "DEADLOCK" if want.is_empty() => !rep.cycles.is_empty(),
        "DEADLOCK" => present(want),
        // A named cycle must be absent; with no name, there must be no cycles at all.
        "NO_DEADLOCK" if want.is_empty() => rep.cycles.is_empty(),
        "NO_DEADLOCK" => !present(want),
        other => panic!("unknown EXPECT verdict: {other:?}"),
    }
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    v.sort();
    v
}

#[test]
fn corpus_contracts() {
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "no .java fixtures found");
    let sinks = SinkConfig::default();

    let mut failures = Vec::new();
    for java in &fixtures {
        let name = java.file_stem().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(java).expect("read fixture");
        let expect = header(&src, "EXPECT").unwrap_or_else(|| panic!("{name}: missing // EXPECT"));
        let want: Vec<String> =
            header(&src, "CYCLE").map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default();

        let dex = java.with_extension("dex");
        assert!(dex.exists(), "{name}: missing {} — run `cargo run --example regen_dex`", dex.display());

        let rep = lockdex::report_for_dex(&dex, &sinks).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        if !ok(&expect, &want, &rep) {
            let got: Vec<&Vec<String>> = rep.cycles.iter().map(|c| &c.locks).collect();
            failures.push(format!("{name}: expect={expect} cycle={want:?} got={got:?}"));
        }
    }

    assert!(failures.is_empty(), "{} fixture(s) failed:\n{}", failures.len(), failures.join("\n"));
}
