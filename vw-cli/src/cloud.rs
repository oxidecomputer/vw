// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw cloud` — manage remote build environments hosted by a vw service.
//!
//! An environment is a set of cloud instances (vivado, helios, artifact) that
//! a workspace builds on. These commands are thin wrappers over the vw service
//! user API, reached through the progenitor generated client in
//! `vw-api-client`.
//!
//! The caller is identified by a Github access token read from `~/.netrc` —
//! the same credential `vw update` uses to fetch private dependencies, so
//! there is nothing extra to configure. Having no token is not by itself an
//! error: a service run with `--no-auth` answers without one, so the request
//! goes out unauthenticated and the missing credential is only reported if
//! the service turns it away.

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Subcommand};
use colored::*;
use indicatif::ProgressBar;
use std::time::{Duration, Instant};
use vw_api_client::user::{types, Client};

/// The vw service commands talk to when the caller does not name one.
///
/// A deployment is reached by its own name — there is one vw service per name,
/// and which one this is is not the client's business beyond the URL. Other
/// deployments (`beta.vw-cloud.dev`, or somebody else's entirely) are reached
/// by pointing `--url` or `$VW_SVC_URL` at them; nothing here enumerates them.
///
/// No port, because the user API answers on 443.
const SERVICE_URL: &str = "https://vw-cloud.dev";

/// The port a vw service's administrative API answers on.
///
/// A second listener rather than a path under the user API, so that whoever
/// runs the service can decide separately who may reach it.
const ADMIN_PORT: u16 = 2053;

/// The service the commands without a `--url` of their own will talk to.
///
/// The same order `clap` applies to `vw cloud`'s `--url`, minus the flag there
/// is nowhere to pass.
fn service_url_from_env() -> String {
    std::env::var("VW_SVC_URL").unwrap_or_else(|_| String::from(SERVICE_URL))
}

/// The administrative API of the service at `url`.
///
/// Derived from the user API's URL rather than named separately, because the
/// two are one deployment and the cost of letting them come apart is paid by
/// the administrative commands — the ones that delete other people's
/// environments. A caller who points `--url` at the beta and gets an admin
/// session on production has been handed the worst version of this tool.
///
/// It is only the port that differs. A deployment that puts its administrative
/// API somewhere else entirely is still reachable, by saying so with
/// `--admin-url` or `$VW_SVC_ADMIN_URL`.
///
/// A URL that will not parse is handed back untouched, so that the error the
/// caller sees comes from the client trying to use it and names the URL, rather
/// than from here.
/// The trailing `/` is trimmed because the generated client builds every
/// request as `{base}/v1/...`, so a base that ends in one asks for `//v1/...`.
/// `Url` always serializes an empty path as `/`, and a caller who exported a
/// URL with a trailing slash meant the same thing, so both are handled here
/// rather than left to produce a 404 nobody would connect to this function.
fn admin_url_for(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.set_port(Some(ADMIN_PORT)).is_err() {
        return url.to_owned();
    }
    parsed.to_string().trim_end_matches('/').to_owned()
}

/// How often `--wait` asks what an environment's instances are doing.
///
/// Instances take minutes, so this is far more often than anything changes.
/// It is tuned for how quickly the answer arrives once it does, and the cost
/// is a handful of requests against a service that is doing nothing else for
/// this caller.
const WAIT_POLL: Duration = Duration::from_secs(2);

/// How long `--wait` waits before giving up.
///
/// Long enough that a rack under load is never mistaken for a broken one, and
/// short enough that a script does not hang for an afternoon. Reaching it is
/// not a statement that the environment failed — only that it did not finish
/// while somebody was watching, and the states it stopped at are reported.
const WAIT_LIMIT: Duration = Duration::from_secs(900);

/// Hosts to look for a Github access token under, in preference order.
const CREDENTIAL_HOSTS: [&str; 2] = ["github.com", "api.github.com"];

/// The instances an environment is made of, in display order, each with the
/// account to log in as.
///
/// The account is a property of the image the instance boots: the vivado and
/// artifact images are Ubuntu, the helios one is not.
const INSTANCES: [(&str, &str); 3] = [
    ("vivado", "ubuntu"),
    ("helios", "root"),
    ("artifact", "ubuntu"),
];

/// The status the service answers with when it wants a Github token and did
/// not get an acceptable one.
const UNAUTHORIZED: u16 = 401;

#[derive(Args)]
pub struct CloudArgs {
    #[arg(
        long,
        global = true,
        env = "VW_SVC_URL",
        help = "Base URL of the vw service. Defaults to \
                https://vw-cloud.dev. Point it at another deployment -- \
                https://beta.vw-cloud.dev, say -- to act on that one's \
                environments instead."
    )]
    url: Option<String>,

    #[arg(
        long,
        global = true,
        env = "VW_SVC_ADMIN_URL",
        help = "Base URL of the vw service's administrative API, which is a \
                separate listener on a separate port. Defaults to --url with \
                that port, so pointing --url at a deployment points these \
                commands at it too. Only used by `vw cloud admin`."
    )]
    admin_url: Option<String>,

    #[arg(
        long,
        global = true,
        help = "Accept the service's TLS certificate without verifying it. \
                For development services fronted by a self-signed \
                certificate; this gives up any guarantee about who is on the \
                other end, and your access token is sent to whatever answers."
    )]
    insecure: bool,

    #[command(subcommand)]
    command: CloudCommand,
}

#[derive(Subcommand)]
pub enum CloudCommand {
    #[command(about = "List your remote build environments")]
    List,
    #[command(about = "Create a remote build environment")]
    Create {
        #[arg(help = "Environment name")]
        name: String,
        #[arg(
            long,
            value_name = "IMAGE",
            help = "Image the vivado instance boots from. Defaults to the \
                    newest the service can see."
        )]
        vivado_image: Option<String>,
        #[arg(
            long,
            value_name = "IMAGE",
            help = "Image the helios instance boots from. Defaults to the \
                    newest the service can see."
        )]
        helios_image: Option<String>,
        #[arg(
            long,
            value_name = "IMAGE",
            help = "Image the artifact instance boots from. Defaults to the \
                    newest the service can see."
        )]
        artifact_image: Option<String>,
        #[arg(
            long,
            value_name = "DIR",
            help = "Directory to write the environment's ssh key into. \
                    Replaces any key already there. [default: ~/.ssh]"
        )]
        key_dir: Option<Utf8PathBuf>,
        #[arg(
            long,
            help = "Do not return until every instance is running. Their \
                    agents come up a few seconds after that."
        )]
        wait: bool,
    },
    #[command(about = "Show a remote build environment")]
    Get {
        #[arg(help = "Environment name. Defaults to $VW_ENV, then to what \
                    `vw cloud set environment` recorded for this checkout, \
                    then to your only environment.")]
        name: Option<String>,
    },
    #[command(about = "Delete a remote build environment")]
    Delete {
        #[arg(help = "Environment name")]
        name: String,
    },
    #[command(about = "Push the workspace to an environment's instances")]
    Sync {
        #[arg(help = "Environment name. Defaults to $VW_ENV, then to what \
                    `vw cloud set environment` recorded for this checkout, \
                    then to your only environment.")]
        name: Option<String>,

        #[arg(
            long,
            help = "Discard the instance's source tree first, so every file \
                    is sent again"
        )]
        force: bool,
        #[arg(long, help = "Keep syncing as files change, until interrupted")]
        watch: bool,
        #[arg(
            long,
            value_name = "MS",
            default_value_t = 150,
            help = "How long to wait for changes to settle before syncing"
        )]
        debounce: u64,
    },
    #[command(about = "List or download an environment's build artifacts")]
    Artifacts {
        #[arg(help = "Environment name. Defaults to $VW_ENV, then to what \
                    `vw cloud set environment` recorded for this checkout, \
                    then to your only environment.")]
        name: Option<String>,

        #[arg(
            long,
            value_name = "PATTERN",
            help = "Download the artifacts matching this instead of listing. \
                    A glob, where '*' crosses '/' — so '*.edif' finds every \
                    netlist and 'reports/*place*' finds the place reports. \
                    Quote it, or the shell may try to expand it first. \
                    Repeat, or use --all, for several."
        )]
        get: Vec<String>,
        #[arg(
            long,
            conflicts_with = "get",
            help = "Download every artifact instead of listing"
        )]
        all: bool,
        #[arg(
            long,
            conflicts_with_all = ["get", "all"],
            help = "Remove every stored artifact. The object store keeps no \
                    versions, so this cannot be undone."
        )]
        clear: bool,
        #[arg(
            long,
            conflicts_with = "clear",
            help = "Wait for the instance to finish uploading before \
                    listing. A build's artifacts reach the store a second or \
                    two after the build writes them, so a script that \
                    collects the moment a build ends can otherwise get a \
                    listing that is short and looks complete. Costs the \
                    seconds the upload takes, which is why it is not the \
                    default."
        )]
        flush: bool,
        #[arg(
            long,
            value_name = "DIR",
            help = "Directory to write downloads into [default: .]"
        )]
        out: Option<Utf8PathBuf>,
    },
    #[command(
        about = "List the workspaces synchronized to an environment",
        long_about = "List the workspaces synchronized to an environment.\n\n\
                      One environment holds a tree per workspace, keyed by \
                      what `vw cloud set workspace` recorded for a checkout \
                      or, failing that, by `[workspace] name` in its \
                      vw.toml. Shows when each was last pushed to, so you \
                      can tell which ones you are finished with."
    )]
    Workspaces {
        #[arg(help = "Environment name. Defaults to $VW_ENV, then to what \
                    `vw cloud set environment` recorded for this checkout, \
                    then to your only environment.")]
        name: Option<String>,
        #[arg(
            long,
            help = "Measure what each workspace occupies. Takes a moment: it \
                    means walking every file a build has written."
        )]
        sizes: bool,
    },
    #[command(
        about = "Remove a workspace from an environment",
        long_about = "Remove a workspace from an environment.\n\n\
                      Its source tree on both instances, everything a build \
                      wrote under it, and its artifacts. For a workspace that \
                      was renamed, or stood for a branch you are done with — \
                      nothing else ever removes one.\n\n\
                      The artifacts cannot be recovered. The source can: it \
                      came from a machine like this one and a sync puts it \
                      back."
    )]
    Forget {
        #[arg(help = "Workspace to remove")]
        workspace: String,
        #[arg(
            long,
            value_name = "NAME",
            help = "Environment to remove it from. Defaults the same way \
                    every other command's does."
        )]
        env: Option<String>,
    },
    #[command(
        about = "Record what this checkout does in the cloud",
        long_about = "Record what this checkout does in the cloud.\n\n\
                      Written to vw-cloud.toml beside vw.toml, and added to \
                      .gitignore — these are facts about your checkout, not \
                      about the project, and committing them would send \
                      everyone who checks out this branch to the same place."
    )]
    Set {
        #[command(subcommand)]
        command: SetCommand,
    },
    #[command(
        about = "Administer the service — every environment, whoever owns it"
    )]
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    #[command(
        about = "Download the ssh key that opens an environment's instances"
    )]
    Keys {
        #[arg(help = "Environment name. Defaults to $VW_ENV, then to what \
                    `vw cloud set environment` recorded for this checkout, \
                    then to your only environment.")]
        name: Option<String>,

        #[arg(
            long,
            value_name = "DIR",
            help = "Directory to write the key into. Replaces any key \
                    already there. [default: ~/.ssh]"
        )]
        dir: Option<Utf8PathBuf>,
    },
}

/// What this checkout can be told about the cloud.
///
/// A separate file from `vw.toml` because both of these are properties of a
/// checkout rather than of the project. The workspace especially: two
/// checkouts of one repository — `main` and a feature branch, say — want two
/// trees on one environment, and the only way for them to disagree about which
/// slot is theirs is for the answer to live somewhere git is not carrying
/// between them.
#[derive(Subcommand)]
pub enum SetCommand {
    #[command(
        about = "The slot on the environment this checkout pushes to",
        long_about = "The slot on the environment this checkout pushes to.\n\n\
                      Defaults to `[workspace] name` from vw.toml, which is \
                      right until a second checkout of the same project needs \
                      a tree of its own.\n\n\
                      This is only the key: which directory the tree goes in, \
                      and which bucket its artifacts land in. It does not \
                      change `[workspace] name`, which is what the design's \
                      own imports resolve through — so both checkouts still \
                      build exactly what they would have built."
    )]
    Workspace {
        #[arg(
            help = "Name for this checkout's slot. Lowercase letters, digits \
                    and '-'. Omit to go back to using vw.toml's name."
        )]
        name: Option<String>,
    },
    #[command(about = "The environment this checkout builds in")]
    Environment {
        #[arg(
            help = "Environment name. Omit to stop pinning one and go back to \
                    $VW_ENV or your only environment."
        )]
        name: Option<String>,
    },
}

/// What can be done through the administrative API.
///
/// Deliberately only the two things the user API cannot do: see across
/// everybody, and delete something that is not yours. Anything an
/// administrator can already do as themselves stays on `vw cloud`.
#[derive(Subcommand)]
pub enum AdminCommand {
    #[command(about = "List every environment on the service and who owns it")]
    List,
    #[command(about = "Delete an environment belonging to someone else")]
    Delete {
        #[arg(help = "User the environment belongs to")]
        user: String,
        #[arg(help = "Environment name")]
        name: String,
    },
    #[command(
        about = "Delete images nothing is using and nothing would use",
        long_about = "Delete the service's images that nothing is using and \
                      nothing would use.\n\nEach kind keeps its newest image \
                      — what an environment created now would boot — and \
                      every image an environment is booting, however old. \
                      Only the service's own images in its project are ever \
                      candidates, so the rack's base images are not at risk."
    )]
    ImageRecycle {
        #[arg(
            long,
            help = "Report what would be deleted without deleting anything"
        )]
        dry_run: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("no vw workspace here; run this from one, or from a directory inside it")]
    NoWorkspace,
    #[error(
        "no cloud environments exist for you. Create one with `vw cloud \
         create <name>`, or pass --local to build on this machine"
    )]
    NoEnvironments,
    #[error(
        "you have several cloud environments ({}); say which with --env, set \
         one for this checkout with `vw cloud set environment <name>`, or \
         pass --local to build on this machine",
        .0.join(", ")
    )]
    AmbiguousEnvironment(Vec<String>),
    #[error(
        "the workspace name '{0}' in {} cannot be used: {1}",
        crate::cloud_config::FILE
    )]
    BadWorkspaceName(String, String),
    #[error("reading this checkout's cloud settings")]
    CloudConfig(#[from] crate::cloud_config::ConfigError),
    #[error("reading the workspace configuration")]
    WorkspaceConfig(#[source] vw_lib::VwError),
    #[error("nothing matches '{0}'; run `vw cloud artifacts <env>` to see what there is")]
    NoSuchArtifact(String),
    #[error("'{0}' is not a valid pattern: {1}")]
    BadArtifactPattern(String, String),
    #[error("'{0}' is not a name an artifact may be written under")]
    UnsafeArtifactName(String),
    #[error("no driver here; {0} does not exist")]
    NoDriver(Utf8PathBuf),
    #[error("scanning {0}")]
    Scan(camino::Utf8PathBuf, #[source] vw_sync::ScanError),
    #[error("reading {0}")]
    ReadSource(String, #[source] std::io::Error),
    #[error("watching the workspace for changes")]
    Watch(#[source] notify::Error),
    #[error("reading github credentials: {0}")]
    Credentials(#[from] vw_lib::VwError),
    #[error(
        "this service requires authorization, but no github access token was \
         found in ~/.netrc. Add a machine entry for {} whose password is a \
         github personal access token with access to oxidecomputer/redhawk",
        CREDENTIAL_HOSTS[0]
    )]
    NoCredentials,
    #[error("building the api client: {0}")]
    Client(#[from] vw_api_client::Error),
    #[error("the service returned {status}: {message}")]
    Service { status: u16, message: String },
    #[error("talking to the service: {0}")]
    Transport(String),
    #[error("creating {0}: {1}")]
    KeyDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}: {1}")]
    KeyWrite(Utf8PathBuf, #[source] std::io::Error),
    #[error(
        "the {kind} instance of '{environment}' is {state}; it is not coming up"
    )]
    InstanceUnusable {
        environment: String,
        kind: String,
        state: String,
    },
    #[error(
        "'{environment}' was still not fully running after {seconds}s ({states}). \
         It was created and may yet come up; check with `vw cloud get {environment}`"
    )]
    WaitTimedOut {
        environment: String,
        seconds: u64,
        states: String,
    },
    #[error("cannot determine the home directory to put the key in")]
    NoHomeDirectory,
    #[error("home directory {0:?} is not valid utf-8")]
    HomeNotUtf8(std::path::PathBuf),
}

/// A connection to the service, and whether we had a credential to offer it.
pub struct Session {
    pub client: Client,
    /// Whether a Github token was found and sent. A `401` means very different
    /// things depending on this: no token means the caller needs to set one
    /// up, a token means Github turned it down.
    authenticated: bool,
}

pub async fn run(args: CloudArgs) -> Result<(), CloudError> {
    // The administrative commands speak to a different listener, and none of
    // them need the user API, so that session is the only one built.
    if let CloudCommand::Admin { command } = args.command {
        // Follows --url, so that pointing at a deployment points every command
        // at it. These are the commands that delete other people's
        // environments; an admin session left on the deployment the caller
        // stopped talking to is the one mistake here worth engineering against.
        let admin_url = args.admin_url.clone().unwrap_or_else(|| {
            admin_url_for(args.url.as_deref().unwrap_or(SERVICE_URL))
        });
        let session = AdminSession::new(&admin_url, args.insecure)?;
        return match command {
            AdminCommand::List => admin_list(&session).await,
            AdminCommand::Delete { user, name } => {
                admin_delete(&session, &user, &name).await
            }
            AdminCommand::ImageRecycle { dry_run } => {
                admin_image_recycle(&session, dry_run).await
            }
        };
    }

    let url = args
        .url
        .clone()
        .unwrap_or_else(|| String::from(SERVICE_URL));
    let session = Session::new(&url, args.insecure)?;

    match args.command {
        CloudCommand::List => list(&session).await,
        CloudCommand::Create {
            name,
            vivado_image,
            helios_image,
            artifact_image,
            key_dir,
            wait,
        } => {
            create(
                &session,
                &name,
                types::EnvironmentCreate {
                    vivado_image,
                    helios_image,
                    artifact_image,
                },
                key_dir.as_deref(),
                wait,
            )
            .await
        }
        CloudCommand::Get { name } => {
            let environment =
                environment_only(&session, name.as_deref()).await?;
            get(&session, &environment).await
        }
        CloudCommand::Delete { name } => delete(&session, &name).await,
        CloudCommand::Artifacts {
            name,
            get,
            all,
            clear,
            flush,
            out,
        } => {
            let target = resolve_target(&session, name.as_deref()).await?;
            artifacts(
                &session,
                &target,
                &get,
                all,
                clear,
                flush,
                out.as_deref(),
            )
            .await
        }
        CloudCommand::Keys { name, dir } => {
            let environment =
                environment_only(&session, name.as_deref()).await?;
            fetch_keys(&session, &environment, dir.as_deref()).await
        }
        CloudCommand::Sync {
            name,
            force,
            watch,
            debounce,
        } => {
            let target = resolve_target(&session, name.as_deref()).await?;
            crate::cloud_sync::run(
                // `vw cloud sync` is the command that means "everything", so
                // it is the one place with no filter.
                &session,
                &target,
                force,
                watch,
                std::time::Duration::from_millis(debounce),
                None,
            )
            .await
        }
        CloudCommand::Workspaces { name, sizes } => {
            let environment =
                environment_only(&session, name.as_deref()).await?;
            workspaces(&session, &environment, sizes).await
        }
        CloudCommand::Forget { workspace, env } => {
            let environment =
                environment_only(&session, env.as_deref()).await?;
            forget(&session, &environment, &workspace).await
        }
        CloudCommand::Set { command } => set(&session, command).await,
        // Handled before the user session is built.
        CloudCommand::Admin { .. } => unreachable!(),
    }
}

/// Which environment a command acts on, for the ones that do not act on a
/// tree.
///
/// `vw cloud get`, `keys`, `workspaces` and `forget` are about the environment
/// itself, and three of the four can reasonably be run from outside a
/// workspace altogether. So this resolves the environment the same way
/// [`resolve_target`] does and simply never asks about a slot — a missing
/// `vw.toml` is not an obstacle to asking what an environment is doing.
async fn environment_only(
    session: &Session,
    named: Option<&str>,
) -> Result<String, CloudError> {
    if let Some(name) = named {
        return Ok(name.to_owned());
    }
    if let Ok(name) = std::env::var("VW_ENV") {
        if !name.is_empty() {
            return Ok(name);
        }
    }
    if let Ok(dir) = crate::cloud_sync::workspace_root() {
        if let Some(name) = crate::cloud_config::load(&dir)?.environment {
            return Ok(name);
        }
    }

    let environments =
        vw_api_client::retrying(|| session.client.get_environments())
            .await
            .map_err(|e| session.error(e))?
            .into_inner()
            .items;

    match environments.len() {
        0 => Err(CloudError::NoEnvironments),
        1 => Ok(environments[0].name.clone()),
        _ => Err(CloudError::AmbiguousEnvironment(
            environments.iter().map(|e| e.name.clone()).collect(),
        )),
    }
}

/// List the workspaces an environment is holding.
async fn workspaces(
    session: &Session,
    environment: &str,
    sizes: bool,
) -> Result<(), CloudError> {
    let held = vw_api_client::retrying(|| {
        session.client.get_workspaces(environment, Some(sizes))
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    if held.is_empty() {
        println!(
            "{} nothing has been synchronized to {environment} yet",
            "\u{2713}".bright_green(),
        );
        return Ok(());
    }

    // Which slot this checkout occupies, so the listing can point at it. A
    // command run from outside a workspace simply has nothing to point at.
    let mine = crate::cloud_sync::workspace_root()
        .ok()
        .and_then(|dir| workspace_slot(&dir).ok())
        .map(|(name, _)| name);

    for workspace in &held {
        let here = if Some(&workspace.name) == mine.as_ref() {
            " (this checkout)".bright_black().to_string()
        } else {
            String::new()
        };
        let size = if sizes {
            format!("  {}", human_bytes(workspace.bytes).bright_black())
        } else {
            String::new()
        };
        println!(
            "{}  {}{}{}",
            workspace.name.cyan(),
            since(workspace.last_synced).bright_black(),
            size,
            here,
        );
    }

    Ok(())
}

/// How long ago a workspace was last pushed to, in words.
///
/// Rounded hard and deliberately: this answers "am I still using this", and
/// nobody deciding that needs minutes.
fn since(last_synced: Option<u64>) -> String {
    let Some(at) = last_synced else {
        return String::from("never synced");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(at);

    match ago {
        0..=3599 => String::from("synced within the hour"),
        3600..=86399 => format!("synced {}h ago", ago / 3600),
        _ => format!("synced {}d ago", ago / 86400),
    }
}

/// Remove a workspace from an environment.
async fn forget(
    session: &Session,
    environment: &str,
    workspace: &str,
) -> Result<(), CloudError> {
    let forgotten = vw_api_client::retrying(|| {
        session.client.forget_workspace(environment, workspace)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    if forgotten.trees.is_empty() && forgotten.artifacts == 0 {
        println!(
            "{} '{workspace}' was not on {environment}",
            "\u{2713}".bright_green(),
        );
        return Ok(());
    }

    println!(
        "{} forgot {} ({} {}, {} artifacts, {})",
        "\u{2713}".bright_green(),
        workspace.cyan(),
        forgotten.trees.len(),
        if forgotten.trees.len() == 1 {
            "tree"
        } else {
            "trees"
        },
        forgotten.artifacts,
        human_bytes(forgotten.bytes),
    );

    Ok(())
}

/// Record what this checkout does in the cloud.
async fn set(session: &Session, command: SetCommand) -> Result<(), CloudError> {
    let dir = crate::cloud_sync::workspace_root()?;
    let mut settings = crate::cloud_config::load(&dir)?;

    match command {
        SetCommand::Workspace { name } => {
            if let Some(name) = &name {
                vw_lib::validate_workspace_name(name).map_err(|e| {
                    CloudError::BadWorkspaceName(name.clone(), e)
                })?;
            }
            settings.workspace = name;
        }
        SetCommand::Environment { name } => {
            // Checked against the service, and only warned about. Pinning an
            // environment before creating it is a reasonable thing to do, and
            // so is doing this on a train — but a typo found now beats one
            // found as a confusing failure on the next sync.
            if let Some(name) = &name {
                match vw_api_client::retrying(|| {
                    session.client.get_environment(name)
                })
                .await
                {
                    Ok(_) => {}
                    Err(e) => println!(
                        "{} could not confirm '{name}' exists: {}",
                        "warning:".yellow(),
                        session.error(e),
                    ),
                }
            }
            settings.environment = name;
        }
    }

    crate::cloud_config::save(&dir, &settings)?;

    let (workspace, workspace_from) = workspace_slot(&dir)?;
    println!(
        "{} {} {}",
        "\u{2713}".bright_green(),
        crate::cloud_config::path(&dir).as_str().bright_black(),
        format!(
            "environment {} · workspace {} ({})",
            settings.environment.as_deref().unwrap_or("unset"),
            workspace,
            workspace_from.describe(),
        )
        .bright_black(),
    );

    Ok(())
}

/// A connection to the service's administrative API.
///
/// Separate from [`Session`] rather than a mode of it, because it is a
/// different listener with a different generated client and a different set of
/// endpoints. The credential is the same one: an administrator is a Github
/// user the service was started with the name of, not a second identity.
pub struct AdminSession {
    client: vw_api_client::admin::Client,
    authenticated: bool,
}

impl AdminSession {
    fn new(url: &str, insecure: bool) -> Result<AdminSession, CloudError> {
        let token = access_token()?;
        Ok(AdminSession {
            client: vw_api_client::admin_client(
                &vw_api_client::ClientConfig {
                    base_url: url,
                    token: token.as_deref(),
                    insecure,
                },
            )?,
            authenticated: token.is_some(),
        })
    }

    /// Render a client error in terms of what the service said.
    ///
    /// The same shape as [`Session::error`], over the admin client's own error
    /// type. A 403 here is the interesting one: it means the caller is who
    /// they say they are and is not an administrator, and the service's
    /// message says how to become one.
    fn error(
        &self,
        error: vw_api_client::admin::Error<vw_api_client::admin::types::Error>,
    ) -> CloudError {
        match error {
            vw_api_client::admin::Error::ErrorResponse(response) => {
                let status = response.status().as_u16();
                if status == UNAUTHORIZED && !self.authenticated {
                    return CloudError::NoCredentials;
                }
                CloudError::Service {
                    status,
                    message: response.into_inner().message,
                }
            }
            other => CloudError::Transport(vw_remote::causes(&other)),
        }
    }
}

/// Every environment on the service, whoever owns it.
async fn admin_list(session: &AdminSession) -> Result<(), CloudError> {
    let page = vw_api_client::retrying(|| session.client.get_environments())
        .await
        .map_err(|e| session.error(e))?;
    let mut environments = page.into_inner().items;

    if environments.is_empty() {
        println!("No cloud environments exist on this service.");
        return Ok(());
    }

    // Grouped by owner, because the question this command answers is usually
    // "who is holding the rack" rather than "what is running".
    environments.sort_by(|a, b| {
        (&a.user, &a.environment.name).cmp(&(&b.user, &b.environment.name))
    });

    let mut current = None;
    for entry in &environments {
        if current != Some(&entry.user) {
            println!("{}", entry.user.bright_white().bold());
            current = Some(&entry.user);
        }
        println!(
            "  {} - {}",
            entry.environment.name.cyan(),
            admin_instance_summary(&entry.environment),
        );
    }

    let owners = environments
        .iter()
        .map(|e| &e.user)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    println!(
        "\n{} environment{} across {} user{}",
        environments.len(),
        if environments.len() == 1 { "" } else { "s" },
        owners,
        if owners == 1 { "" } else { "s" },
    );

    Ok(())
}

/// Delete somebody else's environment.
async fn admin_delete(
    session: &AdminSession,
    user: &str,
    name: &str,
) -> Result<(), CloudError> {
    // Positional, and in the order the path template names them
    // (`/environment/{user}/{name}`) rather than the order the spec lists the
    // parameters in. Getting this backwards produces a 404 naming an
    // environment that exists, which is a confusing thing to debug.
    vw_api_client::retrying(|| session.client.delete_environment(user, name))
        .await
        .map_err(|e| session.error(e))?;

    println!(
        "{} Deleted {}'s cloud environment: {}",
        "✓".bright_green(),
        user.bright_white(),
        name.cyan(),
    );
    Ok(())
}

/// Reclaim the images nothing is using and nothing would use.
///
/// What was kept is printed as well as what went, and first: the question an
/// administrator has after running this is usually about an image that is
/// still there. On a dry run it is the whole answer.
async fn admin_image_recycle(
    session: &AdminSession,
    dry_run: bool,
) -> Result<(), CloudError> {
    let report = vw_api_client::retrying(|| {
        session.client.recycle_images(Some(dry_run))
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    for image in &report.kept {
        let why = match (image.latest, image.used_by.as_slice()) {
            (true, []) => "newest of its kind".to_owned(),
            (true, used) => {
                format!("newest of its kind; in use by {}", used.join(", "))
            }
            (false, used) => format!("in use by {}", used.join(", ")),
        };
        println!("{} {} - {}", "keep".green(), image.name.cyan(), why);
    }

    for image in &report.deleted {
        let verb = if dry_run { "would delete" } else { "deleted" };
        println!("{} {}", verb.yellow(), image.name.cyan());
    }

    if report.deleted.is_empty() {
        println!(
            "\nNothing to recycle; {} image{} kept.",
            report.kept.len(),
            if report.kept.len() == 1 { "" } else { "s" },
        );
    } else if dry_run {
        println!(
            "\n{} image{} would be deleted. Run without {} to do it.",
            report.deleted.len(),
            if report.deleted.len() == 1 { "" } else { "s" },
            "--dry-run".bright_white(),
        );
    } else {
        println!(
            "\n{} Deleted {} image{}.",
            "✓".bright_green(),
            report.deleted.len(),
            if report.deleted.len() == 1 { "" } else { "s" },
        );
    }

    Ok(())
}

/// A one line rendering of an environment's instances, over the admin client's
/// types.
///
/// The same shape as [`instance_summary`], which cannot be shared: the two
/// APIs are generated separately and their `Environment` types are distinct
/// Rust types that happen to look alike.
fn admin_instance_summary(
    environment: &vw_api_client::admin::types::Environment,
) -> String {
    let instances = [
        &environment.vivado_instance,
        &environment.helios_instance,
        &environment.artifact_instance,
    ];

    INSTANCES
        .iter()
        .zip(instances)
        .map(|((label, _), instance)| match instance {
            Some(instance) => format!(
                "{label}: {}",
                admin_colored_state(&instance.state.to_string())
            ),
            None => format!("{label}: {}", "none".bright_black()),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// [`colored_state`] over the admin client's state type, matched by name so
/// the two listings read the same.
fn admin_colored_state(state: &str) -> ColoredString {
    match state {
        "running" => state.green(),
        "creating" | "starting" | "stopping" | "rebooting" | "migrating"
        | "repairing" => state.magenta(),
        "stopped" => state.bright_black(),
        "failed" | "destroyed" => state.red(),
        other => other.normal(),
    }
}

impl Session {
    fn new(url: &str, insecure: bool) -> Result<Session, CloudError> {
        // A missing token is not fatal here. Services run with `--no-auth`
        // answer without one, so send what we have and let the service decide.
        let token = access_token()?;
        Ok(Session {
            client: vw_api_client::user_client(&vw_api_client::ClientConfig {
                base_url: url,
                token: token.as_deref(),
                insecure,
            })?,
            authenticated: token.is_some(),
        })
    }

    /// Render a client error in terms of what the service said.
    ///
    /// The service's own message is the part a person can act on, so pull it
    /// out of the response body rather than reporting the client's error enum.
    pub fn error(
        &self,
        error: vw_api_client::user::Error<types::Error>,
    ) -> CloudError {
        match error {
            vw_api_client::user::Error::ErrorResponse(response) => {
                let status = response.status().as_u16();
                if status == UNAUTHORIZED && !self.authenticated {
                    // We never offered a credential, so the service's "no
                    // token is present" is really a message about this
                    // machine's setup.
                    return CloudError::NoCredentials;
                }
                CloudError::Service {
                    status,
                    message: response.into_inner().message,
                }
            }
            other => CloudError::Transport(vw_remote::causes(&other)),
        }
    }
}

async fn list(session: &Session) -> Result<(), CloudError> {
    // The endpoint takes no pagination parameters, so this one page is every
    // environment the caller owns.
    let page = vw_api_client::retrying(|| session.client.get_environments())
        .await
        .map_err(|e| session.error(e))?;
    let environments = page.into_inner().items;

    if environments.is_empty() {
        println!(
            "No cloud environments. Create one with {}.",
            "vw cloud create <name>".cyan()
        );
        return Ok(());
    }

    println!("Environments:");
    for environment in &environments {
        println!(
            "  {} - {}",
            environment.name.cyan(),
            instance_summary(environment)
        );
    }
    Ok(())
}

async fn create(
    session: &Session,
    name: &str,
    images: types::EnvironmentCreate,
    key_dir: Option<&Utf8Path>,
    wait: bool,
) -> Result<(), CloudError> {
    let keys = vw_api_client::retrying(|| {
        session.client.create_environment(name, &images)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    println!(
        "{} Created cloud environment: {}",
        "✓".bright_green(),
        name.cyan()
    );

    // Almost certainly what this checkout wants, and it saves the follow-up
    // command. Only when there is a workspace here to record it in, and only
    // when nothing has been recorded already — somebody creating a second
    // environment from a checkout already pinned to one did not mean to
    // repoint it.
    if let Ok(dir) = crate::cloud_sync::workspace_root() {
        let mut settings = crate::cloud_config::load(&dir)?;
        if settings.environment.is_none() {
            settings.environment = Some(name.to_owned());
            crate::cloud_config::save(&dir, &settings)?;
            println!(
                "  {}",
                format!(
                    "recorded in {} — this checkout builds here now",
                    crate::cloud_config::FILE
                )
                .bright_black(),
            );
        }
    }

    // The environment exists either way, so a key that cannot be saved is a
    // warning and a recovery instruction rather than a failure. Reporting an
    // error here would suggest the create had not happened.
    match save_keys(name, &keys, key_dir) {
        Ok((private, public)) => report_keys(&private, &public),
        Err(e) => {
            eprintln!("{} {e}", "warning:".yellow());
            eprintln!(
                "  the environment was created; fetch its key with {}",
                format!("vw cloud keys {name}").cyan(),
            );
        }
    }

    if wait {
        wait_for_instances(session, name).await?;
    }

    Ok(())
}

/// Wait until every one of `name`'s instances is running.
///
/// Creating an environment records the intent and returns; the instances are
/// the reconciler's business and appear a minute or two later. That is the
/// right shape for a service but the wrong one for a script, which has nothing
/// to do with an environment whose machines do not exist yet.
///
/// What is waited for is the instance state Oxide reports, which is a weaker
/// promise than the environment being usable: an agent takes a few more
/// seconds to start after the machine it runs on does. It is still the useful
/// boundary, because everything before it is measured in minutes.
async fn wait_for_instances(
    session: &Session,
    name: &str,
) -> Result<(), CloudError> {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(120));
    spinner.set_message(format!("waiting for {}", name.cyan()));

    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let environment =
            vw_api_client::retrying(|| session.client.get_environment(name))
                .await
                .map_err(|e| session.error(e))?
                .into_inner();

        spinner.set_message(instance_summary(&environment));

        // Reported in the order they are displayed, so the kind named in an
        // error is the one whose state the caller just watched go red.
        let states: Vec<(&str, Option<&types::InstanceState>)> = INSTANCES
            .iter()
            .zip(instances(&environment))
            .map(|((kind, _), instance)| {
                (*kind, instance.as_ref().map(|i| &i.state))
            })
            .collect();

        // A machine that has failed or gone away is not on its way to running,
        // and waiting out the limit would only delay saying so.
        for (kind, state) in &states {
            if let Some(
                state @ (types::InstanceState::Failed
                | types::InstanceState::Destroyed),
            ) = state
            {
                spinner.finish_and_clear();
                return Err(CloudError::InstanceUnusable {
                    environment: name.to_owned(),
                    kind: (*kind).to_owned(),
                    state: state.to_string(),
                });
            }
        }

        if states
            .iter()
            .all(|(_, state)| *state == Some(&types::InstanceState::Running))
        {
            spinner.finish_and_clear();
            println!(
                "{} All instances running: {}",
                "✓".bright_green(),
                name.cyan()
            );
            return Ok(());
        }

        if Instant::now() >= deadline {
            spinner.finish_and_clear();
            return Err(CloudError::WaitTimedOut {
                environment: name.to_owned(),
                seconds: WAIT_LIMIT.as_secs(),
                states: instance_summary(&environment),
            });
        }

        tokio::time::sleep(WAIT_POLL).await;
    }
}

async fn get(session: &Session, name: &str) -> Result<(), CloudError> {
    let environment =
        vw_api_client::retrying(|| session.client.get_environment(name))
            .await
            .map_err(|e| session.error(e))?;
    let environment = environment.into_inner();

    println!("{}", environment.name.cyan());
    if let Some(images) = &environment.images {
        println!("  images");
        for ((label, _), image) in INSTANCES.iter().zip([
            &images.vivado,
            &images.helios,
            &images.artifact,
        ]) {
            println!("    {label:<8} {}", image.name.bright_black());
        }
    }

    println!("  instances");
    for ((label, user), instance) in
        INSTANCES.iter().zip(instances(&environment))
    {
        match instance {
            // An instance the service has asked for but not yet heard back
            // about has a state and no address, so there is nothing to show
            // but the state.
            Some(instance) => println!(
                "    {label:<8} {:<20} {}",
                colored_state(&instance.state),
                match instance.external_ip {
                    Some(ip) => format!("{user}@{ip}"),
                    None => String::new(),
                },
            ),
            None => {
                println!("    {label:<8} {}", "not provisioned".bright_black())
            }
        }
    }

    print_login_hints(&environment);
    Ok(())
}

/// Print a ready-to-run ssh line for every instance that can be reached.
///
/// The key path is the one `vw cloud create` and `vw cloud keys` write by
/// default; a caller who redirected it elsewhere has to substitute their own.
fn print_login_hints(environment: &types::Environment) {
    let reachable: Vec<String> = INSTANCES
        .iter()
        .zip(instances(environment))
        .filter_map(|((label, user), instance)| {
            let ip = instance.as_ref()?.external_ip?;
            let key = default_key_dir()
                .map(|dir| dir.join(format!("vw-{}.key", environment.name)))
                .map(|path| path.to_string())
                .unwrap_or_else(|_| {
                    format!("~/.ssh/vw-{}.key", environment.name)
                });
            Some(format!(
                "  ssh -i {key} {user}@{ip}{}",
                format!("  # {label}").bright_black()
            ))
        })
        .collect();

    if reachable.is_empty() {
        return;
    }

    println!();
    println!("log in with:");
    for line in reachable {
        println!("{line}");
    }
}

async fn delete(session: &Session, name: &str) -> Result<(), CloudError> {
    vw_api_client::retrying(|| session.client.delete_environment(name))
        .await
        .map_err(|e| session.error(e))?;
    println!(
        "{} Deleted cloud environment: {}",
        "✓".bright_green(),
        name.cyan()
    );
    Ok(())
}

/// The environment's instances in [`INSTANCES`] order.
fn instances(
    environment: &types::Environment,
) -> [&Option<types::OxideInstance>; 3] {
    [
        &environment.vivado_instance,
        &environment.helios_instance,
        &environment.artifact_instance,
    ]
}

/// A one line rendering of which of an environment's instances are up.
fn instance_summary(environment: &types::Environment) -> String {
    INSTANCES
        .iter()
        .zip(instances(environment))
        .map(|((label, _), instance)| match instance {
            Some(instance) => {
                format!("{label}: {}", colored_state(&instance.state))
            }
            None => format!("{label}: {}", "none".bright_black()),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Write an environment's ssh key out where ssh can find it.
///
/// The service generates a keypair per environment and attaches it to every
/// instance, so this is all that stands between `vw cloud create` and being
/// able to log in.
async fn fetch_keys(
    session: &Session,
    name: &str,
    dir: Option<&Utf8Path>,
) -> Result<(), CloudError> {
    let keys =
        vw_api_client::retrying(|| session.client.get_environment_keys(name))
            .await
            .map_err(|e| session.error(e))?
            .into_inner();

    let (private, public) = save_keys(name, &keys, dir)?;
    report_keys(&private, &public);

    Ok(())
}

/// Write an environment's keypair into `dir`, returning the paths written.
///
/// Replaces whatever was there. An environment's keypair is generated once,
/// when it is created, so a file already sitting at one of these paths belongs
/// to an earlier environment of the same name — which cannot still exist, or
/// this one could not have been created. Keeping it would only leave a key to
/// nowhere in the way of the one that works.
fn save_keys(
    name: &str,
    keys: &types::SshKeyPair,
    dir: Option<&Utf8Path>,
) -> Result<(Utf8PathBuf, Utf8PathBuf), CloudError> {
    let dir = match dir {
        Some(dir) => dir.to_owned(),
        None => default_key_dir()?,
    };
    let private = dir.join(format!("vw-{name}.key"));
    let public = dir.join(format!("vw-{name}.pub"));

    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudError::KeyDir(dir.clone(), e))?;
    write_key(&private, keys.private_key.as_bytes(), true)?;
    write_key(&public, keys.public_key.as_bytes(), false)?;

    Ok((private, public))
}

fn report_keys(private: &Utf8Path, public: &Utf8Path) {
    println!("{} Wrote {}", "✓".bright_green(), private.as_str().cyan());
    println!("{} Wrote {}", "✓".bright_green(), public.as_str().cyan());
}

/// Where keys go when the caller does not say: alongside every other ssh key.
fn default_key_dir() -> Result<Utf8PathBuf, CloudError> {
    let home = dirs::home_dir().ok_or(CloudError::NoHomeDirectory)?;
    let home =
        Utf8PathBuf::from_path_buf(home).map_err(CloudError::HomeNotUtf8)?;
    Ok(home.join(".ssh"))
}

/// Write a key file, keeping a private one to the current user.
///
/// ssh refuses to use a private key that anyone else can read, so getting the
/// mode wrong here would leave a key that looks fine and does not work.
fn write_key(
    path: &Utf8Path,
    contents: &[u8],
    private: bool,
) -> Result<(), CloudError> {
    // Removed rather than truncated in place: a key left at 0400 by something
    // else cannot be opened for writing even by its owner, and replacing it is
    // the whole point.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CloudError::KeyWrite(path.to_owned(), e)),
    }

    std::fs::write(path, contents)
        .map_err(|e| CloudError::KeyWrite(path.to_owned(), e))?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CloudError::KeyWrite(path.to_owned(), e))?;
    }
    #[cfg(not(unix))]
    let _ = private;

    Ok(())
}

/// Render an instance state in a colour that says how to feel about it.
///
/// The states an environment moves through are worth telling apart at a
/// glance: whether it is ready, still on its way, deliberately idle, or
/// broken. `colored` drops the escapes when stdout is not a terminal, so
/// piping this stays plain.
fn colored_state(state: &types::InstanceState) -> ColoredString {
    let text = state.to_string();
    match state {
        // Up and usable.
        types::InstanceState::Running => text.green(),
        // On its way somewhere. Nothing to do but wait.
        types::InstanceState::Creating
        | types::InstanceState::Starting
        | types::InstanceState::Stopping
        | types::InstanceState::Rebooting
        | types::InstanceState::Migrating
        | types::InstanceState::Repairing => text.magenta(),
        // Idle, and fine.
        types::InstanceState::Stopped => text.bright_black(),
        // Broken, or gone while the service still expects it to be here.
        types::InstanceState::Failed | types::InstanceState::Destroyed => {
            text.red()
        }
    }
}

/// The Github access token to authenticate with, if this machine has one.
///
/// A missing `~/.netrc`, or one with no entry for Github, yields `None` rather
/// than an error — the service may not require authorization. A netrc that
/// exists but cannot be read or parsed is still an error, since that is a
/// broken setup the user wants to hear about.
fn access_token() -> Result<Option<String>, CloudError> {
    for host in CREDENTIAL_HOSTS {
        if let Some(token) = vw_lib::get_access_token_from_netrc(host)? {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

/// Open a vivado session on an environment's instance.
///
/// What comes back drives exactly like a local worker, because it implements
/// the same trait and speaks the same protocol. The worker starts when this
/// socket opens and dies when it closes, so a run never inherits anything from
/// the one before it.
pub async fn open_vivado_session(
    session: &Session,
    target: &Target,
    params: vw_remote::SessionParams,
) -> Result<vw_remote::RemoteBackend<reqwest::Upgraded>, CloudError> {
    let upgraded = vw_api_client::retrying(|| {
        session.client.vivado_session(
            &target.environment,
            &target.workspace,
            Some(params.info_with_stack),
            params.part.as_deref(),
            params.variant.as_deref(),
            Some(params.verbose),
        )
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;

    // No note sink here: where an instance's progress reports belong depends
    // on who is asking. A one-shot run writes them to stderr. A full-screen
    // REPL must not — anything written straight to the terminal lands in the
    // middle of a frame it does not control — so it leaves this unset and the
    // reports fall through to its scrollback instead.
    Ok(vw_remote::RemoteBackend::new(socket))
}

/// How a name was arrived at, for the line that says which one is being used.
///
/// Worth reporting. An environment holding several workspaces means two
/// checkouts can land in one slot and overwrite each other, and the only thing
/// standing between a developer and that is being told which slot they are
/// pushing to and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chose {
    /// Said explicitly: an argument, or `$VW_ENV`.
    Named,
    /// `vw-cloud.toml`, in this checkout.
    File,
    /// `[workspace] name`, from `vw.toml`.
    Manifest,
    /// The only environment there is.
    Inferred,
}

impl Chose {
    fn describe(&self) -> &'static str {
        match self {
            Chose::Named => "named",
            Chose::File => crate::cloud_config::FILE,
            Chose::Manifest => "vw.toml",
            Chose::Inferred => "the only one",
        }
    }
}

/// Which environment a command acts on, and which slot on it.
///
/// One value rather than two arguments, because they travel together through
/// every call from here down and a signature taking two strings of the same
/// type is a signature somebody eventually passes backwards.
pub struct Target {
    pub environment: String,
    pub workspace: String,
    environment_from: Chose,
    workspace_from: Chose,
}

impl Target {
    /// Say which environment and slot this is, and how each was decided.
    ///
    /// Printed by anything that pushes. "Why did my feature branch overwrite
    /// main" has no visible cause otherwise.
    pub fn announce(&self) {
        println!(
            "{} {} {} {} {}",
            "\u{2192}".bright_black(),
            self.environment.cyan(),
            format!("({})", self.environment_from.describe()).bright_black(),
            self.workspace.cyan(),
            format!("({})", self.workspace_from.describe()).bright_black(),
        );
    }
}

/// The slot this checkout occupies, and what said so.
///
/// `vw-cloud.toml` first, so a second checkout of one project can take a slot
/// of its own; `vw.toml` otherwise, which is what a single checkout wants and
/// never has to think about.
///
/// Deliberately not the same value as `[workspace] name` when the two differ.
/// That name is what the workspace's own imports resolve through and what
/// `vw::project_name` reports, and moving it would change what gets built
/// rather than only where it is kept.
pub fn workspace_slot(
    workspace_dir: &Utf8Path,
) -> Result<(String, Chose), CloudError> {
    let settings = crate::cloud_config::load(workspace_dir)?;
    if let Some(name) = settings.workspace {
        vw_lib::validate_workspace_name(&name)
            .map_err(|e| CloudError::BadWorkspaceName(name.clone(), e))?;
        return Ok((name, Chose::File));
    }

    let config = vw_lib::load_workspace_config(workspace_dir)
        .map_err(CloudError::WorkspaceConfig)?;
    Ok((config.workspace.name, Chose::Manifest))
}

/// Which environment and slot a command should act on.
///
/// The environment is settled in order: what was said, then what this checkout
/// records, then the only one there is. Two environments, nothing said and
/// nothing recorded is a question only the developer can answer, and guessing
/// would build somewhere they did not intend.
pub async fn resolve_target(
    session: &Session,
    named: Option<&str>,
) -> Result<Target, CloudError> {
    let workspace_dir = crate::cloud_sync::workspace_root()?;
    let (workspace, workspace_from) = workspace_slot(&workspace_dir)?;
    let settings = crate::cloud_config::load(&workspace_dir)?;

    // `$VW_ENV` arrives already folded into `named` for the commands clap
    // reads it for, and is read here for `vw cloud`, which has nowhere to put
    // a flag. Either way somebody said it, which is what `Named` records.
    let said = named
        .map(str::to_owned)
        .or_else(|| std::env::var("VW_ENV").ok().filter(|v| !v.is_empty()));

    if let Some(environment) = said {
        return Ok(Target {
            environment,
            workspace,
            environment_from: Chose::Named,
            workspace_from,
        });
    }

    if let Some(environment) = settings.environment {
        return Ok(Target {
            environment,
            workspace,
            environment_from: Chose::File,
            workspace_from,
        });
    }

    let environments =
        vw_api_client::retrying(|| session.client.get_environments())
            .await
            .map_err(|e| session.error(e))?
            .into_inner()
            .items;

    match environments.len() {
        0 => Err(CloudError::NoEnvironments),
        1 => Ok(Target {
            environment: environments[0].name.clone(),
            workspace,
            environment_from: Chose::Inferred,
            workspace_from,
        }),
        _ => Err(CloudError::AmbiguousEnvironment(
            environments.iter().map(|e| e.name.clone()).collect(),
        )),
    }
}

impl Session {
    /// A session pointed at whatever service the environment names.
    ///
    /// For commands that are not `vw cloud` and so have no `--url` of their
    /// own. Same variable, same default, so a developer points `$VW_SVC_URL` at
    /// a deployment once and every command finds it — which is the point of its
    /// being a variable rather than only a flag: `vw run`, `vw check` and the
    /// rest have nowhere to put a flag.
    ///
    /// `insecure` comes from the command's own flag; `VW_SVC_INSECURE` says
    /// the same thing for a shell that talks to a development service all day
    /// and would otherwise pass the flag every time. Either is enough.
    pub fn from_env(insecure: bool) -> Result<Session, CloudError> {
        let url = service_url_from_env();
        let insecure = insecure
            || std::env::var("VW_SVC_INSECURE")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        Session::new(&url, insecure)
    }
}

/// Remove the build output on an environment's instances.
pub async fn clean_build_output(
    session: &Session,
    target: &Target,
) -> Result<(), CloudError> {
    crate::cloud_sync::clean(session, target).await
}

/// Push the workspace to an environment before building in it.
///
/// A build reads what is on the instance, so this is what makes it the same
/// code the developer is looking at.
pub async fn sync_for_build(
    session: &Session,
    target: &Target,
    only: Option<vw_api_types_versions::latest::TargetKind>,
) -> Result<(), CloudError> {
    crate::cloud_sync::run(
        session,
        target,
        false,
        false,
        std::time::Duration::from_millis(0),
        only,
    )
    .await
}

/// List an environment's artifacts, or fetch some of them.
///
/// Everything comes through the service rather than from the store directly.
/// The store is on the rack's internal network and its instance's external
/// address is usually only reachable over a VPN — needing one to collect a
/// build's output would make this useless from a train, which is exactly where
/// people want it.
async fn artifacts(
    session: &Session,
    target: &Target,
    get: &[String],
    all: bool,
    clear: bool,
    flush: bool,
    out: Option<&Utf8Path>,
) -> Result<(), CloudError> {
    if clear {
        return clear_artifacts(session, target).await;
    }

    if flush {
        flush_artifacts(session, target).await;
    }

    let available = vw_api_client::retrying(|| {
        session
            .client
            .get_artifacts(&target.environment, &target.workspace)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let wanted: Vec<&vw_api_types_versions::latest::Artifact> = if all {
        available.iter().collect()
    } else if get.is_empty() {
        show(&available);
        return Ok(());
    } else {
        select(&available, get)?
    };

    if wanted.is_empty() {
        println!("{}", "no artifacts to download".bright_black());
        return Ok(());
    }

    let directory = out.unwrap_or(Utf8Path::new("."));
    std::fs::create_dir_all(directory)
        .map_err(|e| CloudError::KeyDir(directory.to_owned(), e))?;

    for artifact in wanted {
        download(session, target, artifact, directory).await?;
    }

    Ok(())
}

/// Wait for the environment to finish uploading what its builds produced.
///
/// Best-effort, and deliberately so. The artifacts already in the store are
/// worth listing whether or not this works, and it can fail for reasons that
/// have nothing to do with them — most often an instance that has been stopped
/// or replaced since the build, which is an ordinary way to collect and not a
/// failure. What it must not do is fail quietly: the whole point of asking is
/// to find out whether the listing that follows is the complete one, so
/// anything short of a settled flush is said out loud.
///
/// A caller that needs the failure to be fatal gets it from `--get`, which
/// already refuses a pattern that matches nothing.
async fn flush_artifacts(session: &Session, target: &Target) {
    let flushed = match vw_api_client::retrying(|| {
        session
            .client
            .flush_artifacts(&target.environment, &target.workspace)
    })
    .await
    {
        Ok(flushed) => flushed.into_inner(),
        Err(e) => {
            eprintln!(
                "{} could not wait for uploads to finish: {}",
                "warning:".yellow(),
                session.error(e),
            );
            eprintln!(
                "{}",
                "the listing below may be missing artifacts that had not \
                 been uploaded yet"
                    .bright_black(),
            );
            return;
        }
    };

    if !flushed.settled {
        eprintln!(
            "{} the instance did not finish uploading in time; the listing \
             below may be short",
            "warning:".yellow(),
        );
        return;
    }

    // Nothing to say when nothing moved: the ordinary case is a build whose
    // artifacts were all uploaded before anyone asked, and reporting "0" every
    // time would only teach people to stop reading it.
    if flushed.uploaded > 0 {
        println!(
            "{} waited for {} artifact(s) to finish uploading",
            "\u{2713}".bright_green(),
            flushed.uploaded.to_string().cyan(),
        );
    }
}

/// Which artifacts the `--get` patterns name.
///
/// Every pattern is a glob over the whole artifact name, and `*` crosses `/`
/// freely — so `*.edif` finds the netlists under place/, route/ and synth/
/// without anyone having to know to write `**`. These names are a flat
/// namespace that happens to contain slashes, not a filesystem, and treating
/// the slash as a boundary would only make the obvious pattern the wrong one.
///
/// A name with no glob characters in it is a pattern that matches only itself,
/// so naming a single artifact still works exactly as it did.
///
/// A pattern matching nothing is an error naming that pattern. Downloading the
/// four things that did match and saying nothing about the fifth is how
/// somebody ends up with a partial set of artifacts and no idea.
fn select<'a>(
    available: &'a [vw_api_types_versions::latest::Artifact],
    patterns: &[String],
) -> Result<Vec<&'a vw_api_types_versions::latest::Artifact>, CloudError> {
    let mut chosen: Vec<&vw_api_types_versions::latest::Artifact> = Vec::new();

    for pattern in patterns {
        let glob = glob::Pattern::new(pattern).map_err(|e| {
            CloudError::BadArtifactPattern(pattern.clone(), e.to_string())
        })?;

        let matched: Vec<_> = available
            .iter()
            .filter(|artifact| glob.matches(&artifact.name))
            .collect();

        if matched.is_empty() {
            return Err(CloudError::NoSuchArtifact(pattern.clone()));
        }

        // Two patterns overlapping is an ordinary thing to type, and it should
        // not mean downloading the same file twice.
        for artifact in matched {
            if !chosen.iter().any(|held| held.name == artifact.name) {
                chosen.push(artifact);
            }
        }
    }

    // The order things are listed in, so a download reads like the listing it
    // was chosen from rather than like the order the patterns were typed.
    chosen.sort_by(|a, b| {
        source_order(a.kind)
            .cmp(&source_order(b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(chosen)
}

/// Throw away everything an environment has stored.
///
/// No confirmation, matching the rest of `vw cloud` — `delete` takes three
/// instances down without asking either. What it does report is exactly what
/// went, since that is the only record left of it.
async fn clear_artifacts(
    session: &Session,
    target: &Target,
) -> Result<(), CloudError> {
    let cleared = vw_api_client::retrying(|| {
        session
            .client
            .clear_artifacts(&target.environment, &target.workspace)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    if cleared.removed == 0 {
        println!("{}", "nothing stored to clear".bright_black());
        return Ok(());
    }

    println!(
        "{} removed {} artifact(s), {}",
        "\u{2713}".bright_green(),
        cleared.removed,
        human_bytes(cleared.bytes),
    );

    Ok(())
}

/// Show what an environment has built.
///
/// Grouped by the instance that made it, because a flat alphabetical list
/// interleaves two unrelated builds — a vivado report between two driver
/// binaries tells nobody anything.
fn show(available: &[vw_api_types_versions::latest::Artifact]) {
    if available.is_empty() {
        println!(
            "{}",
            "no artifacts yet; run a build that produces one".bright_black(),
        );
        return;
    }

    let mut sorted: Vec<&vw_api_types_versions::latest::Artifact> =
        available.iter().collect();
    sorted.sort_by(|a, b| {
        source_order(a.kind)
            .cmp(&source_order(b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });

    for artifact in sorted {
        println!(
            "{:<10} {:>10}  {}",
            colored_source(artifact.kind),
            human_bytes(artifact.size),
            artifact.name,
        );
    }
}

/// Which instance built it, in a colour that is not the other one's.
///
/// Two builds land in the same listing and they have nothing to do with each
/// other; telling them apart should not require reading.
fn colored_source(
    kind: vw_api_types_versions::latest::TargetKind,
) -> colored::ColoredString {
    let name = kind.to_string();
    match kind {
        vw_api_types_versions::latest::TargetKind::Vivado => name.cyan(),
        vw_api_types_versions::latest::TargetKind::Helios => name.magenta(),
    }
}

/// Sort key for the instance that built something.
///
/// Fixed rather than alphabetical so the order does not change if a kind is
/// ever renamed, and so hardware comes before software, which is the order
/// they happen in.
fn source_order(kind: vw_api_types_versions::latest::TargetKind) -> u8 {
    match kind {
        vw_api_types_versions::latest::TargetKind::Vivado => 0,
        vw_api_types_versions::latest::TargetKind::Helios => 1,
    }
}

/// Fetch one artifact into `directory`.
async fn download(
    session: &Session,
    target: &Target,
    artifact: &vw_api_types_versions::latest::Artifact,
    directory: &Utf8Path,
) -> Result<(), CloudError> {
    use futures::StreamExt;

    let response = vw_api_client::retrying(|| {
        session.client.get_artifact(
            &target.environment,
            &target.workspace,
            &artifact.kind,
            &artifact.name,
        )
    })
    .await
    .map_err(|e| session.error(e))?;

    // An artifact's name carries the stage that produced it — `synth/x.edif`
    // and `route/x.edif` are different netlists — so the structure is kept on
    // the way down rather than flattened into collisions.
    let path = directory.join(safe_name(&artifact.name)?);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CloudError::KeyDir(parent.to_owned(), e))?;
    }

    // Written as it arrives rather than collected first: an image runs to
    // hundreds of megabytes and there is no reason for it to be in memory on
    // the way past.
    let mut file = std::fs::File::create(&path)
        .map_err(|e| CloudError::KeyWrite(path.clone(), e))?;

    let mut body = response.into_inner_stream();
    let mut written = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| CloudError::Transport(e.to_string()))?;
        std::io::Write::write_all(&mut file, &chunk)
            .map_err(|e| CloudError::KeyWrite(path.clone(), e))?;
        written += chunk.len() as u64;
    }

    println!(
        "{} {} ({})",
        "\u{2713}".bright_green(),
        path.as_str(),
        human_bytes(written),
    );

    Ok(())
}

/// An artifact's name, once it has been established that it is only a name.
///
/// The name becomes a path on the developer's machine, and it arrives from a
/// service. Nothing we run puts anything strange in it, but "nothing we run"
/// is not the same as "nothing", and a download that can write outside the
/// directory it was pointed at is the kind of thing that is obvious only
/// afterwards.
fn safe_name(name: &str) -> Result<&str, CloudError> {
    let refused = || CloudError::UnsafeArtifactName(name.to_owned());

    if name.is_empty() || name.starts_with('/') || name.contains('\\') {
        return Err(refused());
    }
    for component in name.split('/') {
        if component.is_empty() || component == ".." || component == "." {
            return Err(refused());
        }
    }

    Ok(name)
}

/// A byte count as a person would say it.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Bring the VHDL vivado generated for this environment's IP into the local
/// tree.
///
/// A static analysis running here has to resolve `entity ip.<name>_wrapper`
/// and `entity xil_defaultlib.<name>`, and those only exist where vivado ran.
/// They land at the same paths they have on the instance, so a language server
/// can open them and "go to definition" arrives somewhere real.
///
/// Only what differs is fetched. A check that changed no IP therefore costs
/// one round trip, which matters because this runs before every check.
pub async fn fetch_generated_ip(
    session: &Session,
    target: &Target,
    workspace: &Utf8Path,
) -> Result<usize, CloudError> {
    let manifest = vw_api_client::retrying(|| {
        session
            .client
            .generated_manifest(&target.environment, &target.workspace)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let mut written = 0usize;
    for entry in &manifest.entries {
        let path = workspace.join(safe_name(&entry.path)?);

        // Already here and already right. The common case: IP changes rarely
        // and a check runs constantly.
        if let Ok(existing) = std::fs::read(&path) {
            if vw_sync::digest_bytes(&existing) == entry.digest {
                continue;
            }
        }

        let contents = vw_api_client::retrying(|| {
            session.client.generated_file(
                &target.environment,
                &target.workspace,
                &entry.path,
            )
        })
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

        let bytes = futures::TryStreamExt::try_fold(
            contents.into_inner(),
            Vec::new(),
            |mut collected, chunk| async move {
                collected.extend_from_slice(&chunk);
                Ok(collected)
            },
        )
        .await
        .map_err(|e| CloudError::Transport(e.to_string()))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CloudError::KeyDir(parent.to_owned(), e))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| CloudError::KeyWrite(path.clone(), e))?;
        written += 1;
    }

    Ok(written)
}

#[cfg(test)]
mod test {
    use super::*;

    use vw_api_types_versions::latest::{Artifact, TargetKind};

    /// `vw cloud` with nothing but a subcommand, so that what comes back is
    /// whatever the defaults and the environment resolved to.
    #[derive(clap::Parser)]
    struct Wrapper {
        #[command(flatten)]
        cloud: CloudArgs,
    }

    fn resolved(argv: &[&str]) -> (String, String) {
        use clap::Parser as _;

        let parsed = Wrapper::try_parse_from(argv).expect("should parse");

        // The same order `run` resolves them in: the service URL first, and
        // the administrative one from it unless it was named outright.
        let url = parsed
            .cloud
            .url
            .clone()
            .unwrap_or_else(|| String::from(SERVICE_URL));
        let admin = parsed
            .cloud
            .admin_url
            .unwrap_or_else(|| admin_url_for(&url));
        (url, admin)
    }

    /// A URL is the whole of how a deployment is chosen.
    ///
    /// There is no flag or variable naming a deployment, and no list of them
    /// here: one vw service answers at one URL, and which deployment that is is
    /// the service's business rather than the client's. So this is about the
    /// only two things a caller can say — nothing, and a URL.
    ///
    /// One test rather than several because the variables are process-global:
    /// two tests setting them would race under the default parallel harness.
    /// Same reason the `$VW_ENV` test in `main.rs` is one test.
    #[test]
    fn a_url_is_how_a_deployment_is_chosen() {
        // A developer's shell may well have $VW_SVC_URL and $VW_SVC_ADMIN_URL
        // exported, and this process inherited whatever it has. The first half
        // of this test is about the defaults, so they must not be at its mercy.
        let inherited: Vec<(&str, Option<String>)> =
            ["VW_SVC_URL", "VW_SVC_ADMIN_URL"]
                .iter()
                .map(|name| (*name, std::env::var(name).ok()))
                .collect();
        for (name, _) in &inherited {
            std::env::remove_var(name);
        }

        // Saying nothing reaches the deployment nearly every invocation wants.
        // The user API carries no port because it answers on 443; the admin API
        // is the same host on its own port.
        let default = (
            String::from("https://vw-cloud.dev"),
            String::from("https://vw-cloud.dev:2053"),
        );
        assert_eq!(resolved(&["vw", "list"]), default);
        assert_eq!(service_url_from_env(), default.0);

        // Another deployment is reached by naming it, on the flag -- and BOTH
        // APIs go with it. This is the property worth having a test for: an
        // admin session left behind on production while --url points at the
        // beta is a `vw cloud admin delete` against the wrong deployment's
        // environments, and it looks exactly like the right one.
        assert_eq!(
            resolved(&["vw", "--url", "https://beta.vw-cloud.dev", "list"]),
            (
                String::from("https://beta.vw-cloud.dev"),
                String::from("https://beta.vw-cloud.dev:2053"),
            ),
        );

        // ...or in the environment, which is what the commands with no --url of
        // their own -- `vw run`, `vw check` -- have to go on, and which follows
        // the same rule.
        std::env::set_var("VW_SVC_URL", "https://beta.vw-cloud.dev");
        assert_eq!(
            resolved(&["vw", "list"]),
            (
                String::from("https://beta.vw-cloud.dev"),
                String::from("https://beta.vw-cloud.dev:2053"),
            ),
        );
        assert_eq!(service_url_from_env(), "https://beta.vw-cloud.dev");

        // A development service on some other port keeps its host and gets the
        // admin port, rather than keeping the port it was given.
        assert_eq!(
            resolved(&["vw", "--url", "https://elsewhere:1234", "list"]),
            (
                String::from("https://elsewhere:1234"),
                String::from("https://elsewhere:2053"),
            ),
        );

        // A trailing slash survives nothing: the generated client appends
        // `/v1/...` to whatever it is given.
        assert_eq!(
            resolved(&["vw", "--url", "https://beta.vw-cloud.dev/", "list"]).1,
            "https://beta.vw-cloud.dev:2053",
        );

        // And a deployment that puts its administrative API somewhere else
        // entirely says so, which is the only thing that separates the two.
        std::env::set_var("VW_SVC_ADMIN_URL", "https://admin.example:9999");
        assert_eq!(
            resolved(&["vw", "--url", "https://beta.vw-cloud.dev", "list"]).1,
            "https://admin.example:9999",
        );
        std::env::remove_var("VW_SVC_ADMIN_URL");

        for (name, value) in inherited {
            match value {
                Some(url) => std::env::set_var(name, url),
                None => std::env::remove_var(name),
            }
        }
    }

    /// A real environment's listing, which is what the patterns have to be
    /// good for.
    fn listing() -> Vec<Artifact> {
        let vivado = [
            "image/vpk120.pdi",
            "place/top_vpk120-netlist.edif",
            "reports/top_vpk120-bus-skew.rpt",
            "reports/top_vpk120-clock-utilization.rpt",
            "reports/top_vpk120-drc.rpt",
            "reports/top_vpk120-methodology.rpt",
            "reports/top_vpk120-place-timing.rpt",
            "reports/top_vpk120-place-utilization.rpt",
            "reports/top_vpk120-power.rpt",
            "reports/top_vpk120-route-status.rpt",
            "reports/top_vpk120-route-timing.rpt",
            "reports/top_vpk120-route-utilization.rpt",
            "reports/top_vpk120-synth-utilization.rpt",
            "reports/top_vpk120-timing-detail.rpt",
            "reports/top_vpk120-timing-summary.rpt",
            "route/top_vpk120-netlist.edif",
            "synth/top_vpk120-netlist.edif",
        ];
        let helios = ["release/rh", "x86_64-illumos/release/rhdrv"];

        vivado
            .into_iter()
            .map(|name| (name, TargetKind::Vivado))
            .chain(helios.into_iter().map(|name| (name, TargetKind::Helios)))
            .map(|(name, kind)| Artifact {
                name: name.to_owned(),
                kind,
                size: 1,
                modified: None,
            })
            .collect()
    }

    fn matching(patterns: &[&str]) -> Vec<String> {
        let available = listing();
        let patterns: Vec<String> =
            patterns.iter().map(|p| (*p).to_owned()).collect();
        select(&available, &patterns)
            .expect("patterns should match")
            .into_iter()
            .map(|a| a.name.clone())
            .collect()
    }

    #[test]
    fn a_directory_pattern_takes_what_is_under_it() {
        assert_eq!(matching(&["place/*"]), ["place/top_vpk120-netlist.edif"]);
    }

    #[test]
    fn a_pattern_can_match_part_of_a_name() {
        assert_eq!(
            matching(&["reports/*place*"]),
            [
                "reports/top_vpk120-place-timing.rpt",
                "reports/top_vpk120-place-utilization.rpt",
            ],
        );
    }

    #[test]
    fn a_star_crosses_a_slash() {
        // The reason for the non-standard glob semantics: `*.edif` is what
        // somebody types when they want the netlists, and they are in three
        // different directories. Requiring `**/*.edif` would make the obvious
        // pattern silently the wrong one.
        assert_eq!(
            matching(&["*.edif"]),
            [
                "place/top_vpk120-netlist.edif",
                "route/top_vpk120-netlist.edif",
                "synth/top_vpk120-netlist.edif",
            ],
        );
    }

    #[test]
    fn an_exact_name_still_names_one_thing() {
        // What `--get` meant before it took patterns, and has to keep meaning.
        assert_eq!(matching(&["image/vpk120.pdi"]), ["image/vpk120.pdi"],);
    }

    #[test]
    fn overlapping_patterns_do_not_download_anything_twice() {
        let found = matching(&["reports/*place*", "*place-timing*"]);

        assert_eq!(
            found,
            [
                "reports/top_vpk120-place-timing.rpt",
                "reports/top_vpk120-place-utilization.rpt",
            ],
        );
    }

    #[test]
    fn results_come_back_in_the_order_they_are_listed_in() {
        // Patterns named backwards, and by the two different builders.
        let found = matching(&["*rhdrv", "image/*"]);

        assert_eq!(
            found,
            ["image/vpk120.pdi", "x86_64-illumos/release/rhdrv"],
            "vivado before helios, whatever order was typed",
        );
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_refused() {
        // Rather than downloading the rest and leaving somebody with a partial
        // set they think is complete.
        let available = listing();
        let patterns = vec!["place/*".to_owned(), "nosuch/*".to_owned()];

        let refused = select(&available, &patterns);

        assert!(
            matches!(refused, Err(CloudError::NoSuchArtifact(p)) if p == "nosuch/*"),
        );
    }

    #[test]
    fn a_malformed_pattern_says_so() {
        let available = listing();
        let patterns = vec!["reports/[".to_owned()];

        assert!(matches!(
            select(&available, &patterns),
            Err(CloudError::BadArtifactPattern(..)),
        ));
    }

    /// The escape `colored` emits for each colour we use.
    const GREEN: &str = "\u{1b}[32m";
    const RED: &str = "\u{1b}[31m";
    const MAGENTA: &str = "\u{1b}[35m";
    const GRAY: &str = "\u{1b}[90m";

    #[test]
    fn instance_states_are_coloured_by_meaning() {
        // `colored` suppresses escapes off a terminal, which is what the test
        // harness looks like.
        colored::control::set_override(true);

        let rendered =
            |state: types::InstanceState| colored_state(&state).to_string();

        for (state, expected) in [
            (types::InstanceState::Running, GREEN),
            // Everything in motion reads the same, because the answer is
            // always "wait".
            (types::InstanceState::Creating, MAGENTA),
            (types::InstanceState::Starting, MAGENTA),
            (types::InstanceState::Stopping, MAGENTA),
            (types::InstanceState::Rebooting, MAGENTA),
            (types::InstanceState::Migrating, MAGENTA),
            (types::InstanceState::Repairing, MAGENTA),
            (types::InstanceState::Stopped, GRAY),
            (types::InstanceState::Failed, RED),
            (types::InstanceState::Destroyed, RED),
        ] {
            let text = rendered(state);
            assert!(
                text.starts_with(expected),
                "{state} should be coloured {expected:?}, got {text:?}",
            );
            // The state name itself still has to be readable.
            assert!(text.contains(&state.to_string()));
        }
    }
}
