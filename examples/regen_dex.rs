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

//! Regenerate the prebuilt `.dex` corpus inputs from the `.java` fixtures.
//!
//! The `.java` files are the readable source of truth; the committed sibling `.dex`
//! is what `cargo test` analyzes, so the suite needs no Java toolchain to run. This
//! helper rebuilds those dex: run it after adding or editing a fixture.
//!
//!     cargo run --example regen_dex            # both corpora
//!     cargo run --example regen_dex -- binder  # just tests/binder
//!
//! Needs `javac` and AOSP `d8` (point at it with `$LOCKDEX_D8`, else it must be on
//! `PATH`). The deadlock corpus (`tests/corpus`) compiles each fixture alone; the
//! Binder corpus (`tests/binder`) compiles each fixture with a fake `android.os`
//! support package so it dexes without the Android SDK.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let only = std::env::args().nth(1);
    let want = |name: &str| only.as_deref().is_none_or(|o| o == name);

    if want("corpus") {
        regen(&root.join("tests/corpus"), &[]);
    }
    if want("binder") {
        let support = java_files(&root.join("tests/binder/support"));
        regen(&root.join("tests/binder"), &support);
    }
    if want("races") {
        regen(&root.join("tests/races"), &[]);
    }
}

/// Compile and dex every top-level `<name>.java` in `dir`, alongside `extra`
/// sources, writing `<name>.dex` next to each fixture.
fn regen(dir: &Path, extra: &[PathBuf]) {
    let d8 = std::env::var("LOCKDEX_D8").unwrap_or_else(|_| "d8".to_string());
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    fixtures.sort();

    for java in &fixtures {
        let name = java.file_stem().unwrap().to_string_lossy().to_string();
        let scratch = std::env::temp_dir().join(format!("lockdex-regen-{name}"));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("mkdir scratch");

        let mut javac = Command::new("javac");
        javac.arg("-d").arg(&scratch).args(extra).arg(java);
        run(&mut javac);

        let classes = java_files_ext(&scratch, "class");
        run(Command::new(&d8).arg("--min-api").arg("26").arg("--output").arg(&scratch).args(&classes));

        std::fs::copy(scratch.join("classes.dex"), java.with_extension("dex")).expect("copy dex");
        std::fs::remove_dir_all(&scratch).ok();
        println!("regenerated {name}.dex");
    }
}

fn run(cmd: &mut Command) {
    let label = format!("{cmd:?}");
    let status = cmd.status().unwrap_or_else(|e| panic!("spawn {label}: {e}"));
    assert!(status.success(), "command failed: {label}");
}

fn java_files(dir: &Path) -> Vec<PathBuf> {
    java_files_ext(dir, "java")
}

fn java_files_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(java_files_ext(&p, ext));
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
    out.sort();
    out
}
