//! lockdex — static lock-order / deadlock analyzer for AOSP, from DEX bytecode.
// Staged build: some model fields/APIs are forward-declared for later stages.
#![allow(dead_code)]

mod analyze;
mod dexdump;
mod export;
mod graph;
mod input;
mod juc;
mod model;
mod report;
mod verify;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
        /// write the verification report here instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Analyze { input, format, out_dir, scope } => {
            let t0 = std::time::Instant::now();
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!(
                "[lockdex] parsed {} classes in {:.1}s",
                dex.classes.len(),
                t0.elapsed().as_secs_f64()
            );
            let an = analyze::analyze(&dex);
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
        Cmd::Verify { input, src_root, max_locks, scope, out } => {
            let set = input::resolve(&input, scope.as_deref())?;
            eprintln!("[lockdex] parsing {} dex file(s) with dexdump (the slow step)...", set.files.len());
            let dex = input::parse_all(&set)?;
            eprintln!("[lockdex] parsed {} classes", dex.classes.len());
            let an = analyze::analyze(&dex);
            let g = graph::LockGraph::build(&an.edges, &an.all_locks);
            let rep = report::build_json(&an, &g);
            eprintln!(
                "[lockdex] {} cycles; verifying those with <= {} locks against {}",
                rep.cycles.len(), max_locks, src_root.display()
            );
            let txt = verify::run(&rep, &src_root, max_locks);
            match out {
                Some(p) => {
                    std::fs::write(&p, &txt)?;
                    eprintln!("[lockdex] verification written to {}", p.display());
                }
                None => print!("{txt}"),
            }
        }
    }
    Ok(())
}

/// Concise stdout when `--out-dir` is used: what was found + which files hold it.
fn outputs_summary(dir: &Path, rep: &report::JsonReport) -> String {
    use std::fmt::Write as _;
    let small = rep.cycles.iter().filter(|c| c.locks.len() <= 12).count();
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
        ("cycles.svg", "the small cycles, drawn (open in a browser)"),
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
    // Cycle subgraph DOT (small) — this is the one worth viewing; render to SVG.
    let cyc = report::dot_cycles(g);
    std::fs::write(p("cycles.dot"), &cyc)?;
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
