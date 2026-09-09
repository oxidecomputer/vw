// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Anodizing a workspace on the instance that holds it, on purpose.
//!
//! Anodization already happens on its own before every `vw bench run` —
//! quietly, cached against a fingerprint of the design sources, and
//! deliberately invisible when there is nothing to do. That is right for a
//! build and useless for working on the anodizer itself, where the questions
//! are "did it run", "what did it emit" and "does what it emitted compile".
//!
//! So this is the same generator with the cache turned off and the answers
//! reported. It runs where the design is, for the same reason the benches do:
//! anodization is an nvc pass over the workspace's VHDL, and the workspace is
//! on the instance.
//!
//! **Why the bench build is part of it.** Anodizer failing is easy to see.
//! Anodizer *succeeding* and emitting Rust that does not compile is not — it
//! surfaces much later, as a bench that will not build, with an error
//! pointing into a generated file nobody wrote. Compiling one bench against
//! the fresh output turns that into the answer to the question that was
//! asked.

use camino::Utf8Path;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// What to anodize, and what to check it against.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnodizeRequest {
    /// The bench crate to build against the result.
    ///
    /// Optional in the protocol though not on the command line, so that a
    /// caller that only wants the generator run has a way to say so without a
    /// second endpoint.
    pub bench: Option<String>,
    /// The VHDL standard, as `nvc` spells it.
    pub standard: String,
}

/// What the instance sends back while anodizing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnodizeEvent {
    /// The generator is about to run, over this much.
    Anodizing { sources: usize, tagged: usize },
    /// It finished and wrote this.
    Generated { path: String, lines: usize },
    /// Nothing was tagged, so there was nothing to generate.
    ///
    /// Distinct from a failure and from success: a developer who has just
    /// added `attribute serialize_rust` and gets this has learnt that the
    /// file they edited is not in the design set, which no other outcome
    /// tells them.
    NothingTagged { sources: usize },
    /// A bench is being compiled against what was just generated.
    Building { bench: String },
    /// The bench compiled, but nothing in it refers to the generated file.
    ///
    /// Worth saying rather than passing for success: a green build that never
    /// touched the generated Rust has not checked the thing this command
    /// exists to check, and a developer who reads it as "the anodizer's
    /// output compiles" has been misled by their own tool.
    NotExercised { bench: String },
    /// One line of cargo's output, as cargo wrote it.
    Line { text: String },
    /// Everything finished.
    Done { success: bool },
    /// It could not be run at all.
    Fatal { message: String },
}

/// Anodize `root`, then build `request.bench` against the result.
pub async fn serve<S>(
    socket: WebSocketStream<S>,
    root: &Utf8Path,
    request: AnodizeRequest,
) -> Result<(), crate::SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut outgoing, mut incoming) = socket.split();
    let (events, mut to_send) =
        tokio::sync::mpsc::unbounded_channel::<AnodizeEvent>();

    // One task writes, as elsewhere: cargo's output arrives while the build
    // is still running and has to go out then, not after.
    let writer = tokio::spawn(async move {
        while let Some(event) = to_send.recv().await {
            let Ok(text) = serde_json::to_string(&event) else {
                continue;
            };
            if outgoing.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        let _ = outgoing.close().await;
    });

    // A developer who walks away should not leave an nvc pass and a cargo
    // build running on a machine nobody is watching.
    let departed = tokio::spawn(async move {
        while let Some(Ok(message)) = incoming.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let running = run(root, &request, events.clone());
    tokio::pin!(running);

    tokio::select! {
        () = &mut running => {}
        _ = departed => {
            tracing::info!("client left; abandoning the anodization");
        }
    }

    drop(events);
    let _ = writer.await;
    Ok(())
}

/// Do the work, reporting as it goes.
///
/// Public because `vw --local` runs exactly this, with the events going to a
/// terminal instead of a socket. One implementation for both, so anodizing
/// here and anodizing on an instance cannot drift apart — the same bargain
/// `vw-bench` makes.
pub async fn run(
    root: &Utf8Path,
    request: &AnodizeRequest,
    events: tokio::sync::mpsc::UnboundedSender<AnodizeEvent>,
) {
    let standard = match request.standard.parse::<vw_lib::VhdlStandard>() {
        Ok(standard) => standard,
        Err(e) => {
            let _ = events.send(AnodizeEvent::Fatal {
                message: e.to_string(),
            });
            return;
        }
    };

    // Said before the generator runs, not after: a pass that fails is exactly
    // when knowing how much it was looking at matters, and a report that only
    // arrives on success cannot say it then.
    let (sources, tagged) = match vw_lib::anodize_scope(root, None) {
        Ok(scope) => scope,
        Err(e) => {
            let _ = events.send(AnodizeEvent::Fatal {
                message: causes(&e),
            });
            return;
        }
    };
    if tagged == 0 {
        let _ = events.send(AnodizeEvent::NothingTagged { sources });
        let _ = events.send(AnodizeEvent::Done { success: true });
        return;
    }
    let _ = events.send(AnodizeEvent::Anodizing { sources, tagged });

    let report =
        match vw_lib::anodize(root, standard, None, vw_lib::Freshness::Always)
            .await
        {
            Ok(report) => report,
            Err(e) => {
                let _ = events.send(AnodizeEvent::Fatal {
                    message: causes(&e),
                });
                return;
            }
        };

    if let Some(generated) = &report.generated {
        let _ = events.send(AnodizeEvent::Generated {
            path: relative(root, generated),
            lines: report.lines,
        });
    }

    let Some(bench) = request.bench.as_deref() else {
        let _ = events.send(AnodizeEvent::Done { success: true });
        return;
    };

    let success = build_bench(root, bench, &events).await;
    if success && !refers_to_generated(root, bench) {
        let _ = events.send(AnodizeEvent::NotExercised {
            bench: bench.to_string(),
        });
    }
    let _ = events.send(AnodizeEvent::Done { success });
}

/// The module name a crate reaches the generated structs through.
///
/// Anodizer's output is included with a `#[path]` module rather than linked,
/// so a crate that uses it names this somewhere in its sources.
const GENERATED_MODULE: &str = "generated_structs";

/// Whether building `bench` would actually compile the generated file.
///
/// Checked by looking at the bench crate's own sources and those of the
/// crates it reaches by path — which is how a workspace shares one copy of
/// the generated module between several benches. Anything further out is not
/// followed: this exists to catch "you have not wired it up yet", and a
/// dependency three crates away that pulls it in is not that.
fn refers_to_generated(root: &Utf8Path, bench: &str) -> bool {
    let crate_dir = root.join("bench").join(bench);
    if mentions_generated(&crate_dir) {
        return true;
    }

    let Ok(manifest) =
        std::fs::read_to_string(crate_dir.join("Cargo.toml").as_std_path())
    else {
        return false;
    };
    let Ok(manifest) = manifest.parse::<toml::Value>() else {
        return false;
    };
    let Some(dependencies) =
        manifest.get("dependencies").and_then(toml::Value::as_table)
    else {
        return false;
    };

    dependencies
        .values()
        .filter_map(|dep| dep.get("path")?.as_str())
        .any(|path| mentions_generated(&crate_dir.join(path)))
}

/// Whether anything under this crate's `src/` names the generated module.
fn mentions_generated(crate_dir: &Utf8Path) -> bool {
    fn walk(dir: &Utf8Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
            return false;
        };
        for entry in entries.flatten() {
            let Ok(path) = camino::Utf8PathBuf::from_path_buf(entry.path())
            else {
                continue;
            };
            let found = if path.is_dir() {
                walk(&path)
            } else {
                path.extension() == Some("rs")
                    && std::fs::read_to_string(path.as_std_path())
                        .is_ok_and(|text| text.contains(GENERATED_MODULE))
            };
            if found {
                return true;
            }
        }
        false
    }
    walk(&crate_dir.join("src"))
}

/// Compile one bench crate against what was just generated.
///
/// Cargo is spawned rather than linked for the same reason it is everywhere
/// else here: the bench workspace pins its own toolchain, and only the rustup
/// shim honours that.
async fn build_bench(
    root: &Utf8Path,
    bench: &str,
    events: &tokio::sync::mpsc::UnboundedSender<AnodizeEvent>,
) -> bool {
    let crate_dir = root.join("bench").join(bench);
    if !crate_dir.join("Cargo.toml").is_file() {
        let _ = events.send(AnodizeEvent::Fatal {
            message: format!(
                "bench/{bench} is not a Rust testbench — it has no Cargo.toml"
            ),
        });
        return false;
    }

    let _ = events.send(AnodizeEvent::Building {
        bench: bench.to_string(),
    });

    let mut command = tokio::process::Command::new("cargo");
    command
        .arg("build")
        .current_dir(crate_dir.as_std_path())
        // Cargo colours its output when it believes it is talking to a
        // terminal, and over a socket it does not. Asking for it anyway is
        // what makes a remote build read like a local one.
        .env("CARGO_TERM_COLOR", "always")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // A bench workspace pins its own toolchain — anodizer's output needs
    // nightly — and rustup resolves that from the directory being built in.
    // But cargo sets these for anything it spawns, and `RUSTUP_TOOLCHAIN`
    // beats the directory: inherited, it silently builds the bench with
    // whatever toolchain launched this instead of the one the bench asked
    // for. Nothing sets them outside a cargo invocation, so clearing them
    // costs nothing and restores the pin when something does.
    for leaked in ["RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC", "CARGO"] {
        command.env_remove(leaked);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = events.send(AnodizeEvent::Fatal {
                message: format!("running cargo: {e}"),
            });
            return false;
        }
    };

    // Both streams, interleaved as they arrive: cargo says almost everything
    // worth reading on stderr, and a build that printed only one of them
    // would be missing either the diagnostics or the progress.
    let mut tasks = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        tasks.push(forward(stdout, events.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tasks.push(forward(stderr, events.clone()));
    }

    let status = child.wait().await;
    for task in tasks {
        let _ = task.await;
    }

    matches!(status, Ok(status) if status.success())
}

/// Forward a child's output, one line per event.
fn forward<R>(
    stream: R,
    events: tokio::sync::mpsc::UnboundedSender<AnodizeEvent>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(text)) = lines.next_line().await {
            if events.send(AnodizeEvent::Line { text }).is_err() {
                break;
            }
        }
    })
}

/// A generated path as the developer would refer to it.
fn relative(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string()
}

/// An error and everything under it, on one line.
///
/// Anodizer failures nest — a codegen error inside an nvc failure inside a
/// workspace error — and only the innermost one says what is actually wrong.
fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}
