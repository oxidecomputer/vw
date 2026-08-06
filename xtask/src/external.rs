// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Tasks that live in another crate.
//!
//! Every development task should be findable by running `cargo xtask`, but not
//! every one should be paid for by everybody. The OpenAPI manager pulls in the
//! whole API surface and its schema machinery; somebody who only wants a
//! development certificate should not wait for that to compile.
//!
//! So the subcommand is here and the work is elsewhere. [`External`] swallows
//! every argument it is given — including `--help`, so the real tool answers
//! that rather than this one — and hands them to `cargo run` for the crate
//! that does the work. The process is replaced rather than spawned, so exit
//! codes, signals and terminal behaviour are the tool's own.
//!
//! The pattern is maghemite's, which took it from omicron.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use clap::Parser;

/// Argument parser for a task implemented in another crate.
#[derive(Debug, Parser)]
#[command(
    disable_help_flag(true),
    disable_help_subcommand(true),
    disable_version_flag(true)
)]
pub struct External {
    #[arg(trailing_var_arg(true), allow_hyphen_values(true))]
    args: Vec<OsString>,
}

impl External {
    /// Run `bin` from `package`, passing on everything this was given.
    ///
    /// Only returns if the tool could not be started at all; otherwise this
    /// process becomes that one.
    pub fn exec(self, package: &str, bin: &str) -> Result<(), ExternalError> {
        // The same cargo that invoked this xtask, so a `+toolchain` or a
        // rustup shim in play stays in play.
        let cargo = std::env::var_os("CARGO")
            .unwrap_or_else(|| OsString::from("cargo"));

        let error = Command::new(&cargo)
            .args(["run", "--quiet", "--package", package, "--bin", bin])
            .arg("--")
            .args(self.args)
            .exec();

        Err(ExternalError {
            bin: bin.to_owned(),
            source: error,
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cannot run {bin}")]
pub struct ExternalError {
    bin: String,
    #[source]
    source: std::io::Error,
}
