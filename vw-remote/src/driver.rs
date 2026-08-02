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

/// How the agent stores what a build produced.
///
/// Passed in because storing is not this module's business — it knows what was
/// built, not where anything keeps things. Called before the build is reported
/// finished, so a developer whose command has returned can go and fetch what
/// it made.
pub type Uploader = Box<
    dyn Fn(Vec<Utf8PathBuf>) -> futures::future::BoxFuture<'static, usize>
        + Send,
>;

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
    /// The build produced something worth keeping, and it has been stored.
    Produced {
        artifacts: Vec<String>,
        stored: usize,
    },
    /// The build could not be run at all.
    Fatal { message: String },
}

/// What cargo says about something it built.
#[derive(serde::Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoMessage {
    /// A file cargo produced.
    CompilerArtifact {
        package_id: String,
        #[serde(default)]
        filenames: Vec<Utf8PathBuf>,
    },
    /// Something rustc had to say, already formatted the way rustc formats it.
    CompilerMessage { message: RustcMessage },
    #[serde(other)]
    Other,
}

#[derive(serde::Deserialize)]
struct RustcMessage {
    #[serde(default)]
    rendered: Option<String>,
}

/// Whether a file cargo produced is a deliverable rather than an intermediate.
///
/// A driver's outputs are things that run or load — a binary, a kernel module.
/// An `rlib` is scaffolding for the next compilation and means nothing on
/// another machine, and `.d` files are make fragments naming paths that only
/// exist on the builder.
fn is_deliverable(path: &Utf8Path) -> bool {
    !matches!(path.extension(), Some("rlib" | "rmeta" | "d"))
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
    upload: Uploader,
) -> Result<Vec<Utf8PathBuf>, crate::SessionError>
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

    let building = build(root, &params, events.clone(), upload);
    tokio::pin!(building);

    // A build that finishes after the developer has gone still produced
    // something, and it still belongs in the store — but one abandoned part
    // way through produced nothing worth keeping.
    let produced = tokio::select! {
        produced = &mut building => produced,
        _ = departed => {
            tracing::info!("client left; abandoning the build");
            Vec::new()
        }
    };

    drop(events);
    let _ = writer.await;

    Ok(produced)
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
///
/// Returns what it produced, by absolute path, so the caller can put it
/// somewhere it will outlive the instance.
async fn build(
    root: &Utf8Path,
    params: &BuildParams,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
    upload: Uploader,
) -> Vec<Utf8PathBuf> {
    // Said before anything is built, because the alternative is a compiler
    // error several minutes from now that names none of this.
    if let Some(contradiction) = contradiction(root) {
        let _ = events.send(DriverEvent::Fatal {
            message: contradiction,
        });
        return Vec::new();
    }

    // Cargo is asked which of its members are the workspace's, so a
    // dependency's artifacts are not mistaken for the driver's.
    let members: std::collections::HashSet<String> = metadata(root)
        .map(|metadata| metadata.workspace_members.into_iter().collect())
        .unwrap_or_default();

    let mut produced = Vec::new();
    for unit in units(root) {
        let _ = events.send(DriverEvent::Building {
            unit: unit.name.clone(),
        });

        match build_one(&unit.directory, params, &events, &members).await {
            Some((true, mut artifacts)) => produced.append(&mut artifacts),
            // Reported already; there is no sense building the kernel module
            // against a userland that did not compile.
            Some((false, _)) | None => return Vec::new(),
        }
    }

    if !produced.is_empty() {
        // Stored before the build is called finished. Otherwise a developer
        // whose command has just returned successfully would go looking for
        // the artifacts and find nothing, which is a race they have no way to
        // know about.
        let stored = upload(produced.clone()).await;
        let _ = events.send(DriverEvent::Produced {
            artifacts: produced.iter().map(ToString::to_string).collect(),
            stored,
        });
    }

    let _ = events.send(DriverEvent::Done {
        success: true,
        code: Some(0),
    });

    produced
}

/// Spawn cargo in one directory and forward everything it says.
///
/// Returns whether it succeeded, or `None` if it could not be run at all — in
/// which case the failure has already been reported.
async fn build_one(
    root: &Utf8Path,
    params: &BuildParams,
    events: &tokio::sync::mpsc::UnboundedSender<DriverEvent>,
    members: &std::collections::HashSet<String>,
) -> Option<(bool, Vec<Utf8PathBuf>)> {
    let mut command = tokio::process::Command::new("cargo");
    command.arg("build");
    if params.release {
        command.arg("--release");
    }
    // Forced on: cargo turns colour off when its output is not a terminal, and
    // here it never is — but there is a terminal at the far end of this, and
    // it is the one that matters.
    command.args(["--color", "always"]);
    // Structured, so the exact set of files this produced comes from cargo
    // rather than from guessing at target directory layout. Diagnostics still
    // arrive pre-rendered, colour and all, so nothing about what a developer
    // sees changes.
    command.args(["--message-format", "json-diagnostic-rendered-ansi"]);
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

    // The two streams say different things under `--message-format=json`.
    // Cargo's own progress goes to stderr as ordinary text; rustc's
    // diagnostics and the record of what was built come down stdout as JSON.
    // Both are forwarded as they arrive, so the order a developer sees is the
    // order things happened.
    let structured = child.stdout.take().map(|stdout| {
        tokio::spawn(read_messages(stdout, events.clone(), members.clone()))
    });
    let plain = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(forward(stderr, events.clone())));

    let status = child.wait().await;

    // Drained before reporting the result, so the last line of a failing build
    // never arrives after the verdict on it.
    let mut artifacts = Vec::new();
    if let Some(structured) = structured {
        if let Ok(found) = structured.await {
            artifacts = found;
        }
    }
    if let Some(plain) = plain {
        let _ = plain.await;
    }

    match status {
        Ok(status) if status.success() => Some((true, artifacts)),
        Ok(status) => {
            let _ = events.send(DriverEvent::Done {
                success: false,
                code: status.code(),
            });
            Some((false, Vec::new()))
        }
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("waiting for cargo: {e}"),
            });
            None
        }
    }
}

/// Read cargo's structured output, forwarding what a person should see and
/// keeping what was built.
///
/// Only the workspace's own members count. A dependency compiled along the way
/// produces artifacts too, and none of them are the driver.
async fn read_messages<R>(
    reader: R,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
    members: std::collections::HashSet<String>,
) -> Vec<Utf8PathBuf>
where
    R: AsyncRead + Unpin,
{
    let mut produced = Vec::new();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<CargoMessage>(&line) else {
            // Not something we understand. Cargo occasionally writes plain
            // text here, and passing it on is better than swallowing it.
            let _ = events.send(DriverEvent::Line { text: line });
            continue;
        };

        match message {
            CargoMessage::CompilerMessage { message } => {
                if let Some(rendered) = message.rendered {
                    // Already formatted by rustc, so it reads exactly as it
                    // would on the developer's own machine.
                    for text in rendered.lines() {
                        let _ = events.send(DriverEvent::Line {
                            text: text.to_owned(),
                        });
                    }
                }
            }
            CargoMessage::CompilerArtifact {
                package_id,
                filenames,
            } if members.contains(&package_id) => {
                produced.extend(
                    filenames.into_iter().filter(|path| is_deliverable(path)),
                );
            }
            _ => {}
        }
    }

    produced
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
