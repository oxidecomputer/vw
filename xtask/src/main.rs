// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Development tasks for the vw workspace.
//!
//! Run through the workspace alias:
//!
//! ```text
//! cargo xtask devcerts    # generate a self-signed cert for `vw-svc --tls`
//! ```

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

mod devcerts;

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

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Devcerts(args) => devcerts::run(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
