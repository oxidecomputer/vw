// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw driver <command>` — building, linting and testing the driver where it
//! runs.
//!
//! The driver targets illumos and pins its own toolchain, so it is compiled on
//! the helios instance rather than on a developer's machine, which is usually
//! neither — and anything that compiles it, clippy included, has to happen
//! there too. Cargo's output comes back line by line and is printed exactly as
//! cargo wrote it, colour and all — the point is that it looks like a build,
//! because it is one.

use camino::Utf8Path;
use colored::*;
use futures::StreamExt;

use std::io::Write;

use vw_api_types_versions::latest::DriverCommand;

use crate::cloud::{CloudError, Session};

/// What to call each part of the run as it starts, in cargo's own style.
fn verb(command: DriverCommand) -> &'static str {
    match command {
        DriverCommand::Build => "Building",
        DriverCommand::Check => "Checking",
        DriverCommand::Clippy => "Linting",
        DriverCommand::Test => "Testing",
        DriverCommand::Fmt => "Formatting",
        DriverCommand::FmtCheck => "Checking",
    }
}

/// Run a cargo command over the driver on an environment's helios instance.
///
/// Returns whether cargo succeeded, so the caller can decide the exit code; a
/// failed build or a lint that found something is not an error in the sense
/// of something having gone wrong with vw.
///
/// With `diagnostics`, rustc's diagnostics are written there as cargo's own
/// JSON, naming this machine's files, for an editor to read — and the instance
/// runs every part of the build even after one fails, since an editor wants
/// all of them.
pub async fn run(
    session: &Session,
    target: &crate::cloud::Target,
    workspace: &Utf8Path,
    command: DriverCommand,
    release: bool,
    args: &[String],
    mut diagnostics: Option<&mut dyn std::io::Write>,
) -> Result<bool, CloudError> {
    let joined = (!args.is_empty()).then(|| args.join(" "));
    let editor = diagnostics.is_some();

    let upgraded = vw_api_client::retrying(|| {
        session.client.driver_cargo(
            &target.environment,
            &target.workspace,
            joined.as_deref(),
            &command,
            Some(editor),
            Some(release),
        )
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;

    // Whether everything cargo produced made it to the store. A build whose
    // artifacts went nowhere has not done what it was asked: whoever runs it
    // is about to go and collect them, and finding nothing there is a much
    // worse place to learn about it than here.
    let mut unstored = 0;

    // Files formatting would have changed here but did not, because they
    // were edited after they were sent. Counted so the command does not
    // claim the tree is formatted when part of it is not.
    let mut left_alone = 0;

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
                println!("{:>12} {unit}", verb(command).bright_green().bold());
            }
            // Printed rather than rendered: cargo already said it better than
            // anything here could.
            vw_remote::DriverEvent::Line { text } => println!("{text}"),
            // Named so a developer knows what to look for in the store
            // without having to guess at target directory layout.
            vw_remote::DriverEvent::Produced {
                artifacts,
                stored,
                problems,
            } => {
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
                for problem in &problems {
                    eprintln!("{} {problem}", "warning:".yellow());
                }
                unstored = artifacts.len().saturating_sub(stored);
            }
            vw_remote::DriverEvent::Done { success, code } => {
                if !success {
                    eprintln!(
                        "{} `cargo {command}` failed on the driver{}",
                        "error:".bright_red(),
                        match code {
                            Some(code) => format!(" (exit {code})"),
                            None => String::from(" (killed)"),
                        },
                    );
                    return Ok(false);
                }
                if unstored > 0 {
                    eprintln!(
                        "{} {unstored} artifact(s) built but not stored",
                        "error:".bright_red(),
                    );
                    return Ok(false);
                }
                if left_alone > 0 {
                    eprintln!(
                        "{} {left_alone} file(s) not formatted; run this \
                         again to format them",
                        "error:".bright_red(),
                    );
                    return Ok(false);
                }
                return Ok(true);
            }
            vw_remote::DriverEvent::Fatal { message } => {
                return Err(CloudError::Transport(message));
            }
            vw_remote::DriverEvent::Diagnostic { mut message } => {
                if let Some(out) = diagnostics.as_mut() {
                    localize(&mut message, workspace);
                    // One line each, which is what an editor reading cargo's
                    // output expects. A failed write means the editor stopped
                    // listening; there is nobody left to tell.
                    if let Ok(line) = serde_json::to_string(&message) {
                        let _ = writeln!(out, "{line}");
                        let _ = out.flush();
                    }
                }
            }
            vw_remote::DriverEvent::Formatted {
                path,
                before,
                contents,
            } => {
                let driver = workspace.join(vw_remote::driver::DIRECTORY);
                match write_formatted(&driver, &path, &before, &contents) {
                    Ok(true) => println!(
                        "{:>12} {}/{path}",
                        "Formatted".bright_green().bold(),
                        vw_remote::driver::DIRECTORY,
                    ),
                    Ok(false) => {
                        left_alone += 1;
                        eprintln!(
                            "{} {}/{path} changed here since it was sent; \
                             left as it is",
                            "warning:".yellow(),
                            vw_remote::driver::DIRECTORY,
                        );
                    }
                    Err(problem) => {
                        left_alone += 1;
                        eprintln!("{} {problem}", "error:".bright_red());
                    }
                }
            }
        }
    }

    // The socket closed without a verdict, which is not a build result.
    Err(CloudError::Transport(format!(
        "the instance ended `cargo {command}` without reporting whether it \
         worked",
    )))
}

/// Run a cargo command over the driver on this machine.
///
/// What `--local` does: run the same cargo command in the same directory, so
/// somebody with an illumos toolchain to hand is not forced through a network.
/// Only from the driver's root, unlike on the instance — a developer running
/// locally is already in charge of which directory cargo starts from.
pub async fn run_locally(
    workspace: &Utf8Path,
    command: DriverCommand,
    release: bool,
    args: &[String],
) -> Result<bool, CloudError> {
    let driver = workspace.join(vw_remote::driver::DIRECTORY);
    if !driver.is_dir() {
        return Err(CloudError::NoDriver(driver));
    }

    let mut cargo = tokio::process::Command::new("cargo");
    match command {
        // Here there is no mirror to write back from: the tree is this one.
        DriverCommand::Fmt => {
            cargo.arg("fmt");
        }
        DriverCommand::FmtCheck => {
            cargo.args(["fmt", "--check"]);
        }
        _ => {
            cargo.arg(command.to_string());
            if release {
                cargo.arg("--release");
            }
        }
    }
    cargo.args(args);
    cargo.env(vw_lib::ANODIZED_ENV, vw_lib::anodized_dir(workspace));

    let status = cargo
        .current_dir(driver.as_std_path())
        .status()
        .await
        .map_err(|e| CloudError::Transport(format!("running cargo: {e}")))?;

    Ok(status.success())
}

/// Put this machine's workspace where the instance's diagnostic names it.
fn localize(value: &mut serde_json::Value, workspace: &Utf8Path) {
    match value {
        serde_json::Value::String(text) => {
            if text.contains(vw_remote::driver::WORKSPACE_MARKER) {
                *text = text.replace(
                    vw_remote::driver::WORKSPACE_MARKER,
                    workspace.as_str(),
                );
            }
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(|item| localize(item, workspace));
        }
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(|item| localize(item, workspace));
        }
        _ => {}
    }
}

/// Write a file formatting changed into this machine's driver.
///
/// `Ok(false)` when the file here is no longer what the instance formatted —
/// edited since it was sent — so writing would overwrite that edit with a
/// formatting of what came before it. `Err` names a path that could not be
/// written, or one that is not a plain path inside the driver; the instance
/// should never send one, and this machine does not take its word for that.
fn write_formatted(
    driver: &Utf8Path,
    path: &str,
    before: &str,
    contents: &str,
) -> Result<bool, String> {
    let relative = Utf8Path::new(path);
    if relative.as_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, camino::Utf8Component::Normal(_)))
    {
        return Err(format!(
            "the instance named `{path}`, which is not a \
                            path inside the driver; not writing it"
        ));
    }
    let local = driver.join(relative);

    let current =
        std::fs::read(&local).map_err(|e| format!("reading {local}: {e}"))?;
    if vw_sync::digest_bytes(&current).0 != before {
        return Ok(false);
    }

    std::fs::write(&local, contents)
        .map_err(|e| format!("writing {local}: {e}"))?;
    Ok(true)
}

/// Whether the driver includes anodizer's output, and so needs it to be
/// current before it is built.
///
/// Asked of the driver's sources and the crates outside `driver/` it reaches
/// by path, then of the design: a driver that includes the output from a
/// design with nothing tagged is left to fail on its own terms, which are
/// clearer than anything that could be said here.
pub fn needs_anodized(workspace: &Utf8Path) -> Result<bool, vw_lib::VwError> {
    let driver = workspace.join(vw_remote::driver::DIRECTORY);
    let mut crates = vec![driver.clone()];
    crates.extend(vw_lib::path_dependency_dirs(&driver));
    if !vw_lib::uses_anodized(&crates) {
        return Ok(false);
    }
    Ok(vw_lib::anodize_fingerprint(workspace)?.is_some())
}

#[cfg(test)]
mod test {
    use super::*;

    fn driver() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        std::fs::create_dir_all(root.join("module/src")).expect("mkdir");
        (dir, root)
    }

    #[test]
    fn a_file_as_it_was_sent_is_formatted() {
        let (_dir, root) = driver();
        let file = root.join("module/src/lib.rs");
        std::fs::write(&file, "fn  f( ){}\n").expect("write");
        let before = vw_sync::digest_bytes(b"fn  f( ){}\n").0;

        let written =
            write_formatted(&root, "module/src/lib.rs", &before, "fn f() {}\n");

        assert_eq!(written, Ok(true));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn f() {}\n");
    }

    #[test]
    fn a_file_edited_since_it_was_sent_is_left_alone() {
        // The developer kept typing while rustfmt ran. Their edit wins.
        let (_dir, root) = driver();
        let file = root.join("module/src/lib.rs");
        std::fs::write(&file, "fn  f( ){ edited }\n").expect("write");
        let before = vw_sync::digest_bytes(b"fn  f( ){}\n").0;

        let written =
            write_formatted(&root, "module/src/lib.rs", &before, "fn f() {}\n");

        assert_eq!(written, Ok(false));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn  f( ){ edited }\n",
        );
    }

    #[test]
    fn a_diagnostic_names_this_machines_files() {
        let mut message = serde_json::json!({
            "package_id": "path+file://{vw-workspace}/driver/rh#0.1.0",
            "message": { "spans": [{
                "file_name": "{vw-workspace}/driver/rh/src/main.rs"
            }]},
        });

        localize(&mut message, Utf8Path::new("/Users/me/ws"));

        assert_eq!(
            message["package_id"],
            "path+file:///Users/me/ws/driver/rh#0.1.0"
        );
        assert_eq!(
            message["message"]["spans"][0]["file_name"],
            "/Users/me/ws/driver/rh/src/main.rs"
        );
    }

    #[test]
    fn a_path_outside_the_driver_is_refused() {
        let (_dir, root) = driver();
        for path in ["../vw.toml", "/etc/passwd", "module/../../x.rs", ""] {
            assert!(
                write_formatted(&root, path, "", "").is_err(),
                "{path} should have been refused",
            );
        }
    }
}
