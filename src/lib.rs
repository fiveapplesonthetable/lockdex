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

//! lockdex — static lock-order / deadlock analyzer for AOSP, working from DEX bytecode.
//!
//! The pipeline is: parse a dex into the [`model`] (via [`dexdump`]), run the
//! compositional [`analyze`]sis to extract lock-order edges, fold them into a
//! [`graph`], and emit a [`report`]. [`export`] renders pprof/HPROF/DOT, [`verify`]
//! resolves candidate cycles back to source, and [`input`] resolves a dex / jar /
//! Soong out dir to the set of dex files to parse.

pub mod analyze;
pub mod dexdump;
pub mod export;
pub mod graph;
pub mod input;
pub mod juc;
pub mod model;
pub mod report;
pub mod verify;

use anyhow::Result;
use std::path::Path;

/// Parse one dex file and build its deadlock report: parse → analyze → graph → report.
pub fn report_for_dex(path: &Path, sinks: &juc::SinkConfig) -> Result<report::JsonReport> {
    let dex = dexdump::parse_dex(path)?;
    let an = analyze::analyze(&dex, sinks);
    let g = graph::LockGraph::build(&an.edges, &an.all_locks);
    Ok(report::build_json(&an, &g))
}
