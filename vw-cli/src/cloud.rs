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

/// Where the service lives if the caller does not say otherwise. Matches
/// `vw-svc`'s own default user API port.
const DEFAULT_SERVICE_URL: &str = "http://localhost:2727";

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
        default_value = DEFAULT_SERVICE_URL,
        help = "Base URL of the vw service"
    )]
    url: String,

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
        #[arg(help = "Environment name")]
        name: String,
    },
    #[command(about = "Delete a remote build environment")]
    Delete {
        #[arg(help = "Environment name")]
        name: String,
    },
    #[command(about = "Push the workspace to an environment's instances")]
    Sync {
        #[arg(help = "Environment name")]
        name: String,
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
        #[arg(help = "Environment name")]
        name: String,
        #[arg(
            long,
            value_name = "FILE",
            help = "Download this artifact instead of listing. Repeat, or \
                    use --all, for several."
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
            value_name = "DIR",
            help = "Directory to write downloads into [default: .]"
        )]
        out: Option<Utf8PathBuf>,
    },
    #[command(
        about = "Download the ssh key that opens an environment's instances"
    )]
    Keys {
        #[arg(help = "Environment name")]
        name: String,
        #[arg(
            long,
            value_name = "DIR",
            help = "Directory to write the key into. Replaces any key \
                    already there. [default: ~/.ssh]"
        )]
        dir: Option<Utf8PathBuf>,
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
        "you have several cloud environments ({}); say which with --env, or \
         pass --local to build on this machine",
        .0.join(", ")
    )]
    AmbiguousEnvironment(Vec<String>),
    #[error("no artifact called '{0}'; run `vw cloud artifacts <env>` to see what there is")]
    NoSuchArtifact(String),
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
    let session = Session::new(&args.url, args.insecure)?;

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
        CloudCommand::Get { name } => get(&session, &name).await,
        CloudCommand::Delete { name } => delete(&session, &name).await,
        CloudCommand::Artifacts {
            name,
            get,
            all,
            clear,
            out,
        } => artifacts(&session, &name, &get, all, clear, out.as_deref()).await,
        CloudCommand::Keys { name, dir } => {
            fetch_keys(&session, &name, dir.as_deref()).await
        }
        CloudCommand::Sync {
            name,
            force,
            watch,
            debounce,
        } => {
            crate::cloud_sync::run(
                // `vw cloud sync` is the command that means "everything", so
                // it is the one place with no filter.
                &session,
                &name,
                force,
                watch,
                std::time::Duration::from_millis(debounce),
                None,
            )
            .await
        }
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
            other => CloudError::Transport(with_causes(&other)),
        }
    }
}

/// Flatten an error and everything underneath it onto one line.
///
/// The client's own `Display` stops at "error sending request", which hides
/// the part worth reading — a certificate that did not verify, a refused
/// connection. The causes are what tell someone what to do next.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        // Errors in this chain tend to embed their own cause in their
        // `Display`, so only append what has not been said already.
        let text = error.to_string();
        if !message.contains(&text) {
            message.push_str(&format!(": {text}"));
        }
        cause = error.source();
    }
    message
}

async fn list(session: &Session) -> Result<(), CloudError> {
    // The endpoint takes no pagination parameters, so this one page is every
    // environment the caller owns.
    let page = session
        .client
        .get_environments()
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
    let keys = session
        .client
        .create_environment(name, &images)
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

    println!(
        "{} Created cloud environment: {}",
        "✓".bright_green(),
        name.cyan()
    );

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
        let environment = session
            .client
            .get_environment(name)
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
    let environment = session
        .client
        .get_environment(name)
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
    session
        .client
        .delete_environment(name)
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
    let keys = session
        .client
        .get_environment_keys(name)
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
    environment: &str,
    params: vw_remote::SessionParams,
) -> Result<vw_remote::RemoteBackend<reqwest::Upgraded>, CloudError> {
    let upgraded = session
        .client
        .vivado_session(
            environment,
            Some(params.info_with_stack),
            params.part.as_deref(),
            params.variant.as_deref(),
            Some(params.verbose),
        )
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

/// Which environment a bare `vw run` should use.
///
/// Named explicitly, or inferred when there is no ambiguity to resolve. Two
/// environments and no `--env` is a question only the developer can answer,
/// and guessing at it would run a build somewhere they did not intend.
pub async fn pick_environment(
    session: &Session,
    named: Option<&str>,
) -> Result<String, CloudError> {
    if let Some(name) = named {
        return Ok(name.to_owned());
    }

    let environments = session
        .client
        .get_environments()
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

impl Session {
    /// A session pointed at whatever service the environment names.
    ///
    /// For commands that are not `vw cloud` and so have no `--url` of their
    /// own. Same variable, same default, so a developer configures the service
    /// once and every command finds it.
    ///
    /// `insecure` comes from the command's own flag; `VW_SVC_INSECURE` says
    /// the same thing for a shell that talks to a development service all day
    /// and would otherwise pass the flag every time. Either is enough.
    pub fn from_env(insecure: bool) -> Result<Session, CloudError> {
        let url = std::env::var("VW_SVC_URL")
            .unwrap_or_else(|_| DEFAULT_SERVICE_URL.to_owned());
        let insecure = insecure
            || std::env::var("VW_SVC_INSECURE")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        Session::new(&url, insecure)
    }

    /// Whether the failure was the service being out of reach, rather than the
    /// service saying no.
    ///
    /// The difference decides whether a bare `vw run` may quietly fall back to
    /// building here: unreachable is a working-from-a-train problem, but a
    /// service that answered and refused is telling us something.
    pub fn unreachable(error: &CloudError) -> bool {
        matches!(error, CloudError::Transport(_))
    }
}

/// Remove the build output on an environment's instances.
pub async fn clean_build_output(
    session: &Session,
    environment: &str,
) -> Result<(), CloudError> {
    crate::cloud_sync::clean(session, environment).await
}

/// Push the workspace to an environment before building in it.
///
/// A build reads what is on the instance, so this is what makes it the same
/// code the developer is looking at.
pub async fn sync_for_build(
    session: &Session,
    environment: &str,
    only: Option<vw_api_types_versions::latest::TargetKind>,
) -> Result<(), CloudError> {
    crate::cloud_sync::run(
        session,
        environment,
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
    environment: &str,
    get: &[String],
    all: bool,
    clear: bool,
    out: Option<&Utf8Path>,
) -> Result<(), CloudError> {
    if clear {
        return clear_artifacts(session, environment).await;
    }

    let available = session
        .client
        .get_artifacts(environment)
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

    let wanted: Vec<&vw_api_types_versions::latest::Artifact> = if all {
        available.iter().collect()
    } else if get.is_empty() {
        show(&available);
        return Ok(());
    } else {
        // Named artifacts have to exist, and saying which one does not is more
        // use than a download that quietly produces nothing.
        let mut wanted = Vec::new();
        for name in get {
            let found = available
                .iter()
                .find(|artifact| artifact.name == *name)
                .ok_or_else(|| CloudError::NoSuchArtifact(name.clone()))?;
            wanted.push(found);
        }
        wanted
    };

    if wanted.is_empty() {
        println!("{}", "no artifacts to download".bright_black());
        return Ok(());
    }

    let directory = out.unwrap_or(Utf8Path::new("."));
    std::fs::create_dir_all(directory)
        .map_err(|e| CloudError::KeyDir(directory.to_owned(), e))?;

    for artifact in wanted {
        download(session, environment, artifact, directory).await?;
    }

    Ok(())
}

/// Throw away everything an environment has stored.
///
/// No confirmation, matching the rest of `vw cloud` — `delete` takes three
/// instances down without asking either. What it does report is exactly what
/// went, since that is the only record left of it.
async fn clear_artifacts(
    session: &Session,
    environment: &str,
) -> Result<(), CloudError> {
    let cleared = session
        .client
        .clear_artifacts(environment)
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
    environment: &str,
    artifact: &vw_api_types_versions::latest::Artifact,
    directory: &Utf8Path,
) -> Result<(), CloudError> {
    use futures::StreamExt;

    let response = session
        .client
        .get_artifact(environment, &artifact.kind, &artifact.name)
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
    environment: &str,
    workspace: &Utf8Path,
) -> Result<usize, CloudError> {
    let manifest = session
        .client
        .generated_manifest(environment)
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

        let contents = session
            .client
            .generated_file(environment, &entry.path)
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
