//! Parallel `vw bench` runner (nextest-style). Enumerates VHDL / cosim
//! testbenches, optionally filters by a name substring, and runs each one
//! as an isolated `vw bench <name> --build-dir target/sim/<name>`
//! subprocess — bounded by a concurrency limit — driving the shared
//! [`crate::test_ui::NextestPanel`] live display.
//!
//! A subprocess-per-bench model (like cargo-nextest) gives us free output
//! capture and per-bench nvc build-dir isolation, and keeps one failing
//! bench from aborting the batch.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use camino::Utf8Path;
use colored::*;
use tokio::sync::Semaphore;

use crate::test_ui::{print_result_line, NextestPanel};

struct BenchResult {
    name: String,
    passed: bool,
    /// Combined stdout+stderr of the subprocess, shown only on failure.
    output: String,
}

/// Run every matching testbench in parallel. `filter` is a substring
/// match against the testbench entity name (nextest-style); `None` runs
/// all. `concurrency` caps how many run at once.
pub async fn run_benches(
    cwd: &Utf8Path,
    filter: Option<&str>,
    list: bool,
    concurrency: usize,
    vhdl_std: vw_lib::VhdlStandard,
    ignore: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let ws = vw_lib::find_workspace_dir(cwd.as_std_path())
        .ok_or("not in a vw workspace (no vw.toml in the parent chain)")?;
    let bench_dir = ws.join("bench");
    if !bench_dir.exists() {
        eprintln!("no bench directory found under {}", ws.as_str().dimmed());
        return Ok(());
    }

    let names = discover_benches(&bench_dir, filter, ignore)?;

    if list {
        for n in &names {
            println!("{}", n.cyan());
        }
        return Ok(());
    }
    if names.is_empty() {
        match filter {
            Some(f) => eprintln!("no testbenches matched {}", f.dimmed()),
            None => eprintln!(
                "no testbenches found under {}",
                bench_dir.as_str().dimmed()
            ),
        }
        return Ok(());
    }

    // Generate anodizer structs once up front; the per-bench subprocesses
    // then skip it (the fingerprint is fresh), avoiding a regen race.
    if let Err(e) = vw_lib::ensure_anodized(&ws, vhdl_std, None).await {
        eprintln!("{} anodize failed: {e}", "error:".bright_red());
        std::process::exit(1);
    }

    let overall = Instant::now();
    let panel = Arc::new(NextestPanel::new(names.len() as u64, "testbenches"));
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let exe = std::env::current_exe()?;
    let std_str = vhdl_std.to_string();

    let mut handles = Vec::new();
    for name in names {
        let sem = sem.clone();
        let exe = exe.clone();
        let ws = ws.clone();
        let std_str = std_str.clone();
        let panel = panel.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let row = panel.start(&name);

            let build_dir = format!("{}/{}", vw_lib::BUILD_DIR, name);
            let start = Instant::now();
            let out = tokio::process::Command::new(&exe)
                .args([
                    "bench",
                    &name,
                    "--build-dir",
                    &build_dir,
                    "--std",
                    &std_str,
                ])
                .current_dir(ws.as_std_path())
                .output()
                .await;
            let secs = start.elapsed().as_secs_f64();

            let result = match out {
                Ok(o) => {
                    let mut output =
                        String::from_utf8_lossy(&o.stdout).into_owned();
                    output.push_str(&String::from_utf8_lossy(&o.stderr));
                    BenchResult {
                        name: name.clone(),
                        passed: o.status.success(),
                        output,
                    }
                }
                Err(e) => BenchResult {
                    name: name.clone(),
                    passed: false,
                    output: format!("failed to launch subprocess: {e}"),
                },
            };

            panel.finish(&row, &result.name, result.passed, secs);
            result
        }));
    }

    let mut failures: Vec<BenchResult> = Vec::new();
    for h in handles {
        match h.await {
            Ok(r) => {
                if !r.passed {
                    failures.push(r);
                }
            }
            Err(e) => failures.push(BenchResult {
                name: "<panicked>".into(),
                passed: false,
                output: format!("runner task panicked: {e}"),
            }),
        }
    }
    panel.clear();

    if !failures.is_empty() {
        println!("\n{}\n", "failures:".red().bold());
        for f in &failures {
            print_bench_failure(f);
        }
    }
    print_result_line(panel.passed(), panel.failed(), overall.elapsed());
    if panel.failed() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Enumerate testbench entity names (`*_tb`) under `bench_dir`, sorted and
/// de-duplicated, filtered by a name substring when given.
fn discover_benches(
    bench_dir: &Utf8Path,
    filter: Option<&str>,
    ignore: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let ignore: HashSet<String> = ignore.iter().cloned().collect();
    let benches = vw_lib::list_testbenches(bench_dir, &ignore, true)?;
    let mut names: Vec<String> = benches
        .into_iter()
        .map(|t| t.name)
        .filter(|n| n.to_lowercase().ends_with("_tb"))
        .filter(|n| filter.is_none_or(|f| n.contains(f)))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Failure block for one bench, in the same visual frame `vw test` uses:
/// the key diagnostic lines surfaced up front, then a (demangled) tail of
/// the captured output for context.
fn print_bench_failure(f: &BenchResult) {
    let bar = "─".repeat(64);
    println!("{}", bar.red());
    println!(" {} {}", "✗".red().bold(), f.name.bold());

    let lines: Vec<&str> = f.output.trim_end().lines().collect();

    // The real reason (`** Fatal: …`, `panicked at …`, `error: …`) often
    // sits far above the tail — pull those lines out and show them first.
    let key: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| is_key_error_line(l))
        .collect();
    if !key.is_empty() {
        println!("\n{}", "ERROR:".red().bold());
        for l in &key {
            println!("  {}", demangle_line(l).red());
        }
    }

    if !lines.is_empty() {
        println!("\n{}", "OUTPUT (tail):".bright_black().bold());
        let start = lines.len().saturating_sub(40);
        for l in &lines[start..] {
            println!("  {}", demangle_line(l));
        }
    }
    println!("{}\n", bar.red());
}

/// Lines worth pulling out of a long crash dump.
fn is_key_error_line(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("** Fatal:")
        || l.starts_with("** Error:")
        || l.starts_with("error:")
        || l.contains("panicked at")
        || l.contains("Caught signal")
        || l.starts_with("Assertion")
        || l.contains("TEST FAILED")
}

/// Demangle any Rust (`_R…`) or C++ (`_Z…`) symbols on a line so nvc
/// backtraces are legible. Lines with no mangled tokens pass through
/// untouched.
fn demangle_line(line: &str) -> String {
    if !line.contains("_R") && !line.contains("_Z") {
        return line.to_string();
    }
    line.split(' ')
        .map(|tok| {
            if tok.starts_with("_R") || tok.starts_with("_Z") {
                demangle_symbol(tok)
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn demangle_symbol(sym: &str) -> String {
    if sym.starts_with("_R") {
        // Rust v0 — unambiguous. `{:#}` drops the disambiguator hash.
        if let Ok(d) = rustc_demangle::try_demangle(sym) {
            return format!("{d:#}");
        }
    } else if sym.starts_with("_Z") {
        // C++ (Itanium) — with a fallback to legacy Rust `_ZN…` mangling.
        if let Ok(s) = cpp_demangle::Symbol::new(sym) {
            if let Ok(d) = s.demangle(&cpp_demangle::DemangleOptions::default())
            {
                return d;
            }
        }
        if let Ok(d) = rustc_demangle::try_demangle(sym) {
            return format!("{d:#}");
        }
    }
    sym.to_string()
}
