// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Running a workspace's testbenches.
//!
//! One orchestrator, used from both sides. A developer running `vw bench run`
//! on their own machine and an agent running it on an instance discover the
//! same benches, prepare the workspace the same way and fan out the same
//! number at a time — because it is the same code, not because two copies
//! were kept in step.
//!
//! What differs between the two is only how a single bench is launched and
//! where the progress goes, so those are the two things passed in. Everything
//! else is a property of the workspace, and the workspace is wherever this
//! happens to be running.
//!
//! **Why a subprocess per bench.** `nvc` inherits stdio, so simulation output
//! goes wherever the process's output goes. Running several in one process
//! would interleave their output beyond repair and lose the ability to show
//! only the failing one's. A child per bench also gives each an isolated
//! build directory and keeps one bench's crash from taking the batch with it —
//! the same reasoning cargo-nextest arrives at.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

/// What to run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Request {
    /// Substring match against a testbench's entity name. `None` runs all.
    pub filter: Option<String>,
    /// The VHDL standard, as `nvc` spells it.
    pub standard: String,
    /// How many run at once. Zero means one — a limit of none would be a
    /// machine with every bench on it at the same time.
    pub concurrency: usize,
    /// Directory names to skip while looking for benches.
    pub ignore: Vec<String>,
}

/// What happened, as it happens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The full set, before any of it runs. Sent first so a display can size
    /// itself to the work rather than growing as results arrive.
    Discovered { names: Vec<String> },
    /// One bench has started.
    Started { name: String },
    /// One bench is done.
    Finished {
        name: String,
        passed: bool,
        seconds: f64,
        /// Everything the bench wrote, kept for the ones that failed. A
        /// passing bench's output is nobody's business.
        output: String,
    },
    /// Something worth saying that is not a result.
    Note { message: String },
}

/// How to launch one bench.
///
/// The two callers use different binaries — a developer's `vw` and an
/// instance's agent — so the command is supplied rather than assumed.
pub type Launch =
    Arc<dyn Fn(&str, &str) -> tokio::process::Command + Send + Sync>;

/// What a whole run came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("not in a vw workspace (no vw.toml in the parent chain)")]
    NoWorkspace,
    #[error("looking for testbenches under {0}")]
    Discover(camino::Utf8PathBuf, #[source] vw_lib::VwError),
    #[error("generating anodizer structs")]
    Anodize(#[source] vw_lib::VwError),
    #[error("generating bench scaffolds")]
    Scaffold(#[source] vw_lib::VwError),
}

/// The benches a request selects, in the order they will run.
pub fn discover(
    workspace: &Utf8Path,
    request: &Request,
) -> Result<Vec<String>, BenchError> {
    let bench_dir = workspace.join("bench");
    if !bench_dir.exists() {
        return Ok(Vec::new());
    }

    let ignore: HashSet<String> = request.ignore.iter().cloned().collect();
    let benches = vw_lib::list_testbenches(&bench_dir, &ignore, true)
        .map_err(|e| BenchError::Discover(bench_dir.clone(), e))?;

    let mut names: Vec<String> = benches
        .into_iter()
        .map(|t| t.name)
        .filter(|n| n.to_lowercase().ends_with("_tb"))
        .collect();

    // A bench that drives a design entity directly — mixed-signal, or Rust
    // cosim with no VHDL harness — is a directory holding a `mist.toml` or a
    // `cosim.toml`, not a VHDL entity. The entity walk above cannot see one
    // and the `_tb` suffix rule does not apply: the name is the directory's.
    // Without this they are invisible to `vw bench list` and to a bare
    // `vw bench run`, even though `run_testbench` knows how to run one by
    // name.
    let mixed_signal = vw_lib::sim::find_mist_configs(&bench_dir)
        .map_err(|e| BenchError::Discover(bench_dir.clone(), e))?;
    let direct = vw_lib::cosim::find_cosim_configs(&bench_dir)
        .map_err(|e| BenchError::Discover(bench_dir.clone(), e))?;
    names.extend(
        mixed_signal
            .into_iter()
            .map(|(name, _)| name)
            .chain(direct.into_iter().map(|(name, _)| name))
            .filter(|name| !ignore.contains(name)),
    );

    // Applied once, over both kinds, so a filter cannot silently mean
    // different things depending on which sort of bench it matches.
    if let Some(filter) = request.filter.as_deref() {
        names.retain(|n| n.contains(filter));
    }

    names.sort();
    names.dedup();

    Ok(names)
}

/// Put the workspace in a state where every bench can build.
///
/// Done once, before the fan-out, rather than per bench: both steps write
/// generated files into the workspace, and several children doing that at
/// once would race each other over the same paths.
pub async fn prepare(
    workspace: &Utf8Path,
    standard: vw_lib::VhdlStandard,
) -> Result<(), BenchError> {
    vw_lib::ensure_anodized(workspace, standard, None)
        .await
        .map_err(BenchError::Anodize)?;

    // A missing generated `bench/<name>/Cargo.toml` — after a `git clean`,
    // say — stops cargo loading the bench workspace manifest at all, which
    // fails every bench's rust build rather than only the cosim ones.
    vw_lib::ensure_bench_scaffolds(workspace).map_err(BenchError::Scaffold)
}

/// Run `names`, at most `concurrency` at a time, reporting as it goes.
pub async fn run(
    workspace: &Utf8Path,
    names: Vec<String>,
    concurrency: usize,
    launch: Launch,
    report: impl Fn(Event) + Send + Sync + 'static,
) -> Summary {
    let report = Arc::new(report);
    report(Event::Discovered {
        names: names.clone(),
    });

    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut running = Vec::new();

    for name in names {
        let permits = Arc::clone(&permits);
        let report = Arc::clone(&report);
        let launch = Arc::clone(&launch);
        let workspace = workspace.to_owned();

        running.push(tokio::spawn(async move {
            let _permit = permits.acquire().await.expect("semaphore closed");
            report(Event::Started { name: name.clone() });

            let build_dir = format!("{}/{}", vw_lib::BUILD_DIR, name);
            let started = Instant::now();
            let finished = launch(&name, &build_dir)
                .current_dir(workspace.as_std_path())
                .output()
                .await;
            let seconds = started.elapsed().as_secs_f64();

            let (passed, output) = match finished {
                Ok(out) => {
                    let mut text =
                        String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    (out.status.success(), text)
                }
                Err(e) => (false, format!("could not start the bench: {e}")),
            };

            report(Event::Finished {
                name,
                passed,
                seconds,
                output,
            });
            passed
        }));
    }

    let mut summary = Summary::default();
    for handle in running {
        match handle.await {
            Ok(true) => summary.passed += 1,
            // A child that failed, or a task that panicked carrying it. Both
            // are a bench that did not pass, and the batch continues either
            // way — one broken bench should not hide the state of the rest.
            Ok(false) | Err(_) => summary.failed += 1,
        }
    }

    summary
}

#[cfg(test)]
mod test {
    use super::*;

    /// A workspace with one VHDL testbench and one mixed-signal bench.
    fn workspace() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        let bench = root.join("bench");

        std::fs::create_dir_all(&bench).expect("mkdir");
        std::fs::write(
            bench.join("widget_tb.vhd"),
            "entity widget_tb is\nend entity;\n",
        )
        .expect("write");

        // A mixed-signal bench is a directory with a `mist.toml` and no VHDL
        // entity of its own.
        let mixed = bench.join("tx-eq");
        std::fs::create_dir_all(&mixed).expect("mkdir");
        std::fs::write(
            mixed.join("mist.toml"),
            "netlist = \"analog/model.cir\"\nentity = \"TxEq\"\nclock = 26.5625e9\n",
        )
        .expect("write");

        (dir, root)
    }

    fn found(root: &Utf8Path, request: Request) -> Vec<String> {
        discover(root, &request).expect("discover")
    }

    #[test]
    fn a_mixed_signal_bench_is_discovered() {
        // It has no VHDL entity and its name does not end in `_tb`, so both of
        // the rules that find an ordinary testbench miss it. It went missing
        // from `--list` for exactly that reason once already.
        let (_dir, root) = workspace();

        assert_eq!(
            found(&root, Request::default()),
            ["tx-eq", "widget_tb"],
            "both kinds of bench should be listed",
        );
    }

    #[test]
    fn a_filter_matches_either_kind() {
        let (_dir, root) = workspace();

        assert_eq!(
            found(
                &root,
                Request {
                    filter: Some("tx".to_owned()),
                    ..Default::default()
                },
            ),
            ["tx-eq"],
        );
        assert_eq!(
            found(
                &root,
                Request {
                    filter: Some("widget".to_owned()),
                    ..Default::default()
                },
            ),
            ["widget_tb"],
        );
    }

    #[test]
    fn an_ignored_directory_is_not_run_whichever_kind_it_is() {
        let (_dir, root) = workspace();

        assert_eq!(
            found(
                &root,
                Request {
                    ignore: vec!["tx-eq".to_owned()],
                    ..Default::default()
                },
            ),
            ["widget_tb"],
        );
    }
}
