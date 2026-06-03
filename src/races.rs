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

//! `lockdex races` — render the field-race / `@GuardedBy` findings: a Markdown
//! report, the violating source lines in context, and a per-field diagram showing
//! the field, its guard lock, and the accessors that miss it.

use crate::analyze::{Analysis, FieldRace};
use crate::source::{class_path, esc, short_lock, short_method, snippet, Source};
use std::fmt::Write as _;
use std::path::Path;

/// Narrow the report to a field and/or guard lock (substring match).
#[derive(Default)]
pub struct Filter {
    pub field: Option<String>,
    pub guard: Option<String>,
}

impl Filter {
    pub fn active(&self) -> bool {
        self.field.is_some() || self.guard.is_some()
    }
    fn keep(&self, f: &FieldRace) -> bool {
        self.field.as_deref().is_none_or(|x| f.field.contains(x))
            && self.guard.as_deref().is_none_or(|x| f.guard.contains(x))
    }
}

/// Diagrams beyond this many are skipped unless a filter narrows the report.
const MAX_DIAGRAMS: usize = 40;

/// The findings that pass a filter, as an owned report (for JSON export).
pub fn filtered(r: &crate::analyze::RaceReport, filter: &Filter) -> crate::analyze::RaceReport {
    crate::analyze::RaceReport {
        fields: r.fields.iter().filter(|f| filter.keep(f)).cloned().collect(),
    }
}

pub fn report(an: &Analysis, filter: &Filter, src_root: Option<&Path>, out_dir: Option<&Path>) -> String {
    let fields: Vec<&FieldRace> = an.races.fields.iter().filter(|f| filter.keep(f)).collect();
    let mut src = src_root.map(Source::index);
    if let Some(d) = out_dir {
        let _ = std::fs::create_dir_all(d);
    }
    let cap = if filter.active() { usize::MAX } else { MAX_DIAGRAMS };

    let mut md = String::new();
    let _ = writeln!(md, "# lockdex — inconsistently-guarded fields (@GuardedBy)\n");
    if filter.active() {
        let what = [filter.field.as_deref(), filter.guard.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" + ");
        let _ = writeln!(md, "_Filtered to: {what}_\n");
    }
    let _ = writeln!(
        md,
        "Each field below is guarded by one lock on most of its writes, but accessed \
         without that lock somewhere — the unguarded accesses are the suspected races. \
         The held-set is interprocedural (a field touched in a helper is guarded if the \
         helper is always reached under the lock). `final`/`volatile` fields and \
         constructor writes are excluded.\n"
    );
    let _ = writeln!(md, "**{}** field(s) flagged.\n", fields.len());

    for (i, f) in fields.iter().enumerate() {
        let writes_v = f.violations.iter().filter(|v| v.write).count();
        let _ = writeln!(md, "## `{}` — @GuardedBy(`{}`)\n", short_lock(&f.field), short_lock(&f.guard));
        let _ = writeln!(
            md,
            "- guarded on **{}/{}** writes; {} read(s); **{}** access(es) miss the guard ({} write).",
            f.guarded_writes, f.writes, f.reads, f.violations.len(), writes_v
        );
        if i < cap {
            if let Some(d) = out_dir {
                if let Some(name) = render(d, &format!("field{:03}", i + 1), &field_dot(f)) {
                    let _ = writeln!(md, "- diagram: `{name}`");
                }
            }
        }
        let _ = writeln!(md, "\nunguarded accesses:");
        for v in &f.violations {
            let kind = if v.write { "write" } else { "read" };
            let loc = v.line.map(|l| format!(":{l}")).unwrap_or_default();
            let _ = writeln!(md, "- {kind} in `{}`{loc}", short_method(&v.method));
        }
        // Inline source for the first few violations of the top fields.
        if i < cap {
            if let Some(s) = src.as_mut() {
                for v in f.violations.iter().take(3) {
                    if let (Some(class), Some(line)) = (class_path(&v.method), v.line) {
                        snippet(&mut md, s, src_root, &class, line as usize);
                    }
                }
            }
        }
        let _ = writeln!(md);
    }
    md
}

const FIELD_NODE: &str = "shape=box, style=\"filled,rounded\", fillcolor=\"#fef9c3\", color=\"#ca8a04\", penwidth=2";
const GUARD_NODE: &str = "shape=box, style=\"filled,rounded\", fillcolor=\"#dcfce7\", color=\"#16a34a\", penwidth=2";
const BAD_NODE: &str = "shape=box, style=\"filled,rounded\", fillcolor=\"#fee2e2\", color=\"#dc2626\"";

/// The field (centre) guarded by its lock (green), with each violating accessor in
/// red — so "guarded by L, but these methods touch it without L" reads at a glance.
fn field_dot(f: &FieldRace) -> String {
    let mut s = String::from(
        "digraph race {\n  \
         rankdir=LR; bgcolor=\"white\";\n  \
         graph [nodesep=0.3, ranksep=0.6];\n  \
         node [fontname=\"Helvetica\", fontsize=11];\n  \
         edge [fontname=\"Helvetica\", fontsize=9, arrowsize=0.8];\n",
    );
    let _ = writeln!(s, "  \"__field__\" [{FIELD_NODE}, label=\"{}\"];", esc(&short_lock(&f.field)));
    let _ = writeln!(s, "  \"__guard__\" [{GUARD_NODE}, label=\"{}\"];", esc(&short_lock(&f.guard)));
    let _ = writeln!(
        s,
        "  \"__guard__\" -> \"__field__\" [label=\" guards {}/{}\", color=\"#16a34a\", fontcolor=\"#15803d\", penwidth=1.6];",
        f.guarded_writes, f.writes
    );
    let mut seen = std::collections::HashSet::new();
    for v in &f.violations {
        let id = format!("m::{}", v.method);
        if seen.insert(id.clone()) {
            let _ = writeln!(s, "  \"{}\" [{BAD_NODE}, label=\"{}\"];", esc(&id), esc(&short_method(&v.method)));
        }
        let kind = if v.write { "writes" } else { "reads" };
        let _ = writeln!(
            s,
            "  \"{}\" -> \"__field__\" [label=\" {kind} unguarded\", color=\"#dc2626\", fontcolor=\"#dc2626\"];",
            esc(&id)
        );
    }
    s.push_str("}\n");
    s
}

/// Write `<name>.dot` and, if Graphviz is present, render `<name>.svg`.
fn render(dir: &Path, name: &str, dot: &str) -> Option<String> {
    let dotp = dir.join(format!("{name}.dot"));
    std::fs::write(&dotp, dot).ok()?;
    if let Ok(o) = std::process::Command::new("dot").arg("-Tsvg").arg(&dotp).output() {
        if o.status.success() {
            std::fs::write(dir.join(format!("{name}.svg")), o.stdout).ok()?;
            return Some(format!("{name}.svg"));
        }
    }
    Some(format!("{name}.dot"))
}
