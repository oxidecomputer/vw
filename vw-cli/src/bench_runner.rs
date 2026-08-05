//! `vw bench` on this machine, and the display both machines feed.
//!
//! The orchestration — what to run, in what order, how many at once — lives in
//! `vw-bench`, because an instance running the same benches has to make the
//! same decisions. What lives here is the part that belongs to a terminal:
//! turning the run's events into the nextest-style panel, and rendering a
//! failure well enough to act on.

use std::sync::Arc;
use std::time::Instant;

use camino::Utf8Path;
use colored::*;

use crate::test_ui::{print_result_line, NextestPanel};

#[derive(Clone)]
pub struct BenchResult {
    name: String,
    /// Combined stdout+stderr of the subprocess. Only failures are kept, so
    /// there is no passing bench's output to decide what to do with.
    output: String,
}

/// Run every matching testbench in parallel, here.
///
/// `filter` is a substring match against the testbench entity name
/// (nextest-style); `None` runs all. `concurrency` caps how many run at once.
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

    let request = vw_bench::Request {
        filter: filter.map(str::to_owned),
        standard: vhdl_std.to_string(),
        concurrency,
        ignore: ignore.to_vec(),
    };
    let names = vw_bench::discover(&ws, &request)?;

    if list {
        for n in &names {
            println!("{}", n.cyan());
        }
        return Ok(());
    }
    if names.is_empty() {
        report_nothing_found(&ws, filter);
        return Ok(());
    }

    if let Err(e) = vw_bench::prepare(&ws, vhdl_std).await {
        crate::report(&e);
        std::process::exit(1);
    }

    // One `vw bench <name> --build-dir …` per bench. The child is this same
    // binary: it already knows how to run exactly one bench into an isolated
    // directory, which is what the internal `--build-dir` mode is for.
    let exe = std::env::current_exe()?;
    let standard = vhdl_std.to_string();
    let launch: vw_bench::Launch =
        Arc::new(move |name: &str, build_dir: &str| {
            let mut command = tokio::process::Command::new(&exe);
            command.args([
                "bench",
                name,
                "--build-dir",
                build_dir,
                "--std",
                &standard,
            ]);
            command
        });

    let overall = Instant::now();
    let panel = Arc::new(NextestPanel::new(names.len() as u64, "testbenches"));
    let failures = Arc::new(std::sync::Mutex::new(Vec::new()));

    let summary = {
        let panel = Arc::clone(&panel);
        let failures = Arc::clone(&failures);
        let rows =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        vw_bench::run(&ws, names, concurrency, launch, move |event| {
            drive_panel(&panel, &rows, &failures, event)
        })
        .await
    };

    panel.clear();

    let failures = failures.lock().expect("failures").clone();
    if !failures.is_empty() {
        println!("\n{}\n", "failures:".red().bold());
        for f in &failures {
            print_bench_failure(f);
        }
    }
    print_result_line(panel.passed(), panel.failed(), overall.elapsed());
    if summary.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Turn one run event into whatever the terminal should show for it.
///
/// Shared by the local runner and the remote one, so a bench finishing looks
/// the same whichever machine it finished on.
pub fn drive_panel(
    panel: &NextestPanel,
    rows: &std::sync::Mutex<
        std::collections::HashMap<String, indicatif::ProgressBar>,
    >,
    failures: &std::sync::Mutex<Vec<BenchResult>>,
    event: vw_bench::Event,
) {
    match event {
        vw_bench::Event::Discovered { .. } => {}
        vw_bench::Event::Started { name } => {
            let row = panel.start(&name);
            rows.lock().expect("rows").insert(name, row);
        }
        vw_bench::Event::Finished {
            name,
            passed,
            seconds,
            output,
        } => {
            let row = rows.lock().expect("rows").remove(&name);
            if let Some(row) = row {
                panel.finish(&row, &name, passed, seconds);
            }
            if !passed {
                failures
                    .lock()
                    .expect("failures")
                    .push(BenchResult { name, output });
            }
        }
        vw_bench::Event::Note { message } => {
            eprintln!("{} {message}", "info:".cyan());
        }
    }
}

fn report_nothing_found(ws: &Utf8Path, filter: Option<&str>) {
    let bench_dir = ws.join("bench");
    if !bench_dir.exists() {
        eprintln!("no bench directory found under {}", ws.as_str().dimmed());
        return;
    }
    match filter {
        Some(f) => eprintln!("no testbenches matched {}", f.dimmed()),
        None => eprintln!(
            "no testbenches found under {}",
            bench_dir.as_str().dimmed()
        ),
    }
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

/// Run the workspace's testbenches on an environment's instance.
///
/// The display is the local one, driven by the same events — a bench finishing
/// looks the same whichever machine it finished on, because the thing that
/// decided how it looks never moved.
pub async fn run_benches_remotely(
    session: &crate::cloud::Session,
    environment: &str,
    filter: Option<&str>,
    concurrency: Option<usize>,
    vhdl_std: vw_lib::VhdlStandard,
    ignore: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let ignored = (!ignore.is_empty()).then(|| ignore.join(","));
    // Bound here rather than in the call: the closure runs once per attempt,
    // and a `&` to a temporary built inside it would not outlive the future.
    let vhdl = vhdl_std.to_string();
    let upgraded = vw_api_client::retrying(|| {
        session.client.bench_session(
            environment,
            concurrency.map(|n| n as u32),
            filter,
            ignored.as_deref(),
            Some(vhdl.as_str()),
        )
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;

    let overall = Instant::now();
    let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rows =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // Sized when the instance says what it found, not before: only it can
    // see the tree to count.
    let mut panel: Option<Arc<NextestPanel>> = None;
    let mut summary = vw_bench::Summary::default();

    while let Some(message) = socket.next().await {
        let text = match message? {
            tokio_tungstenite::tungstenite::Message::Text(text) => text,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };

        match serde_json::from_str::<vw_remote::BenchEvent>(&text)? {
            vw_remote::BenchEvent::Progress { event } => {
                if let vw_bench::Event::Discovered { names } = &event {
                    if names.is_empty() {
                        eprintln!("no testbenches matched");
                        return Ok(());
                    }
                    panel = Some(Arc::new(NextestPanel::new(
                        names.len() as u64,
                        "testbenches",
                    )));
                }
                if let Some(panel) = panel.as_ref() {
                    drive_panel(panel, &rows, &failures, event);
                }
            }
            vw_remote::BenchEvent::Done { passed, failed } => {
                summary = vw_bench::Summary { passed, failed };
                break;
            }
            vw_remote::BenchEvent::Fatal { message } => {
                if let Some(panel) = panel.as_ref() {
                    panel.clear();
                }
                return Err(message.into());
            }
        }
    }

    let Some(panel) = panel else {
        // The instance never got as far as saying what it found.
        return Err("the instance ended the run without reporting".into());
    };
    panel.clear();

    let failures = failures.lock().expect("failures").clone();
    if !failures.is_empty() {
        println!("\n{}\n", "failures:".red().bold());
        for f in &failures {
            print_bench_failure(f);
        }
    }
    print_result_line(panel.passed(), panel.failed(), overall.elapsed());
    if summary.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
