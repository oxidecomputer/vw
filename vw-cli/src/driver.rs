// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw driver build` — building the driver where it runs.
//!
//! The driver targets illumos and pins its own toolchain, so it is built on
//! the helios instance rather than on a developer's machine, which is usually
//! neither. Cargo's output comes back line by line and is printed exactly as
//! cargo wrote it, colour and all — the point is that it looks like a build,
//! because it is one.

use camino::Utf8Path;
use colored::*;
use futures::StreamExt;

use crate::cloud::{CloudError, Session};

/// Build the driver on an environment's helios instance.
///
/// Returns whether the build succeeded, so the caller can decide the exit
/// code; a failed build is not an error in the sense of something having gone
/// wrong with vw.
pub async fn build(
    session: &Session,
    environment: &str,
    release: bool,
    args: &[String],
) -> Result<bool, CloudError> {
    let joined = (!args.is_empty()).then(|| args.join(" "));

    let upgraded = session
        .client
        .driver_build(environment, joined.as_deref(), Some(release))
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

    let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;

    while let Some(message) = socket.next().await {
        let text =
            match message.map_err(|e| CloudError::Transport(e.to_string()))? {
                tokio_tungstenite::tungstenite::Message::Text(text) => text,
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };

        match serde_json::from_str::<vw_remote::DriverEvent>(&text)
            .map_err(|e| CloudError::Transport(e.to_string()))?
        {
            // A driver is more than one cargo invocation, because userland
            // and a kernel module are different targets. Saying which is
            // building keeps two sets of cargo output from reading as one.
            vw_remote::DriverEvent::Building { unit } => {
                println!("{:>12} {unit}", "Building".bright_green().bold());
            }
            // Printed rather than rendered: cargo already said it better than
            // anything here could.
            vw_remote::DriverEvent::Line { text } => println!("{text}"),
            // Named so a developer knows what to look for in the store
            // without having to guess at target directory layout.
            vw_remote::DriverEvent::Produced { artifacts, stored } => {
                for artifact in &artifacts {
                    println!(
                        "{:>12} {artifact}",
                        "Produced".bright_green().bold(),
                    );
                }
                if stored > 0 {
                    println!(
                        "{:>12} {stored} artifact(s) — `vw cloud artifacts` \
                         to fetch them",
                        "Stored".bright_green().bold(),
                    );
                }
            }
            vw_remote::DriverEvent::Done { success, code } => {
                if !success {
                    eprintln!(
                        "{} the driver build failed{}",
                        "error:".bright_red(),
                        match code {
                            Some(code) => format!(" (exit {code})"),
                            None => String::from(" (killed)"),
                        },
                    );
                }
                return Ok(success);
            }
            vw_remote::DriverEvent::Fatal { message } => {
                return Err(CloudError::Transport(message));
            }
        }
    }

    // The socket closed without a verdict, which is not a build result.
    Err(CloudError::Transport(String::from(
        "the instance ended the build without reporting whether it worked",
    )))
}

/// Build the driver on this machine.
///
/// What `--local` does: run the same cargo command in the same directory, so
/// somebody with an illumos toolchain to hand is not forced through a network.
pub async fn build_locally(
    workspace: &Utf8Path,
    release: bool,
    args: &[String],
) -> Result<bool, CloudError> {
    let driver = workspace.join(vw_cloud_driver_dir());
    if !driver.is_dir() {
        return Err(CloudError::NoDriver(driver));
    }

    let mut command = tokio::process::Command::new("cargo");
    command.arg("build");
    if release {
        command.arg("--release");
    }
    command.args(args);

    let status = command
        .current_dir(driver.as_std_path())
        .status()
        .await
        .map_err(|e| CloudError::Transport(format!("running cargo: {e}")))?;

    Ok(status.success())
}

/// The directory a vw workspace keeps its driver in.
fn vw_cloud_driver_dir() -> &'static str {
    crate::cloud_sync::DRIVER
}
