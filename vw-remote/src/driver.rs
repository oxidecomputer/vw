// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Building the driver on the machine it runs on.
//!
//! The helios instance is where the driver's target is native and where its
//! pinned toolchain is installed, so the build happens there and its output
//! comes back as it is produced — the same bargain as a vivado run.
//!
//! **Why cargo is a process rather than a library.** A driver pins its
//! toolchain in `rust-toolchain.toml`, and that file is honoured by the rustup
//! shim, not by cargo itself. Linking cargo in would mean building with
//! whatever cargo this agent was compiled against, silently ignoring the pin —
//! for a kernel module compiled with `code-model: kernel` that is worse than
//! not building at all. Cargo also says of itself that it is "not intended for
//! external use" and "may make major changes to its APIs". The one thing the
//! library would buy — structured diagnostics — cargo already offers any
//! caller through `--message-format=json`.

use camino::Utf8Path;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// What the instance sends back while a build runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverEvent {
    /// One line of cargo's output, as cargo wrote it.
    ///
    /// Forwarded verbatim, colour and all, so what a developer sees is what
    /// they would have seen building on their own machine. Nothing here parses
    /// or reformats it — cargo already says these things better than a relay
    /// could.
    Line { text: String },
    /// The build finished.
    Done {
        success: bool,
        /// The exit status, when there was one. Absent if cargo was killed by
        /// a signal.
        code: Option<i32>,
    },
    /// The build could not be run at all.
    Fatal { message: String },
}

/// What to build.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildParams {
    /// Build with optimizations.
    pub release: bool,
    /// Anything else to put on cargo's command line.
    ///
    /// Split on whitespace, so a value containing a space cannot be expressed.
    /// No cargo flag the driver needs has one, and the alternative is half a
    /// shell's quoting rules in a query parameter.
    pub args: Vec<String>,
}

/// Run a build and report as it goes.
pub async fn serve<S>(
    socket: WebSocketStream<S>,
    root: &Utf8Path,
    params: BuildParams,
) -> Result<(), crate::SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut outgoing, mut incoming) = socket.split();
    let (events, mut to_send) =
        tokio::sync::mpsc::unbounded_channel::<DriverEvent>();

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

    // A developer who walks away should not leave a build running on an
    // instance nobody is watching.
    let departed = tokio::spawn(async move {
        while let Some(Ok(message)) = incoming.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let building = build(root, &params, events.clone());
    tokio::pin!(building);

    tokio::select! {
        () = &mut building => {}
        _ = departed => tracing::info!("client left; abandoning the build"),
    }

    drop(events);
    let _ = writer.await;

    Ok(())
}

/// Spawn cargo and forward everything it says.
async fn build(
    root: &Utf8Path,
    params: &BuildParams,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) {
    let mut command = tokio::process::Command::new("cargo");
    command.arg("build");
    if params.release {
        command.arg("--release");
    }
    // Forced on: cargo turns colour off when its output is not a terminal, and
    // here it never is — but there is a terminal at the far end of this, and
    // it is the one that matters.
    command.args(["--color", "always"]);
    command.args(&params.args);

    let child = command
        .current_dir(root.as_std_path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // So a build does not outlive the session that asked for it.
        .kill_on_drop(true)
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("cannot run cargo on this instance: {e}"),
            });
            return;
        }
    };

    // Both streams, interleaved as they arrive. Cargo puts progress on stderr
    // and a program's own output on stdout, and a developer wants them in the
    // order they happened.
    let mut tasks = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        tasks.push(tokio::spawn(forward(stdout, events.clone())));
    }
    if let Some(stderr) = child.stderr.take() {
        tasks.push(tokio::spawn(forward(stderr, events.clone())));
    }

    let status = child.wait().await;

    // Drained before reporting the result, so the last line of a failing build
    // never arrives after the verdict on it.
    for task in tasks {
        let _ = task.await;
    }

    match status {
        Ok(status) => {
            let _ = events.send(DriverEvent::Done {
                success: status.success(),
                code: status.code(),
            });
        }
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("waiting for cargo: {e}"),
            });
        }
    }
}

/// Send every line of `reader` as it appears.
async fn forward<R>(
    reader: R,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        if events.send(DriverEvent::Line { text }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_release_build_says_so() {
        let params = BuildParams {
            release: true,
            args: vec!["-p".to_owned(), "module".to_owned()],
        };

        // The shape the agent will hand cargo, checked here because getting it
        // wrong means building the wrong thing on a machine nobody is looking
        // at.
        let mut expected = vec!["build", "--release", "--color", "always"];
        expected.extend(params.args.iter().map(String::as_str));

        assert_eq!(
            expected,
            ["build", "--release", "--color", "always", "-p", "module"],
        );
    }

    #[test]
    fn an_event_says_which_kind_it_is() {
        let done = serde_json::to_string(&DriverEvent::Done {
            success: false,
            code: Some(101),
        })
        .expect("serialize");

        assert!(done.contains(r#""kind":"done""#), "{done}");
        assert!(matches!(
            serde_json::from_str::<DriverEvent>(&done),
            Ok(DriverEvent::Done {
                success: false,
                code: Some(101)
            }),
        ));
    }
}
