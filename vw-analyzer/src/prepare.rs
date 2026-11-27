// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Keeping a workspace's generated Rust inputs current for the editor.
//!
//! rust-analyzer cannot load a vw workspace's Rust until `vw editor prepare`
//! has generated the mixed-signal benches' crates and fetched current
//! anodizer output. Nobody should have to remember to run that, and the vw
//! analyzer is already running in the editor and already watching the files
//! those inputs come from — so it runs it: once per workspace when the editor
//! starts, and again whenever a `mist.toml`, the workspace manifest, or a VHDL
//! source carrying `serialize_rust` changes.
//!
//! **Why a child process rather than a function call.** This server's stdout
//! is the LSP channel, and preparing a workspace syncs to the cloud, anodizes
//! and downloads — all of which print. A child's output is captured whole, so
//! none of it can corrupt the protocol; a slow or failing cloud call cannot
//! stall the server; and the analyzer does not have to link the cloud client
//! to do any of it. `vw editor prepare --json` ends with a one-line summary
//! on its stdout, which is all this reads from it; everything else it said
//! goes to the editor's LSP log.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use tower_lsp::lsp_types::notification::Progress;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tracing::info;

/// How long to wait before running, so a burst of changes — a branch switch,
/// a save-all — becomes one run rather than many.
const SETTLE: Duration = Duration::from_millis(1500);

/// What `vw editor prepare --json` says it did. Only the parts acted on here;
/// the rest of the summary is ignored.
#[derive(Debug, Default, serde::Deserialize)]
struct Summary {
    #[serde(default)]
    anodized: String,
    #[serde(default)]
    problems: Vec<String>,
}

impl Summary {
    /// Whether anodizer's output on disk changed. rust-analyzer only reads
    /// it when it runs build scripts, which is at load, so it will not see
    /// the change until it is restarted.
    fn changed(&self) -> bool {
        matches!(self.anodized.as_str(), "fetched" | "generated")
    }
}

/// The state of preparing one workspace.
#[derive(Debug, Default)]
struct Run {
    /// A run is in flight.
    running: bool,
    /// Something changed while it was; run once more when it finishes.
    again: bool,
    /// What was last shown, so the same complaint is not shown again on
    /// every save. Still in the log each time.
    shown: Option<String>,
}

/// Runs `vw editor prepare` for the editor's workspaces.
pub struct Preparer {
    client: Client,
    /// The `vw` to run: this very binary under `vw analyzer`, or whatever
    /// `vw` is on the path for the standalone `vw-analyzer`.
    program: PathBuf,
    runs: Mutex<HashMap<Utf8PathBuf, Run>>,
    progress: AtomicU64,
}

impl Preparer {
    pub fn new(client: Client, program: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            client,
            program,
            runs: Mutex::new(HashMap::new()),
            progress: AtomicU64::new(0),
        })
    }

    /// Whether `workspace` has been prepared, or is being, this session.
    pub fn seen(&self, workspace: &Utf8Path) -> bool {
        self.runs
            .lock()
            .map(|runs| runs.contains_key(workspace))
            .unwrap_or(true)
    }

    /// Prepare `workspace` soon, if it has any Rust to prepare.
    ///
    /// At most one run per workspace at a time. Asking while one is in
    /// flight queues exactly one more, which sees everything that changed in
    /// the meantime.
    pub fn request(self: &Arc<Self>, workspace: Utf8PathBuf) {
        if !has_rust(&workspace) {
            return;
        }
        {
            let Ok(mut runs) = self.runs.lock() else {
                return;
            };
            let run = runs.entry(workspace.clone()).or_default();
            if run.running {
                run.again = true;
                return;
            }
            run.running = true;
        }

        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SETTLE).await;
                this.run_once(&workspace).await;

                let Ok(mut runs) = this.runs.lock() else {
                    return;
                };
                let Some(run) = runs.get_mut(&workspace) else {
                    return;
                };
                if run.again {
                    run.again = false;
                    continue;
                }
                run.running = false;
                return;
            }
        });
    }

    async fn run_once(&self, workspace: &Utf8Path) {
        let name = workspace.file_name().unwrap_or(workspace.as_str());
        info!(%workspace, "preparing Rust for the editor");
        let token = self.begin(format!("vw: preparing {name}")).await;

        let output = tokio::process::Command::new(&self.program)
            .args(["editor", "prepare", "--json"])
            .current_dir(workspace.as_std_path())
            // Colour codes are noise in an editor's log.
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;

        self.end(token).await;

        let output = match output {
            Ok(output) => output,
            Err(e) => {
                // Most likely no `vw` on the path under the standalone
                // binary. Said once, in the log: an editor without vw is not
                // something to nag about.
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "vw-analyzer: cannot run `{} editor prepare`: {e}",
                            self.program.display()
                        ),
                    )
                    .await;
                return;
            }
        };

        let said = String::from_utf8_lossy(&output.stderr);
        if !said.trim().is_empty() {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("vw editor prepare ({workspace}):\n{said}"),
                )
                .await;
        }

        let summary = String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Summary>(line).ok());

        let message = match &summary {
            // It never got as far as a summary, so it failed outright — no
            // environment, no network. Its last words say why.
            None => Some((
                MessageType::WARNING,
                format!(
                    "vw: preparing {name} for rust-analyzer failed: {}",
                    said.lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("no reason given")
                        .trim(),
                ),
            )),
            Some(summary) if summary.anodized == "failed" => Some((
                MessageType::WARNING,
                format!(
                    "vw: anodizing {name} failed, so rust-analyzer has no \
                     current anodizer output; the log says why"
                ),
            )),
            Some(summary) if !summary.problems.is_empty() => Some((
                MessageType::WARNING,
                format!(
                    "vw: {}{}",
                    summary.problems[0],
                    match summary.problems.len() {
                        1 => String::new(),
                        n => format!(" (and {} more; see the log)", n - 1),
                    }
                ),
            )),
            Some(summary) if summary.changed() => Some((
                MessageType::INFO,
                format!(
                    "vw: anodizer output for {name} updated; restart \
                     rust-analyzer to pick it up"
                ),
            )),
            Some(_) => None,
        };

        let fresh = {
            let Ok(mut runs) = self.runs.lock() else {
                return;
            };
            let run = runs.entry(workspace.to_owned()).or_default();
            let text = message.as_ref().map(|(_, text)| text.clone());
            // An update is news every time it happens; a complaint only the
            // first time it is made.
            let changed = summary.as_ref().is_some_and(Summary::changed);
            let fresh = changed || text != run.shown;
            run.shown = text;
            fresh
        };
        if let (Some((kind, text)), true) = (message, fresh) {
            self.client.show_message(kind, text).await;
        }
    }

    /// Start an editor progress indicator, if the editor takes them.
    async fn begin(&self, title: String) -> Option<NumberOrString> {
        let token = NumberOrString::String(format!(
            "vw-prepare-{}",
            self.progress.fetch_add(1, Ordering::Relaxed)
        ));
        self.client
            .send_request::<WorkDoneProgressCreate>(
                WorkDoneProgressCreateParams {
                    token: token.clone(),
                },
            )
            .await
            .ok()?;
        self.client
            .send_notification::<Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title,
                        ..Default::default()
                    },
                )),
            })
            .await;
        Some(token)
    }

    async fn end(&self, token: Option<NumberOrString>) {
        if let Some(token) = token {
            self.client
                .send_notification::<Progress>(ProgressParams {
                    token,
                    value: ProgressParamsValue::WorkDone(
                        WorkDoneProgress::End(WorkDoneProgressEnd::default()),
                    ),
                })
                .await;
        }
    }
}

/// Whether a workspace has Rust an editor would load: a driver, or benches.
/// A purely HDL workspace has nothing to prepare, and should not pay for
/// trying.
fn has_rust(workspace: &Utf8Path) -> bool {
    workspace.join("driver/Cargo.toml").is_file()
        || workspace.join("bench/Cargo.toml").is_file()
}

/// Whether a change to `path` can change what preparing produces.
///
/// The workspace manifest and lockfile pin the dependencies the anodizer
/// fingerprint covers; a `mist.toml` is what a bench's crate is generated
/// from; and a VHDL source carrying `serialize_rust` is where the records
/// anodizer generates come from. Other VHDL edits are left out on purpose:
/// they happen constantly, and re-anodizing in the cloud on every one would
/// make editing HDL cost a round trip. Whatever they change is picked up by
/// the next run that does happen, which checks the whole fingerprint.
pub fn affects(path: &Utf8Path, kind: FileChangeType) -> bool {
    match path.file_name() {
        Some("vw.toml" | "vw.lock" | "mist.toml") => true,
        _ => match path.extension() {
            Some("vhd" | "vhdl") => {
                kind == FileChangeType::DELETED
                    || std::fs::read_to_string(path)
                        .is_ok_and(|text| text.contains("serialize_rust"))
            }
            _ => false,
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn only_what_generated_rust_comes_from_matters() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let tagged = root.join("tagged.vhd");
        std::fs::write(
            &tagged,
            "attribute serialize_rust of t : type is true;",
        )
        .unwrap();
        let plain = root.join("plain.vhd");
        std::fs::write(&plain, "entity e is end;").unwrap();

        let changed = FileChangeType::CHANGED;
        assert!(affects(&root.join("vw.toml"), changed));
        assert!(affects(&root.join("bench/x/mist.toml"), changed));
        assert!(affects(&tagged, changed));
        assert!(!affects(&plain, changed));
        assert!(affects(&plain, FileChangeType::DELETED));
        assert!(!affects(&root.join("driver/src/lib.rs"), changed));
    }

    #[test]
    fn an_update_is_news_and_current_output_is_not() {
        let fetched: Summary =
            serde_json::from_str(r#"{"anodized":"fetched","problems":[]}"#)
                .unwrap();
        let current: Summary =
            serde_json::from_str(r#"{"anodized":"current","problems":[]}"#)
                .unwrap();
        assert!(fetched.changed());
        assert!(!current.changed());
    }
}
