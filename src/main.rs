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

//! Command-line front-end for the `lockdex` library.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lockdex::resolve::{Lookup, ResolveIndex};
use lockdex::{analyze, binder, export, graph, input, juc, races, report, verify};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "lockdex", about = "Static lock-order / deadlock analyzer (DEX-based)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Analyze a dex / jar / Soong out dir and report lock-order deadlocks.
    Analyze {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// output format for stdout: text | json | dot
        #[arg(long, default_value = "text")]
        format: String,
        /// write the full artifact set (report, json, dot, svg, pprof, hprof) here
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// narrow a Soong out dir to jars whose name contains this (e.g. services)
        #[arg(long)]
        scope: Option<String>,
        /// extra async-dispatch methods: `Class.method` to add, `-Class.method`
        /// to disable a built-in, `extends pkg.Base: m1 m2` to cover a base class
        /// and everything inheriting from it, `-extends pkg.Base` to disable a
        /// built-in base (one per line, `#` comments). Added on top of the
        /// defaults (Handler.post, Executor.execute, Thread.start, ... and their
        /// subtypes — see ASYNC_BASES).
        #[arg(long)]
        async_dispatch: Option<PathBuf>,
    },
    /// Resolve monitor-contention sites to the canonical lock taken there.
    /// Give one or more FILE:LINE (e.g. `ActivityManagerService.java:1701`, or a
    /// full path). Resolution is DEX register dataflow — no source, no rules —
    /// so `this`, fields, outer `this$0` fields, getters and aliases all resolve.
    Resolve {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// FILE:LINE locations to resolve (repeatable)
        #[arg(required = true)]
        locs: Vec<String>,
        /// tolerate line drift: if no monitor-enter sits exactly on the line,
        /// snap to the nearest one within this many lines (0 = exact only).
        #[arg(long, default_value = "0")]
        fuzz: u32,
        /// anchor to a method (substring of its key) instead of trusting the
        /// line — survives line drift. Pair with the contention's method name.
        #[arg(long)]
        method: Option<String>,
        /// last resort when the line can't be matched: if the file (after any
        /// --method filter) takes exactly one distinct lock, return it.
        #[arg(long)]
        if_unique: bool,
        /// narrow a Soong out dir to jars whose name contains this (e.g. services)
        #[arg(long)]
        scope: Option<String>,
    },
    /// Build a small, reusable resolve index once (the slow dexdump + fixpoint),
    /// so later `query` calls answer FILE:LINE lookups in milliseconds without
    /// re-analyzing. The index is a JSON projection of just the monitor-enter
    /// sites — independent of the input jars and safe to cache/ship.
    Index {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// where to write the index (e.g. locks.idx.json)
        #[arg(long, short = 'o')]
        out: PathBuf,
        /// narrow a Soong out dir to jars whose name contains this (e.g. services)
        #[arg(long)]
        scope: Option<String>,
    },
    /// Resolve FILE:LINE sites against a prebuilt `index` (see `lockdex index`).
    /// Loads in milliseconds and answers millions of queries cheaply — pass them
    /// as args and/or stream them on stdin (one FILE:LINE per line).
    Query {
        /// index file written by `lockdex index`
        index: PathBuf,
        /// FILE:LINE locations to resolve (repeatable); optional with --stdin
        locs: Vec<String>,
        /// also read FILE:LINE queries from stdin, one per line (`#` comments and
        /// blank lines ignored) — the path for millions of queries.
        #[arg(long)]
        stdin: bool,
        /// tolerate line drift: snap to the nearest monitor-enter within N lines.
        #[arg(long, default_value = "0")]
        fuzz: u32,
        /// anchor to a method (substring of its key) instead of trusting the line.
        #[arg(long)]
        method: Option<String>,
        /// last resort: if the file (after --method) takes exactly one distinct
        /// lock, return it.
        #[arg(long)]
        if_unique: bool,
    },
    /// Analyze, then pull the source for each candidate cycle and print a verdict.
    Verify {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// source checkout to resolve file:line against (e.g. ~/dev/aosp)
        #[arg(long)]
        src_root: PathBuf,
        /// only verify cycles with at most this many locks
        #[arg(long, default_value_t = 6)]
        max_locks: usize,
        /// narrow a Soong out dir to jars whose name contains this
        #[arg(long)]
        scope: Option<String>,
        /// write just the text report to this file (instead of stdout)
        #[arg(long)]
        out: Option<PathBuf>,
        /// write the full bundle here: verify.txt + per-candidate dot/svg/pprof/hprof
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// extra async-dispatch methods (see `analyze --async-dispatch`)
        #[arg(long)]
        async_dispatch: Option<PathBuf>,
    },
    /// Report locks held across Binder IPC boundaries (a cross-process hazard).
    Binder {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// which boundaries to report: out | in | both
        #[arg(long, default_value = "both")]
        direction: String,
        /// only findings whose lock name contains this (emits all their diagrams)
        #[arg(long)]
        lock: Option<String>,
        /// only findings whose holder/entry contains this, e.g. a service name
        #[arg(long)]
        class: Option<String>,
        /// source checkout to inline holding sites from (optional)
        #[arg(long)]
        src_root: Option<PathBuf>,
        /// write binder.md + binder.json + per-finding dot/svg + pprof/hprof here
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// narrow a Soong out dir to jars whose name contains this
        #[arg(long)]
        scope: Option<String>,
        /// extra async-dispatch methods (see `analyze --async-dispatch`)
        #[arg(long)]
        async_dispatch: Option<PathBuf>,
    },
    /// Infer each field's guard lock and flag the ones guarded inconsistently.
    Races {
        /// .dex, .jar/.apk (multidex), or a Soong `out` directory
        input: PathBuf,
        /// only fields whose name contains this (emits all their diagrams)
        #[arg(long)]
        field: Option<String>,
        /// only fields guarded by a lock whose name contains this
        #[arg(long)]
        guard: Option<String>,
        /// baseline: percent of a field's writes that must share the guard before
        /// the rest count as violations (lower = more, noisier; higher = stricter)
        #[arg(long, default_value_t = 66)]
        min_coverage: u32,
        /// ignore fields written fewer than this many times
        #[arg(long, default_value_t = 2)]
        min_writes: usize,
        /// source checkout to inline the unguarded accesses from (optional)
        #[arg(long)]
        src_root: Option<PathBuf>,
        /// write races.md + races.json + per-field dot/svg here
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// narrow a Soong out dir to jars whose name contains this
        #[arg(long)]
        scope: Option<String>,
        /// extra async-dispatch methods (see `analyze --async-dispatch`)
        #[arg(long)]
        async_dispatch: Option<PathBuf>,
    },
}

/// Load `--async-dispatch` adjustments. Per line (`#` comments, blanks ignored):
///   Class.method | method          add an async-dispatch point by name
///   -Class.method | -method        disable a built-in name entry
///   extends pkg.Base: m1 m2        dispatch methods on pkg.Base *and every
///                                  class inheriting from it* (type hierarchy)
///   -extends pkg.Base              disable a built-in hierarchy base entirely
fn load_async_dispatch(path: Option<&Path>) -> Result<juc::AsyncConfig> {
    let mut cfg = juc::AsyncConfig::default();
    let Some(p) = path else { return Ok(cfg) };
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    for line in text.lines() {
        let s = line.split('#').next().unwrap_or("").trim();
        if s.is_empty() {
            continue;
        }
        let (removing, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest.trim()),
            None => (false, s.trim_start_matches('+').trim()),
        };
        if let Some(rest) = body.strip_prefix("extends ") {
            if removing {
                // tolerate `-extends pkg.Base: m1 m2` — the disable is per base,
                // so anything after `:` is irrelevant.
                let base = rest.split(':').next().unwrap_or(rest).trim();
                cfg.remove_base.insert(base.to_string());
            } else {
                let (base, methods) = rest.split_once(':').with_context(|| {
                    format!("`extends` entry needs `extends pkg.Base: m1 m2 ...`, got: {s}")
                })?;
                let base = base.trim();
                // hierarchy matching compares fully-qualified names from the dex;
                // a simple name would silently never match.
                anyhow::ensure!(
                    base.contains('.'),
                    "`extends {base}`: base must be fully qualified (e.g. com.example.os.{base})"
                );
                let ms: std::collections::HashSet<String> =
                    methods.split_whitespace().map(String::from).collect();
                anyhow::ensure!(!ms.is_empty(), "`extends {base}` lists no methods");
                cfg.add_base.entry(base.to_string()).or_default().extend(ms);
            }
        } else if removing {
            cfg.remove.insert(body.to_string());
        } else {
            cfg.add.insert(body.to_string());
        }
    }
    eprintln!(
        "[lockdex] async dispatch: +{} -{} names, +{} -{} hierarchy bases (on top of built-ins)",
        cfg.add.len(), cfg.remove.len(), cfg.add_base.len(), cfg.remove_base.len()
    );
    Ok(cfg)
}

/// Resolve one `FILE:LINE` against a prepared [`Lookup`] and write the result as
/// `loc<TAB>lock[, lock ...][note]` (or `loc<TAB>(unresolved)`). Shared by
/// `resolve` and `query` so their output is byte-identical. Malformed locs are
/// reported to stderr and skipped, never aborting a batch.
fn resolve_one<W: Write>(
    lookup: &Lookup,
    loc: &str,
    fuzz: u32,
    method: Option<&str>,
    if_unique: bool,
    out: &mut W,
) -> std::io::Result<()> {
    let Some((file, line_s)) = loc.rsplit_once(':') else {
        eprintln!("skip {loc}: expected FILE:LINE");
        return Ok(());
    };
    let Ok(line) = line_s.trim().parse::<i64>() else {
        eprintln!("skip {loc}: line is not a number");
        return Ok(());
    };
    let r = lookup.resolve(file, line, fuzz, method, if_unique);
    if r.locks.is_empty() {
        writeln!(out, "{loc}\t(unresolved)")
    } else {
        writeln!(out, "{loc}\t{}{}", r.locks.join(", "), r.note)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Resolve { input, locs, fuzz, method, if_unique, scope } => {
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump...", set.files.len());
            let dex = input::parse_all(&set)?;
            let an = analyze::analyze(&dex, &load_async_dispatch(None)?);
            // Same projection `index` persists — so `resolve` and `query` share
            // one code path and cannot give different answers.
            let index = ResolveIndex::from_acquisitions(&an.acquisitions);
            let lookup = index.prepare();
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            for loc in &locs {
                resolve_one(&lookup, loc, fuzz, method.as_deref(), if_unique, &mut out)?;
            }
            out.flush()?;
        }
        Cmd::Index { input, out, scope } => {
            let t0 = std::time::Instant::now();
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            let an = analyze::analyze(&dex, &load_async_dispatch(None)?);
            let index = ResolveIndex::from_acquisitions(&an.acquisitions);
            let json = serde_json::to_string(&index)?;
            std::fs::write(&out, &json)
                .with_context(|| format!("writing index {}", out.display()))?;
            eprintln!(
                "[lockdex] indexed {} monitor-enter site(s) to {} ({:.1} MB) in {:.1}s — \
                 query it with `lockdex query {} FILE:LINE ...` (no re-analysis)",
                index.sites.len(),
                out.display(),
                json.len() as f64 / 1e6,
                t0.elapsed().as_secs_f64(),
                out.display(),
            );
        }
        Cmd::Query { index, locs, stdin, fuzz, method, if_unique } => {
            let t0 = std::time::Instant::now();
            let raw = std::fs::read_to_string(&index)
                .with_context(|| format!("reading index {}", index.display()))?;
            let idx: ResolveIndex = serde_json::from_str(&raw)
                .with_context(|| format!("parsing index {} — rebuild with `lockdex index`?", index.display()))?;
            anyhow::ensure!(
                idx.version == ResolveIndex::VERSION,
                "index {} is version {} but this lockdex expects {} — rebuild with `lockdex index`",
                index.display(), idx.version, ResolveIndex::VERSION
            );
            let lookup = idx.prepare();
            eprintln!(
                "[lockdex] loaded {} site(s) in {:.0}ms — answering queries",
                idx.sites.len(),
                t0.elapsed().as_secs_f64() * 1e3
            );
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            for loc in &locs {
                resolve_one(&lookup, loc, fuzz, method.as_deref(), if_unique, &mut out)?;
            }
            if stdin {
                use std::io::BufRead;
                let inp = std::io::stdin();
                for line in inp.lock().lines() {
                    let line = line?;
                    let loc = line.trim();
                    if loc.is_empty() || loc.starts_with('#') {
                        continue;
                    }
                    resolve_one(&lookup, loc, fuzz, method.as_deref(), if_unique, &mut out)?;
                }
            }
            out.flush()?;
        }
        Cmd::Analyze { input, format, out_dir, scope, async_dispatch } => {
            let t0 = std::time::Instant::now();
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!(
                "[lockdex] parsed {} classes in {:.1}s",
                dex.classes.len(),
                t0.elapsed().as_secs_f64()
            );
            let async_cfg = load_async_dispatch(async_dispatch.as_deref())?;
            let an = analyze::analyze(&dex, &async_cfg);
            let g = graph::LockGraph::build(&an.edges, &an.all_locks);
            let rep = report::build_json(&an, &g);
            eprintln!(
                "[lockdex] {} methods, {} locks, {} edges, {} deadlock cycles, {} suppressed ({:.1}s)",
                rep.method_count, rep.node_count, rep.edge_count,
                rep.cycles.len(), rep.suppressed.len(), t0.elapsed().as_secs_f64()
            );

            if let Some(dir) = &out_dir {
                write_artifacts(dir, &an, &g, &rep)?;
                // with --out-dir the full report lives in report.txt; on stdout
                // just say what was written (unless an explicit pipe format).
                match format.as_str() {
                    "json" => println!("{}", serde_json::to_string_pretty(&rep)?),
                    "dot" => print!("{}", report::dot(&g)),
                    "none" => {}
                    _ => print!("{}", outputs_summary(dir, &rep)),
                }
            } else {
                match format.as_str() {
                    "json" => println!("{}", serde_json::to_string_pretty(&rep)?),
                    "dot" => print!("{}", report::dot(&g)),
                    "none" => {}
                    _ => print!("{}", report::text(&rep)),
                }
            }
        }
        Cmd::Verify { input, src_root, max_locks, scope, out, out_dir, async_dispatch } => {
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!("[lockdex] parsed {} classes", dex.classes.len());
            let async_cfg = load_async_dispatch(async_dispatch.as_deref())?;
            let an = analyze::analyze(&dex, &async_cfg);
            let g = graph::LockGraph::build(&an.edges, &an.all_locks);
            let rep = report::build_json(&an, &g);
            eprintln!(
                "[lockdex] {} SCC(s); verifying small cycles and tangle inversions with <= {} locks against {}",
                rep.cycles.len(), max_locks, src_root.display()
            );
            let txt = verify::run(&rep, &an.paths, &src_root, max_locks, out_dir.as_deref());
            if let Some(d) = &out_dir {
                std::fs::create_dir_all(d)?;
                std::fs::write(d.join("verify.txt"), &txt)?;
                eprintln!(
                    "[lockdex] verify.txt + per-candidate dot/svg/pprof/hprof written to {}",
                    d.display()
                );
            } else if let Some(p) = &out {
                std::fs::write(p, &txt)?;
                eprintln!("[lockdex] verification written to {}", p.display());
            } else {
                print!("{txt}");
            }
        }
        Cmd::Binder { input, direction, lock, class, src_root, out_dir, scope, async_dispatch } => {
            let Some(dir) = binder::Direction::parse(&direction) else {
                anyhow::bail!("--direction must be `out`, `in`, or `both`");
            };
            let filter = binder::Filter { lock, class };
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!("[lockdex] parsed {} classes", dex.classes.len());
            let async_cfg = load_async_dispatch(async_dispatch.as_deref())?;
            let an = analyze::analyze(&dex, &async_cfg);
            let md = binder::report(&an, dir, &filter, src_root.as_deref(), out_dir.as_deref());
            let report = binder::filtered(&an.binder, &filter);
            if let Some(d) = &out_dir {
                std::fs::create_dir_all(d)?;
                std::fs::write(d.join("binder.md"), &md)?;
                std::fs::write(d.join("binder.json"), serde_json::to_string_pretty(&report)?)?;
                let me = binder::method_edges(&an, dir, &filter);
                if !me.is_empty() {
                    export::write_file(&d.join("binder.pb.gz"), &export::pprof_method_edges(&me))?;
                    export::write_file(&d.join("binder.hprof"), &export::hprof_method_edges(&me))?;
                }
                eprintln!(
                    "[lockdex] binder.md + binder.json + per-finding dot/svg + pprof/hprof written to {}",
                    d.display()
                );
                println!(
                    "lockdex binder: {} outgoing hold-site(s), {} incoming entr(ies) ({} high-risk). See {}/binder.md",
                    report.outgoing.len(),
                    report.incoming.len(),
                    report.incoming.iter().filter(|f| f.high).count(),
                    d.display()
                );
            } else {
                print!("{md}");
            }
        }
        Cmd::Races { input, field, guard, min_coverage, min_writes, src_root, out_dir, scope, async_dispatch } => {
            let filter = races::Filter { field, guard, min_coverage, min_writes };
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!("[lockdex] parsed {} classes", dex.classes.len());
            let async_cfg = load_async_dispatch(async_dispatch.as_deref())?;
            let an = analyze::analyze(&dex, &async_cfg);
            let md = races::report(&an, &filter, src_root.as_deref(), out_dir.as_deref());
            let report = races::filtered(&an.races, &filter);
            if let Some(d) = &out_dir {
                std::fs::create_dir_all(d)?;
                std::fs::write(d.join("races.md"), &md)?;
                std::fs::write(d.join("races.json"), serde_json::to_string_pretty(&report)?)?;
                eprintln!("[lockdex] races.md + races.json + per-field dot/svg written to {}", d.display());
                println!(
                    "lockdex races: {} inconsistently-guarded field(s). See {}/races.md",
                    report.fields.len(),
                    d.display()
                );
            } else {
                print!("{md}");
            }
        }
    }
    Ok(())
}

/// Concise stdout when `--out-dir` is used: what was found + which files hold it.
fn outputs_summary(dir: &Path, rep: &report::JsonReport) -> String {
    use std::fmt::Write as _;
    let small = rep.cycles.iter().filter(|c| c.locks.len() <= report::TANGLE).count();
    let tangles = rep.cycles.len() - small;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "lockdex: {} deadlock cycle(s) — {} small (actionable), {} large tangle(s); {} suppressed by guard.",
        rep.cycles.len(), small, tangles, rep.suppressed.len()
    );
    let _ = writeln!(s, "outputs in {}:", dir.display());
    let entries: &[(&str, &str)] = &[
        ("report.txt", "the report — read this first (cycles, locks, file:line)"),
        ("cycles.svg", "the cycles drawn — small SCCs red, tangle inversions amber"),
        ("lockgraph.json", "full graph + findings, for tooling"),
        ("lockorder.pb.gz", "pprof — go tool pprof -http=: <dir>/lockorder.pb.gz"),
        ("methodlock.hprof", "Perfetto heap graph — drag into https://ui.perfetto.dev"),
        ("lockgraph.dot", "full graph for tooling (not rendered)"),
    ];
    for (f, desc) in entries {
        if dir.join(f).exists() {
            let _ = writeln!(s, "  {:<17} {}", f, desc);
        }
    }
    let _ = writeln!(
        s,
        "\nnext: open {0}/report.txt — or verify a cycle against source:\n  \
         lockdex verify <input> --src-root <aosp> --max-locks 3",
        dir.display()
    );
    s
}

fn write_artifacts(
    dir: &Path,
    an: &analyze::Analysis,
    g: &graph::LockGraph,
    rep: &report::JsonReport,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let p = |name: &str| dir.join(name);

    eprintln!("[lockdex] writing report.txt + lockgraph.json ...");
    std::fs::write(p("report.txt"), report::text(rep))?;
    std::fs::write(p("deadlock_cycles.txt"), report::text(rep))?;
    std::fs::write(p("lockgraph.json"), serde_json::to_string_pretty(rep)?)?;

    eprintln!("[lockdex] writing pprof + hprof ...");
    export::write_file(&p("lockorder.pb.gz"), &export::pprof_lock_order(g))?;
    export::write_file(&p("methodlock.hprof"), &export::hprof_method_graph(an))?;

    // Full graph DOT (for tooling) — written but NOT rendered: too many edges.
    std::fs::write(p("lockgraph.dot"), report::dot(g))?;
    // Cycle subgraph DOT (small cycles + tangle inversions) — the one worth
    // viewing; render to SVG.
    let cyc = report::dot_cycles(g);
    std::fs::write(p("cycles.dot"), &cyc)?;
    // Graphviz is pathologically slow past a few tens of thousands of edges; on
    // such a graph leave the .dot for the user to render (nothing is dropped —
    // the same data is in report.txt / lockgraph.json).
    let cyc_edges = cyc.matches(" -> ").count();
    if cyc_edges > 20_000 {
        eprintln!(
            "[lockdex] cycles.dot has {cyc_edges} edges — skipping the automatic SVG render \
             (run `dot -Tsvg cycles.dot > cycles.svg` yourself, it may take a while)"
        );
        eprintln!("[lockdex] artifacts written to {}", dir.display());
        return Ok(());
    }
    eprintln!("[lockdex] rendering cycle SVG with graphviz (skip if dot is missing) ...");
    match std::process::Command::new("dot")
        .arg("-Tsvg")
        .arg(p("cycles.dot"))
        .output()
    {
        Ok(out) if out.status.success() => {
            std::fs::write(p("cycles.svg"), out.stdout)?;
        }
        Ok(_) => eprintln!("[lockdex] (graphviz failed; cycles.dot written, render it yourself)"),
        Err(_) => eprintln!("[lockdex] (graphviz `dot` not found; skipped SVG, cycles.dot written)"),
    }
    eprintln!("[lockdex] artifacts written to {}", dir.display());
    Ok(())
}
