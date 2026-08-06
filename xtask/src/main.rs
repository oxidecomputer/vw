// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Development tasks for the vw workspace.
//!
//! Run through the workspace alias:
//!
//! ```text
//! cargo xtask devcerts    # generate a self-signed cert for `vw-svc --tls`
//! cargo xtask openapi     # manage the checked-in OpenAPI documents
//! ```
//!
//! One entry point, so `cargo xtask` on its own lists everything a developer
//! can do here. Tasks with heavy dependencies live in their own crates and are
//! reached through [`external`], so having them listed costs nothing to
//! somebody who does not run them.

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

mod devcerts;
mod external;

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Development tasks for the vw workspace")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Generate a self-signed certificate for local https")]
    Devcerts(DevcertArgs),

    #[command(
        about = "Manage the checked-in OpenAPI documents",
        long_about = "Manage the checked-in OpenAPI documents under \
                      `openapi/`. Run `cargo xtask openapi --help` for what \
                      it can do; `generate` writes them from the API traits \
                      and `check` fails if what is on disk is out of date."
    )]
    Openapi(external::External),
}

#[derive(Parser)]
pub struct DevcertArgs {
    /// Directory to write `cert.pem` and `key.pem` into.
    #[arg(default_value = devcerts::DEFAULT_DIR)]
    pub dir: Utf8PathBuf,

    /// Additional name the certificate should be valid for. A DNS name or an
    /// IP address; may be given more than once.
    #[arg(long = "san", value_name = "NAME")]
    pub subject_alt_names: Vec<String>,

    /// How many days the certificate is valid for.
    #[arg(long, default_value_t = 365)]
    pub days: u16,

    /// Replace an existing certificate and key in the target directory.
    #[arg(long)]
    pub force: bool,
}

/// Whatever went wrong, from whichever task.
#[derive(Debug, thiserror::Error)]
enum XtaskError {
    #[error(transparent)]
    Devcerts(#[from] devcerts::DevcertError),
    #[error(transparent)]
    External(#[from] external::ExternalError),
}

fn main() {
    let cli = Cli::parse();
    let result: Result<(), XtaskError> = match cli.command {
        Command::Devcerts(args) => devcerts::run(args).map_err(Into::into),
        // Replaces this process, so it only returns if it could not start.
        Command::Openapi(external) => external
            .exec("vw-openapi-manager", "vw-openapi-manager")
            .map_err(Into::into),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        // The source chain matters here: "cannot run vw-openapi-manager" on
        // its own does not say whether cargo is missing or the crate failed
        // to build.
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}
