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

/// The directory a vw workspace keeps its driver in.
///
/// The whole workspace is synchronized to both of an environment's instances,
/// so the helios instance holds a copy of all of it and this is what picks out
/// the part that is a cargo workspace. Named here, beside the code that builds
/// it, because everything else that needs the name — `vw driver build
/// --local`, `vw sources driver`, the agent handing this module a root —
/// needs it for exactly this reason.
pub const DIRECTORY: &str = "driver";

/// How the agent stores what a build produced.
///
/// Passed in because storing is not this module's business — it knows what was
/// built, not where anything keeps things. Called before the build is reported
/// finished, so a developer whose command has returned can go and fetch what
/// it made.
pub type Uploader = Box<
    dyn Fn(Vec<Utf8PathBuf>) -> futures::future::BoxFuture<'static, Stored>
        + Send,
>;

/// What a build needs from elsewhere in the environment before cargo runs.
///
/// Passed in for the same reason [`Uploader`] is: fetching things is the
/// agent's business. Answers with what to add to cargo's environment and
/// anything worth telling the developer about where it came from — or, when
/// something the build needs cannot be had, why, which is reported in place of
/// a build that could only fail further in.
pub type Preparer = Box<
    dyn FnOnce()
            -> futures::future::BoxFuture<'static, Result<Prepared, String>>
        + Send,
>;

/// What [`Preparer`] came back with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prepared {
    /// Variables to set for every cargo invocation.
    pub env: Vec<(String, String)>,
    /// Lines for the developer, said before the build starts.
    pub notes: Vec<String>,
}

/// What became of storing a build's artifacts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stored {
    /// How many made it to the store.
    pub count: usize,
    /// Why the rest did not, one line each. The developer cannot see this
    /// instance's log, so the reason has to travel with the result.
    pub problems: Vec<String>,
}

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
        /// Why anything in `artifacts` was not stored. Defaulted so a client
        /// and an instance one version apart still understand each other.
        #[serde(default)]
        problems: Vec<String>,
    },
    /// The build could not be run at all.
    Fatal { message: String },
    /// One of rustc's diagnostics, for an editor.
    ///
    /// Cargo's `compiler-message` object as cargo wrote it, except that the
    /// instance's workspace root is replaced by [`WORKSPACE_MARKER`] wherever
    /// it appears — span file names, package ids, manifest paths — for the
    /// client to replace with its own. Files outside the workspace, a registry
    /// crate or the standard library, keep their absolute paths, which name
    /// nothing on the client and say so honestly. Only sent when
    /// [`BuildParams::diagnostics`] asked for it.
    Diagnostic { message: serde_json::Value },
    /// A file formatting changed, as it now reads.
    ///
    /// Sent whole rather than as a diff, for the reason synchronization sends
    /// whole files: source is small, and applying a diff is one more thing to
    /// get wrong. `before` is the digest of what the instance formatted, so
    /// the client can tell whether its own copy is still that — a developer
    /// who kept editing while this ran must not have the edit overwritten
    /// with a formatting of what was there before it.
    Formatted {
        /// Relative to the driver's directory, `/`-separated.
        path: String,
        /// Hex blake3, as synchronization spells a digest.
        before: String,
        contents: String,
    },
}

/// What cargo says about something it built.
#[derive(serde::Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoMessage {
    /// A file cargo produced.
    CompilerArtifact {
        package_id: String,
        target: CargoTarget,
        #[serde(default)]
        filenames: Vec<Utf8PathBuf>,
    },
    /// Something rustc had to say, already formatted the way rustc formats it.
    CompilerMessage { message: RustcMessage },
    #[serde(other)]
    Other,
}

/// What a produced file was built as.
#[derive(serde::Deserialize)]
struct CargoTarget {
    #[serde(default)]
    kind: Vec<String>,
}

impl CargoTarget {
    /// Whether this is a build script rather than something the driver is
    /// made of.
    ///
    /// Cargo compiles every `build.rs` into a binary and reports it like any
    /// other artifact. It is a step in the build, not a product of it — it
    /// runs once on the machine that compiled it and means nothing anywhere
    /// else, while weighing tens of megabytes unstripped.
    fn is_build_script(&self) -> bool {
        self.kind.iter().any(|kind| kind == "custom-build")
    }
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

/// Which cargo command to run over the driver.
///
/// The commands a driver is developed with. They differ in what they need and
/// what they leave behind, which is why this is a closed set rather than any
/// cargo subcommand: each answer below is one the instance has to get right
/// on a machine nobody is looking at.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    #[default]
    Build,
    Check,
    Clippy,
    Test,
    /// Format, and send back every file that changed.
    ///
    /// The instance's copy is a mirror, so formatting it alone would change
    /// nothing a developer keeps: what matters is the content that comes back
    /// as [`DriverEvent::Formatted`], for the client to write into the tree
    /// it actually works in. The instance's copy is left formatted, which is
    /// harmless — the next sync makes it match the developer's either way.
    Fmt,
    /// `cargo fmt --check`: report, rewrite nothing.
    FmtCheck,
}

impl Command {
    /// Whether this compiles the driver, and so needs everything compiling
    /// it needs: anodizer output, and members built for their own targets
    /// built from their own directories.
    ///
    /// Formatting reads source and nothing else — rustfmt does not expand
    /// `include!`, so it neither needs anodizer output nor has an opinion
    /// about targets.
    pub fn compiles(self) -> bool {
        !matches!(self, Command::Fmt | Command::FmtCheck)
    }

    /// Whether what this produces is worth keeping. Only a build's is: a lint
    /// or a check makes metadata, and a test binary has done its job by the
    /// time it exits.
    pub fn stores(self) -> bool {
        matches!(self, Command::Build)
    }

    /// Whether a member compiled for its own target takes part.
    ///
    /// It does for anything that only compiles. It cannot for `test`, which
    /// has to run what it compiles, and such a member is by vw's definition
    /// compiled for somewhere other than here — a kernel module has no test
    /// harness to run.
    fn includes_separately_targeted(self) -> bool {
        !matches!(self, Command::Test | Command::Fmt | Command::FmtCheck)
    }

    /// The arguments that start cargo's command line.
    fn subcommand(self) -> &'static [&'static str] {
        match self {
            Command::Build => &["build"],
            Command::Check => &["check"],
            Command::Clippy => &["clippy"],
            Command::Test => &["test"],
            Command::Fmt => &["fmt"],
            Command::FmtCheck => &["fmt", "--check"],
        }
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Build => "build",
            Command::Check => "check",
            Command::Clippy => "clippy",
            Command::Test => "test",
            Command::Fmt => "fmt",
            Command::FmtCheck => "fmt --check",
        })
    }
}

/// What to run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildParams {
    /// The cargo command. Defaulted so a request from before there was a
    /// choice still means what it meant: a build.
    #[serde(default)]
    pub command: Command,
    /// Compile with optimizations. Ignored by `fmt`, which compiles nothing.
    pub release: bool,
    /// Anything else to put on cargo's command line.
    ///
    /// Split on whitespace, so a value containing a space cannot be expressed.
    /// No cargo flag the driver needs has one, and the alternative is half a
    /// shell's quoting rules in a query parameter.
    pub args: Vec<String>,
    /// Report diagnostics for an editor: each as cargo's `compiler-message`
    /// JSON in a [`DriverEvent::Diagnostic`], and every unit run even when
    /// one before it failed.
    #[serde(default)]
    pub diagnostics: bool,
}

/// Run a build and report as it goes.
pub async fn serve<S>(
    socket: WebSocketStream<S>,
    root: &Utf8Path,
    params: BuildParams,
    prepare: Option<Preparer>,
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

    let building = build(root, &params, prepare, events.clone(), upload);
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
fn units(root: &Utf8Path, command: Command) -> Vec<Unit> {
    let mut units = vec![Unit {
        directory: root.to_owned(),
        name: "workspace".to_owned(),
    }];

    // Formatting from the root covers every member, whatever it targets, so
    // there is nothing to split. Testing cannot include a member built for
    // somewhere else. Either way the workspace invocation is the whole of it.
    if !command.includes_separately_targeted() {
        return units;
    }

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
    prepare: Option<Preparer>,
    events: tokio::sync::mpsc::UnboundedSender<DriverEvent>,
    upload: Uploader,
) -> Vec<Utf8PathBuf> {
    // The whole workspace is on this instance and only part of it is the
    // driver, so a workspace that has none reaches here as a directory that
    // is not there. Said plainly, because what cargo would otherwise fail to
    // spawn with is a bare "no such file or directory" naming a path on a
    // machine the developer is not looking at.
    if !root.is_dir() {
        let _ = events.send(DriverEvent::Fatal {
            message: format!(
                "there is nothing to build: this workspace has no \
                 `{DIRECTORY}` directory, which is where a vw workspace keeps \
                 its driver"
            ),
        });
        return Vec::new();
    }

    // Said before anything is built, because the alternative is a compiler
    // error several minutes from now that names none of this. Only for what
    // compiles: formatting does not care which target a member is for.
    if let Some(contradiction) = params
        .command
        .compiles()
        .then(|| contradiction(root))
        .flatten()
    {
        let _ = events.send(DriverEvent::Fatal {
            message: contradiction,
        });
        return Vec::new();
    }

    // Before cargo, so a missing input is reported as itself rather than as a
    // compile error naming a file the developer never wrote.
    let mut env = Vec::new();
    if let Some(prepare) = prepare.filter(|_| params.command.compiles()) {
        match prepare().await {
            Ok(prepared) => {
                for text in prepared.notes {
                    let _ = events.send(DriverEvent::Line { text });
                }
                env = prepared.env;
            }
            Err(message) => {
                let _ = events.send(DriverEvent::Fatal { message });
                return Vec::new();
            }
        }
    }

    // Cargo is asked which of its members are the workspace's, so a
    // dependency's artifacts are not mistaken for the driver's.
    let members: std::collections::HashSet<String> = metadata(root)
        .map(|metadata| metadata.workspace_members.into_iter().collect())
        .unwrap_or_default();

    if params.command == Command::Fmt {
        let _ = events.send(DriverEvent::Building {
            unit: "workspace".to_owned(),
        });
        let (success, code) = format(root, params, &events).await;
        let _ = events.send(DriverEvent::Done { success, code });
        return Vec::new();
    }

    // Where rustc's relative paths start from, and what to make them
    // relative to instead. Cargo runs rustc from the cargo workspace's root
    // for every member, whichever directory cargo itself was started in.
    let paths = params.diagnostics.then(|| Relocate {
        cargo_root: root.to_owned(),
        workspace: root.parent().map(Utf8Path::to_owned).unwrap_or_default(),
    });

    let mut produced = Vec::new();
    let mut failed = None;
    for unit in units(root, params.command) {
        let _ = events.send(DriverEvent::Building {
            unit: unit.name.clone(),
        });

        match build_one(
            &unit.directory,
            params,
            &env,
            &events,
            &members,
            paths.as_ref(),
        )
        .await
        {
            Some(Ok(mut artifacts)) => produced.append(&mut artifacts),
            // Reported already. For a person there is no sense building the
            // kernel module against a userland that did not compile, and a
            // lint that has already failed has already said what they need.
            // An editor is different: it wants every unit's diagnostics, and
            // the module's errors are no less real for userland's.
            Some(Err(code)) if params.diagnostics => {
                failed = failed.or(Some(code));
            }
            Some(Err(code)) => {
                let _ = events.send(DriverEvent::Done {
                    success: false,
                    code,
                });
                return Vec::new();
            }
            None => return Vec::new(),
        }
    }

    if let Some(code) = failed {
        let _ = events.send(DriverEvent::Done {
            success: false,
            code,
        });
        return Vec::new();
    }

    if params.command.stores() && !produced.is_empty() {
        // Stored before the build is called finished. Otherwise a developer
        // whose command has just returned successfully would go looking for
        // the artifacts and find nothing, which is a race they have no way to
        // know about.
        let stored = upload(produced.clone()).await;
        let _ = events.send(DriverEvent::Produced {
            artifacts: produced.iter().map(ToString::to_string).collect(),
            stored: stored.count,
            problems: stored.problems,
        });
    }

    let _ = events.send(DriverEvent::Done {
        success: true,
        code: Some(0),
    });

    produced
}

/// Format the driver and send back everything that changed.
///
/// Two passes: `--check` with rustfmt's `-l` first, which names the files that
/// would change without changing them, so what they held before can be
/// recorded; then the formatting itself. Returns whether it worked and cargo's
/// exit status, for [`DriverEvent::Done`].
async fn format(
    root: &Utf8Path,
    params: &BuildParams,
    events: &tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) -> (bool, Option<i32>) {
    // rustfmt's own flags go after cargo's `--`. A developer who already
    // wrote one gets theirs extended rather than a second, which cargo would
    // hand rustfmt as a file name.
    let rustfmt_list = |mut args: Vec<String>| {
        if !args.iter().any(|arg| arg == "--") {
            args.push("--".to_owned());
        }
        args.push("-l".to_owned());
        args
    };

    let mut check = vec!["fmt".to_owned(), "--check".to_owned()];
    check.extend(params.args.iter().cloned());
    let listed = match run_quietly(root, &rustfmt_list(check), events).await {
        Ok(listed) => listed,
        Err(code) => return (false, code),
    };

    // rustfmt names files as cargo handed them over, which is absolute; the
    // client needs them relative to the driver, since that is the one
    // directory both ends agree on.
    let base = std::fs::canonicalize(root)
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .unwrap_or_else(|| root.to_owned());
    let mut before = Vec::new();
    for line in &listed.lines {
        let path = Utf8PathBuf::from(line.trim());
        let absolute = std::fs::canonicalize(root.join(&path))
            .ok()
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
            .unwrap_or(path);
        let Ok(relative) = absolute.strip_prefix(&base) else {
            // A member outside the driver's directory. Formatted on the
            // instance like the rest, but there is nowhere on the client this
            // could be said to belong, so it is said rather than sent.
            let _ = events.send(DriverEvent::Line {
                text: format!(
                    "warning: {absolute} needs formatting but is outside \
                     `{DIRECTORY}`; format it locally"
                ),
            });
            continue;
        };
        match std::fs::read(&absolute) {
            Ok(bytes) => before.push((
                absolute.clone(),
                relative.to_owned(),
                vw_sync::digest_bytes(&bytes).0,
            )),
            Err(e) => {
                let _ = events.send(DriverEvent::Fatal {
                    message: format!("reading {absolute}: {e}"),
                });
                return (false, None);
            }
        }
    }

    if !listed.success && before.is_empty() && listed.lines.is_empty() {
        // `--check` failed without naming anything, which means it failed for
        // a reason of its own — a file that does not parse, a module that
        // does not exist. rustfmt has already said which.
        return (false, listed.code);
    }
    if listed.lines.is_empty() {
        return (true, Some(0));
    }

    let mut write = vec!["fmt".to_owned()];
    write.extend(params.args.iter().cloned());
    match run_quietly(root, &write, events).await {
        Ok(ran) if ran.success => {}
        Ok(ran) => return (false, ran.code),
        Err(code) => return (false, code),
    }

    for (absolute, relative, digest) in before {
        match std::fs::read_to_string(&absolute) {
            Ok(contents) => {
                let _ = events.send(DriverEvent::Formatted {
                    path: relative.as_str().replace('\\', "/"),
                    before: digest,
                    contents,
                });
            }
            Err(e) => {
                let _ = events.send(DriverEvent::Fatal {
                    message: format!("reading {absolute}: {e}"),
                });
                return (false, None);
            }
        }
    }

    (true, Some(0))
}

/// What a cargo run that kept its stdout came to.
struct Quiet {
    success: bool,
    code: Option<i32>,
    lines: Vec<String>,
}

/// Run cargo, keeping stdout and forwarding stderr.
///
/// For `cargo fmt -l`, whose stdout is a list of files for this module to act
/// on rather than anything a developer should read. `Err` when cargo could not
/// be run at all, which has already been reported.
async fn run_quietly(
    root: &Utf8Path,
    args: &[String],
    events: &tokio::sync::mpsc::UnboundedSender<DriverEvent>,
) -> Result<Quiet, Option<i32>> {
    let child = tokio::process::Command::new("cargo")
        .args(args)
        .current_dir(root.as_std_path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("cannot run cargo on this instance: {e}"),
            });
            return Err(None);
        }
    };

    let plain = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(forward(stderr, events.clone())));
    let mut lines = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
    }
    let status = child.wait().await;
    if let Some(plain) = plain {
        let _ = plain.await;
    }

    match status {
        Ok(status) => Ok(Quiet {
            success: status.success(),
            code: status.code(),
            lines,
        }),
        Err(e) => {
            let _ = events.send(DriverEvent::Fatal {
                message: format!("waiting for cargo: {e}"),
            });
            Err(None)
        }
    }
}

/// Cargo's command line for `params`, after the program name.
fn arguments(params: &BuildParams) -> Vec<String> {
    let mut args: Vec<String> = params
        .command
        .subcommand()
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();

    if params.command.compiles() {
        if params.release {
            args.push("--release".to_owned());
        }
        if params.diagnostics {
            // For an editor, which renders diagnostics itself from the
            // structure and shows `rendered` as text: colour codes in it
            // would be noise.
            args.extend(["--message-format", "json"].map(str::to_owned));
        } else {
            // Forced on: cargo turns colour off when its output is not a
            // terminal, and here it never is — but there is a terminal at the
            // far end of this, and it is the one that matters.
            args.extend(["--color", "always"].map(str::to_owned));
            // Structured, so the exact set of files this produced comes from
            // cargo rather than from guessing at target directory layout.
            // Diagnostics still arrive pre-rendered, colour and all, so
            // nothing about what a developer sees changes. `cargo fmt` has
            // neither flag: it is rustfmt with a cargo-shaped front, and its
            // diff is the output.
            args.extend(
                ["--message-format", "json-diagnostic-rendered-ansi"]
                    .map(str::to_owned),
            );
        }
    }

    args.extend(params.args.iter().cloned());
    args
}

/// Spawn cargo in one directory and forward everything it says.
///
/// Returns what it produced, or cargo's exit status if it failed — which the
/// caller decides how to report — or `None` if it could not be run at all, in
/// which case that has already been reported.
async fn build_one(
    root: &Utf8Path,
    params: &BuildParams,
    env: &[(String, String)],
    events: &tokio::sync::mpsc::UnboundedSender<DriverEvent>,
    members: &std::collections::HashSet<String>,
    paths: Option<&Relocate>,
) -> Option<Result<Vec<Utf8PathBuf>, Option<i32>>> {
    let mut command = tokio::process::Command::new("cargo");
    command.args(arguments(params));
    command.envs(env.iter().map(|(k, v)| (k, v)));

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
        tokio::spawn(read_messages(
            stdout,
            events.clone(),
            members.clone(),
            paths.cloned(),
        ))
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
        Ok(status) if status.success() => Some(Ok(artifacts)),
        Ok(status) => Some(Err(status.code())),
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
    paths: Option<Relocate>,
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
            CargoMessage::CompilerMessage { .. } if paths.is_some() => {
                let Ok(mut message) =
                    serde_json::from_str::<serde_json::Value>(&line)
                else {
                    continue;
                };
                if let Some(paths) = &paths {
                    paths.relocate(&mut message);
                }
                let _ = events.send(DriverEvent::Diagnostic { message });
            }
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
                target,
                filenames,
            } if members.contains(&package_id) && !target.is_build_script() => {
                produced.extend(
                    filenames.into_iter().filter(|path| is_deliverable(path)),
                );
            }
            _ => {}
        }
    }

    produced
}

/// What a [`DriverEvent::Diagnostic`] says in place of the instance's
/// workspace root, for the client to replace with its own.
///
/// A marker rather than a relative path because the root appears in more
/// than file names: cargo's package ids are `path+file://` URLs and its
/// manifest paths are absolute, and an editor matches diagnostics to packages
/// by them. A marker can be swapped wherever it is; a relative path cannot be
/// told apart from a relative path that was always there.
pub const WORKSPACE_MARKER: &str = "{vw-workspace}";

/// How a diagnostic's paths are made to mean something off the instance.
#[derive(Clone, Debug)]
struct Relocate {
    /// What rustc's relative paths are relative to.
    cargo_root: Utf8PathBuf,
    /// The prefix [`WORKSPACE_MARKER`] stands in for.
    workspace: Utf8PathBuf,
}

impl Relocate {
    /// Rewrite every path in a cargo message, wherever it is nested — spans,
    /// the spans of child notes, the expansion chain of a span that came out
    /// of a macro, and the package it is about.
    fn relocate(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    if let serde_json::Value::String(text) = value {
                        if key == "file_name" {
                            *text = self.absolute(text);
                        }
                        *text = self.mark(text);
                    } else {
                        self.relocate(value);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    self.relocate(item);
                }
            }
            serde_json::Value::String(text) => *text = self.mark(text),
            _ => {}
        }
    }

    /// A span's file, as an absolute path on this instance. rustc names a
    /// member's files relative to the cargo workspace root, and anything
    /// else absolutely.
    fn absolute(&self, name: &str) -> String {
        // `<macros>`-style names are not files at all.
        if name.starts_with('<') {
            return name.to_owned();
        }
        let path = Utf8Path::new(name);
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            self.cargo_root.join(path)
        };
        normalize(&absolute).into_string()
    }

    /// `text` with the workspace root replaced by [`WORKSPACE_MARKER`]
    /// wherever it starts a path.
    fn mark(&self, text: &str) -> String {
        let root = self.workspace.as_str();
        if root.is_empty() || !text.contains(root) {
            return text.to_owned();
        }
        if text == root {
            return WORKSPACE_MARKER.to_owned();
        }
        text.replace(&format!("{root}/"), &format!("{WORKSPACE_MARKER}/"))
    }
}

/// `a/b/../c` as `a/c`, without touching the filesystem — the path may name
/// something that exists only on the instance, which is the point.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for part in path.components() {
        match part {
            camino::Utf8Component::ParentDir => {
                out.pop();
            }
            camino::Utf8Component::CurDir => {}
            other => out.push(other.as_str()),
        }
    }
    out
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
            command: Command::Build,
            release: true,
            args: vec!["-p".to_owned(), "module".to_owned()],
            diagnostics: false,
        };

        // The shape the agent will hand cargo, checked here because getting it
        // wrong means building the wrong thing on a machine nobody is looking
        // at.
        assert_eq!(
            arguments(&params),
            [
                "build",
                "--release",
                "--color",
                "always",
                "--message-format",
                "json-diagnostic-rendered-ansi",
                "-p",
                "module",
            ],
        );
    }

    #[test]
    fn a_lint_is_clippy_with_what_the_developer_added() {
        let params = BuildParams {
            command: Command::Clippy,
            release: false,
            args: vec!["--all-targets".to_owned()],
            diagnostics: false,
        };

        assert_eq!(
            arguments(&params),
            [
                "clippy",
                "--color",
                "always",
                "--message-format",
                "json-diagnostic-rendered-ansi",
                "--all-targets",
            ],
        );
    }

    #[test]
    fn checking_formatting_passes_nothing_meant_for_a_compiler() {
        // `--release` means nothing to rustfmt, and `--message-format` means
        // something else entirely, so neither is passed.
        let params = BuildParams {
            command: Command::FmtCheck,
            release: true,
            args: Vec::new(),
            diagnostics: false,
        };

        assert_eq!(arguments(&params), ["fmt", "--check"]);
    }

    #[test]
    fn a_request_that_names_no_command_is_a_build() {
        // What an instance hears from a service that predates the choice.
        let params: BuildParams =
            serde_json::from_str(r#"{"release":false,"args":[]}"#)
                .expect("deserialize");
        assert_eq!(params.command, Command::Build);
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

        let names: Vec<String> = units(&root, Command::Build)
            .into_iter()
            .map(|unit| unit.name)
            .collect();

        assert_eq!(names, ["workspace", "kmod"]);
    }

    #[test]
    fn a_workspace_that_is_all_userland_is_one_build() {
        // The common case, and it should cost nothing: no member asks for a
        // different target, so one invocation covers it.
        let (_dir, root) = workspace(false, false);

        let names: Vec<String> = units(&root, Command::Build)
            .into_iter()
            .map(|unit| unit.name)
            .collect();

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
    fn only_compiling_splits_off_a_member_with_its_own_target() {
        // Formatting from the root already covers it, and testing cannot run
        // what a kernel module compiles to.
        let (_dir, root) = workspace(true, false);

        for command in [Command::Fmt, Command::FmtCheck, Command::Test] {
            let names: Vec<String> = units(&root, command)
                .into_iter()
                .map(|unit| unit.name)
                .collect();
            assert_eq!(names, ["workspace"], "{command}");
        }

        for command in [Command::Check, Command::Clippy] {
            let names: Vec<String> = units(&root, command)
                .into_iter()
                .map(|unit| unit.name)
                .collect();
            assert_eq!(names, ["workspace", "kmod"], "{command}");
        }
    }

    /// Run `command` over a workspace whose `userland` member is badly
    /// formatted, collecting everything the instance would have sent.
    async fn run_over_unformatted(command: Command) -> Vec<DriverEvent> {
        let (_dir, root) = workspace(false, false);
        std::fs::write(
            root.join("userland/src/lib.rs"),
            "mod m;\nfn  f( ){}\n",
        )
        .expect("write");
        std::fs::write(root.join("userland/src/m.rs"), "pub fn g() {}\n")
            .expect("write");
        // Already formatted, so only `userland` has anything to change.
        std::fs::write(root.join("kmod/src/lib.rs"), "\n").expect("write");

        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let params = BuildParams {
            command,
            ..BuildParams::default()
        };
        let upload: Uploader =
            Box::new(|_| Box::pin(async { Stored::default() }));
        build(&root, &params, None, events, upload).await;

        let mut all = Vec::new();
        while let Ok(event) = received.try_recv() {
            all.push(event);
        }
        all
    }

    #[tokio::test]
    async fn formatting_sends_back_exactly_what_changed() {
        let events = run_over_unformatted(Command::Fmt).await;

        let formatted: Vec<(&str, &str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                DriverEvent::Formatted {
                    path,
                    before,
                    contents,
                } => Some((path.as_str(), before.as_str(), contents.as_str())),
                _ => None,
            })
            .collect();

        // Only the file that needed it, named relative to the driver, with
        // the digest of what it held so the client can tell it is unchanged.
        assert_eq!(formatted.len(), 1, "{events:?}");
        let (path, before, contents) = formatted[0];
        assert_eq!(path, "userland/src/lib.rs");
        assert_eq!(before, vw_sync::digest_bytes(b"mod m;\nfn  f( ){}\n").0);
        assert_eq!(contents, "mod m;\nfn f() {}\n");
        assert!(
            matches!(
                events.last(),
                Some(DriverEvent::Done { success: true, .. })
            ),
            "{events:?}",
        );
    }

    #[tokio::test]
    async fn checking_formatting_sends_nothing_back_and_fails() {
        let events = run_over_unformatted(Command::FmtCheck).await;

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, DriverEvent::Formatted { .. })),
            "{events:?}",
        );
        assert!(
            matches!(
                events.last(),
                Some(DriverEvent::Done { success: false, .. })
            ),
            "{events:?}",
        );
    }

    #[tokio::test]
    async fn a_file_that_does_not_parse_fails_formatting_and_sends_nothing() {
        let (_dir, root) = workspace(false, false);
        std::fs::write(root.join("userland/src/lib.rs"), "fn f( {\n")
            .expect("write");

        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let params = BuildParams {
            command: Command::Fmt,
            ..BuildParams::default()
        };
        let upload: Uploader =
            Box::new(|_| Box::pin(async { Stored::default() }));
        build(&root, &params, None, events, upload).await;

        let mut all = Vec::new();
        while let Ok(event) = received.try_recv() {
            all.push(event);
        }
        assert!(
            !all.iter()
                .any(|event| matches!(event, DriverEvent::Formatted { .. })),
            "{all:?}",
        );
        assert!(
            matches!(
                all.last(),
                Some(DriverEvent::Done { success: false, .. })
            ),
            "{all:?}",
        );
    }

    #[test]
    fn an_editor_gets_plain_json_with_no_colour() {
        let params = BuildParams {
            command: Command::Check,
            release: false,
            args: Vec::new(),
            diagnostics: true,
        };
        assert_eq!(arguments(&params), ["check", "--message-format", "json"]);
    }

    #[test]
    fn diagnostic_paths_name_the_workspace_by_marker() {
        let paths = Relocate {
            cargo_root: Utf8PathBuf::from("/root/sync/ws/driver"),
            workspace: Utf8PathBuf::from("/root/sync/ws"),
        };
        let mut message = serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///root/sync/ws/driver/rh#0.1.0",
            "manifest_path": "/root/sync/ws/driver/rh/Cargo.toml",
            "message": {
                "spans": [{
                    // A member, as rustc names it: relative to cargo's root.
                    "file_name": "module/src/lib.rs",
                    "expansion": { "span": {
                        // A path dependency outside the driver.
                        "file_name": "/root/sync/ws/driver/../common/u/src/lib.rs"
                    }}
                }],
                "children": [{ "spans": [{
                    // Not the workspace's, so left as it is.
                    "file_name": "/root/.cargo/registry/src/x/lib.rs"
                }]}],
            },
        });

        paths.relocate(&mut message);

        let span = &message["message"]["spans"][0];
        assert_eq!(
            span["file_name"],
            "{vw-workspace}/driver/module/src/lib.rs"
        );
        assert_eq!(
            span["expansion"]["span"]["file_name"],
            "{vw-workspace}/common/u/src/lib.rs"
        );
        assert_eq!(
            message["message"]["children"][0]["spans"][0]["file_name"],
            "/root/.cargo/registry/src/x/lib.rs"
        );
        assert_eq!(
            message["package_id"],
            "path+file://{vw-workspace}/driver/rh#0.1.0"
        );
        assert_eq!(
            message["manifest_path"],
            "{vw-workspace}/driver/rh/Cargo.toml"
        );
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
