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
use vw_api_client::user::{types, Client};

/// Where the service lives if the caller does not say otherwise. Matches
/// `vw-svc`'s own default user API port.
const DEFAULT_SERVICE_URL: &str = "http://localhost:2727";

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
    #[error("reading the workspace configuration")]
    Workspace(#[source] vw_lib::VwError),
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
            )
            .await
        }
        CloudCommand::Get { name } => get(&session, &name).await,
        CloudCommand::Delete { name } => delete(&session, &name).await,
        CloudCommand::Keys { name, dir } => {
            fetch_keys(&session, &name, dir.as_deref()).await
        }
        CloudCommand::Sync {
            name,
            watch,
            debounce,
        } => {
            crate::cloud_sync::run(
                &session,
                &name,
                watch,
                std::time::Duration::from_millis(debounce),
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

    Ok(())
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
