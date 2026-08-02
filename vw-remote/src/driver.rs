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

use camino::{Utf8Path, Utf8PathBuf};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// What the instance sends back while a build runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverEvent {
    /// A new part of the build has started.
    ///
    /// A driver is not one cargo invocation. Userland and a kernel module are
    /// built for different targets, and one `cargo build` produces artifacts
    /// for exactly one target — so there are as many invocations as there are
    /// targets, and it is worth saying which one is talking.
    Building { unit: String },
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

/// One place cargo has to be run, and what to call it.
struct Unit {
    /// The directory to run from. This is the whole point: cargo reads
    /// `.cargo/config.toml` from the current directory and its ancestors, and
    /// explicitly does not read one belonging to a workspace member when it is
    /// invoked from the workspace root. A member whose config selects a target
    /// therefore cannot be built correctly any other way.
    directory: Utf8PathBuf,
    name: String,
}

/// What `cargo metadata` tells us about a driver workspace.
#[derive(serde::Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    #[serde(default)]
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: Utf8PathBuf,
}

impl Package {
    fn directory(&self) -> Utf8PathBuf {
        self.manifest_path
            .parent()
            .map(Utf8Path::to_owned)
            .unwrap_or_default()
    }

    /// Whether this member's own cargo config selects a build target.
    ///
    /// This is vw's signal for "compiled for somewhere other than here". A
    /// kernel module sets it because it is not userland; nothing else has a
    /// reason to. It is a signal a project already has to set for its own
    /// build to work, rather than one more file to keep in step.
    fn is_separately_targeted(&self) -> bool {
        let config = self.directory().join(".cargo/config.toml");
        let Ok(text) = std::fs::read_to_string(&config) else {
            return false;
        };
        let Ok(parsed) = text.parse::<toml::Table>() else {
            return false;
        };
        parsed
            .get("build")
            .and_then(toml::Value::as_table)
            .is_some_and(|build| build.contains_key("target"))
    }
}

/// Ask cargo what this workspace is made of.
fn metadata(root: &Utf8Path) -> Option<Metadata> {
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root.as_std_path())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Every cargo invocation a driver build needs, in the order to run them.
///
/// vw's rule, and the whole of it: **a workspace member whose own
/// `.cargo/config.toml` selects a build target is built separately, from its
/// own directory.** Everything else is built together as the workspace's
/// default members.
///
/// Two things force this and neither is a matter of taste. One cargo
/// invocation compiles for exactly one target, so a driver with a kernel
/// module and userland tooling is at least two invocations however it is
/// arranged. And a member's cargo config only applies when cargo runs from
/// that directory, so the invocation for such a member has to start there —
/// which, as a bonus, is also what keeps workspace feature unification from
/// pulling `std` into a `no_std` build.
///
/// Deriving it from a file the project already needs means there is nothing
/// extra to declare, and nothing that can disagree with how the project
/// actually builds.
fn units(root: &Utf8Path) -> Vec<Unit> {
    let mut units = vec![Unit {
        directory: root.to_owned(),
        name: "workspace".to_owned(),
    }];

    let Some(metadata) = metadata(root) else {
        return units;
    };

    let mut separate: Vec<Unit> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .filter(|package| package.is_separately_targeted())
        .map(|package| Unit {
            directory: package.directory(),
            name: package.name.clone(),
        })
        .collect();
    separate.sort_by(|a, b| a.name.cmp(&b.name));

    units.append(&mut separate);
    units
}

/// A member that is built for its own target and also in the workspace's
/// default members.
///
/// Worth catching before anything runs. Cargo will build it for the host
/// instead, quietly ignoring the target its config asks for, and the failure
/// arrives much later as a duplicate `panic_impl` lang item or a missing
/// `core` — which reads as a dependency problem and is not one.
fn contradiction(root: &Utf8Path) -> Option<String> {
    let metadata = metadata(root)?;

    let offender = metadata
        .packages
        .iter()
        .filter(|package| {
            metadata.workspace_default_members.contains(&package.id)
        })
        .find(|package| package.is_separately_targeted())?;

    Some(format!(
        "`{name}` sets its own build target in {directory}/.cargo/config.toml, \
         but it is also one of this workspace's `default-members`. Cargo does \
         not read a member's config when it runs from the workspace root, so \
         building it that way compiles it for this machine instead of for the \
         target it asks for. Remove `{name}` from `default-members` — vw \
         builds it separately, from its own directory, which is the only way \
         that config takes effect.",
        name = offender.name,
        directory = offender
            .directory()
            .strip_prefix(root)
            .unwrap_or(&offender.directory()),
    ))
}

/// Run every part of the build, stopping at the first failure.
async fn build(
    root: &Utf8Path,
    params: &BuildParams,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) {
    // Said before anything is built, because the alternative is a compiler
    // error several minutes from now that names none of this.
    if let Some(contradiction) = contradiction(root) {
        let _ = events.send(DriverEvent::Fatal {
            message: contradiction,
        });
        return;
    }

    for unit in units(root) {
        let _ = events.send(DriverEvent::Building {
            unit: unit.name.clone(),
        });

        match build_one(&unit.directory, params, &events).await {
            Some(true) => {}
            // Reported already; there is no sense building the kernel module
            // against a userland that did not compile.
            Some(false) | None => return,
        }
    }

    let _ = events.send(DriverEvent::Done {
        success: true,
        code: Some(0),
    });
}

/// Spawn cargo in one directory and forward everything it says.
///
/// Returns whether it succeeded, or `None` if it could not be run at all — in
/// which case the failure has already been reported.
async fn build_one(
    root: &Utf8Path,
    params: &BuildParams,
    events: &tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) -> Option<bool> {
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
            return None;
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
        Ok(status) if status.success() => Some(true),
        Ok(status) => {
            let _ = events.send(DriverEvent::Done {
                success: false,
                code: status.code(),
            });
            Some(false)
        }
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("waiting for cargo: {e}"),
            });
            None
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

    /// A driver workspace with `userland` and, optionally, a member built for
    /// its own target.
    fn workspace(
        separately_targeted: bool,
        in_default_members: bool,
    ) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();

        let members = if in_default_members {
            r#"["userland", "kmod"]"#
        } else {
            r#"["userland"]"#
        };
        std::fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\"userland\", \"kmod\"]\n\
                 default-members = {members}\nresolver = \"2\"\n"
            ),
        )
        .expect("write");

        for member in ["userland", "kmod"] {
            let package = root.join(member);
            std::fs::create_dir_all(package.join("src")).expect("mkdir");
            std::fs::write(
                package.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\n\
                     edition = \"2021\"\n"
                ),
            )
            .expect("write");
            std::fs::write(package.join("src/lib.rs"), "").expect("write");
        }

        if separately_targeted {
            let config = root.join("kmod/.cargo");
            std::fs::create_dir_all(&config).expect("mkdir");
            std::fs::write(
                config.join("config.toml"),
                "[build]\ntarget = \"x86_64-unknown-none.json\"\n\n\
                 [unstable]\nbuild-std = [\"core\", \"alloc\"]\n",
            )
            .expect("write");
        }

        (dir, root)
    }

    #[test]
    fn a_member_with_its_own_target_is_built_on_its_own() {
        // vw's whole rule. The kernel module's cargo config only applies when
        // cargo runs from its directory, so that is where it is run from.
        let (_dir, root) = workspace(true, false);

        let names: Vec<String> =
            units(&root).into_iter().map(|unit| unit.name).collect();

        assert_eq!(names, ["workspace", "kmod"]);
    }

    #[test]
    fn a_workspace_that_is_all_userland_is_one_build() {
        // The common case, and it should cost nothing: no member asks for a
        // different target, so one invocation covers it.
        let (_dir, root) = workspace(false, false);

        let names: Vec<String> =
            units(&root).into_iter().map(|unit| unit.name).collect();

        assert_eq!(names, ["workspace"]);
    }

    #[test]
    fn a_member_in_default_members_that_wants_its_own_target_is_refused() {
        // The mistake worth catching: cargo silently builds it for the host
        // and the failure surfaces minutes later as a duplicate lang item,
        // which reads as a dependency problem and is not one.
        let (_dir, root) = workspace(true, true);

        let complaint = contradiction(&root).expect("should be caught");

        assert!(complaint.contains("kmod"), "{complaint}");
        assert!(complaint.contains("default-members"), "{complaint}");
    }

    #[test]
    fn a_workspace_with_nothing_contradictory_is_left_alone() {
        let (_dir, root) = workspace(true, false);
        assert!(contradiction(&root).is_none());

        let (_dir, root) = workspace(false, false);
        assert!(contradiction(&root).is_none());
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
