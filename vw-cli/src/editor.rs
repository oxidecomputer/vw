// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw editor` — making a workspace legible to the editor on this machine.
//!
//! An editor's Rust support runs here, against this checkout, and a vw
//! workspace's Rust depends on things that are only ever made in the cloud:
//! a mixed-signal bench's crate is generated from its `mist.toml`, the driver
//! includes anodizer output generated from the design, and the driver's real
//! diagnostics come from compiling it for illumos. Without them rust-analyzer
//! cannot load the bench workspace at all and cannot resolve anything the
//! anodizer generated.
//!
//! Two commands close the gap, and neither feeds anything back into a build.
//! `prepare` puts the generated inputs where rust-analyzer can see them, and
//! only where synchronization never looks: scaffolds only for a bench whose
//! `.gitignore` already covers them, and anodizer output under `target/`.
//! Every instance goes on generating or fetching its own. `check` is what rust-analyzer runs on save: the
//! driver's `cargo check` on the helios instance, reported as cargo's JSON
//! naming this machine's files.

use std::io::Write;

use camino::{Utf8Path, Utf8PathBuf};
use colored::*;
use vw_api_types_versions::latest::{DriverCommand, TargetKind};

use crate::cloud;

type Error = Box<dyn std::error::Error>;

/// What the editor configuration that runs [`check`] looks like, for Helix.
///
/// Shown rather than written: editor configuration belongs to the developer,
/// and is often checked in by a project that has its own opinions about it.
const HELIX_CHECK: &str = "\
[language-server.rust-analyzer.config.check]
overrideCommand = [\"vw\", \"editor\", \"check\"]";

/// The cargo configuration that tells every cargo invocation — the editor's
/// included — where anodizer's output is.
const CARGO_ENV: &str = "\
[env]
VW_ANODIZED_DIR = { value = \"target/anodizer/gen\", relative = true }";

/// What [`prepare`] did, for whoever ran it — a person, or the vw analyzer
/// running it on the editor's behalf and reading this as JSON.
#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    /// Mixed-signal benches whose generated files are in place.
    pub scaffolds_written: Vec<String>,
    /// Benches left alone, because generating here would be synchronized.
    pub scaffolds_skipped: Vec<String>,
    /// How anodizer's output came to be what it is.
    pub anodized: Anodized,
    /// What still keeps rust-analyzer from reading everything, one line
    /// each. The detail went to stderr.
    pub problems: Vec<String>,
}

impl Report {
    /// Whether everything the editor needs is in place.
    pub fn ready(&self) -> bool {
        self.anodized != Anodized::Failed && self.problems.is_empty()
    }
}

/// How anodizer's output came to be what it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Anodized {
    /// Nothing in the design is tagged, so there is none.
    #[default]
    Untagged,
    /// Already current; nothing changed.
    Current,
    /// Fetched from the environment — new to this machine, so a running
    /// rust-analyzer has not seen it.
    Fetched,
    /// Generated on this machine, with `--local`. New, as above.
    Generated,
    /// The anodize phase failed, and said why.
    Failed,
}

/// The note pointing at rust-analyzer's check command, for a person.
pub fn print_check_hint() {
    println!(
        "\n{} to have rust-analyzer check the driver on the helios \
         instance, set its check command. For Helix, in \
         .helix/languages.toml:\n\n{HELIX_CHECK}\n",
        "note:".bright_cyan(),
    );
}

/// Make generated inputs available to the editor.
pub async fn prepare(
    cwd: &Utf8Path,
    local: bool,
    named: Option<&str>,
    insecure: bool,
) -> Result<Report, Error> {
    let workspace = vw_lib::find_workspace_dir(cwd.as_std_path()).ok_or(
        "no vw workspace here; run this from one, or from a directory inside it",
    )?;

    // One at a time per workspace. An editor may run a language server per
    // project root — one for the HDL, one rooted at the driver — and each
    // prepares the workspace it is in; two at once would fetch into the same
    // place and could both start the anodize phase. The second waits, then
    // finds everything current.
    let _held = lock(&workspace).await?;

    // The anodizer fingerprint covers dependency sources, so they have to be
    // here before it can be worked out.
    crate::ensure_workspace_deps(cwd).await;

    // Generated from files in this checkout and nothing else, so made here
    // the same way the instance makes them — but only the ones a bench's
    // `.gitignore` covers, so nothing made here is ever synchronized.
    let scaffolds = vw_lib::ensure_editor_scaffolds(&workspace)?;
    if !scaffolds.written.is_empty() {
        println!(
            "{} bench scaffolds generated: {}",
            "\u{2713}".bright_green(),
            scaffolds.written.join(", "),
        );
    }
    let mut report = Report {
        scaffolds_written: scaffolds.written.clone(),
        scaffolds_skipped: scaffolds.skipped.clone(),
        ..Report::default()
    };
    for bench in &scaffolds.skipped {
        report.problems.push(format!(
            "bench/{bench}: its .gitignore does not cover the files generated \
             from mist.toml, so they were not generated"
        ));
        eprintln!(
            "{} bench/{bench}: its .gitignore does not list every file \
             generated from mist.toml, so they were not generated here — \
             synchronization would carry them to the cloud. `vw bench run` \
             writes the .gitignore; commit it.",
            "warning:".yellow(),
        );
    }

    report.anodized = anodized(&workspace, local, named, insecure).await?;
    report.problems.extend(advise(&workspace));

    Ok(report)
}

/// Wait for, then hold, the right to prepare `workspace`. Released when the
/// returned file is dropped, or when this process ends however it ends.
async fn lock(workspace: &Utf8Path) -> Result<std::fs::File, Error> {
    let dir = workspace.join("target");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".vw-editor-prepare.lock"))?;
    let file = tokio::task::spawn_blocking(move || {
        use std::os::fd::AsRawFd;
        // SAFETY: `flock` on a descriptor this function owns.
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if held == 0 {
            Ok(file)
        } else {
            Err(std::io::Error::last_os_error())
        }
    })
    .await??;
    Ok(file)
}

/// Make anodizer's output here current, if the design has any.
async fn anodized(
    workspace: &Utf8Path,
    local: bool,
    named: Option<&str>,
    insecure: bool,
) -> Result<Anodized, Error> {
    let expected = match vw_lib::anodized_status(workspace)? {
        vw_lib::Anodized::Untagged => {
            println!(
                "{} nothing in the design is tagged `serialize_rust`; no \
                 anodizer output to fetch",
                "\u{2713}".bright_green(),
            );
            return Ok(Anodized::Untagged);
        }
        vw_lib::Anodized::Current { fingerprint, .. } => {
            println!(
                "{} anodizer output current ({fingerprint:016x})",
                "\u{2713}".bright_green(),
            );
            return Ok(Anodized::Current);
        }
        vw_lib::Anodized::Stale { expected, .. } => expected,
    };

    if local {
        vw_lib::ensure_anodized(workspace, vw_lib::VhdlStandard::Vhdl2019)
            .await?;
        println!(
            "{} anodizer output generated ({expected:016x})",
            "\u{2713}".bright_green(),
        );
        return Ok(Anodized::Generated);
    }

    let session = cloud::Session::from_env(insecure)?;
    let target = cloud::resolve_target(&session, named).await?;

    // What the environment already published, if it matches this checkout.
    if fetch_anodized(&session, &target, workspace, expected).await? {
        return Ok(Anodized::Fetched);
    }

    // It does not, so anodize — the same explicit phase `vw anodize` runs,
    // publishing to the store — and fetch what that published.
    cloud::sync_for_build(&session, &target, Some(TargetKind::Vivado)).await?;
    let options = crate::anodize::Options {
        bench: None,
        standard: vw_lib::VhdlStandard::Vhdl2019,
        force: false,
    };
    if !crate::anodize::run(&session, &target, &options).await? {
        return Ok(Anodized::Failed);
    }
    if fetch_anodized(&session, &target, workspace, expected).await? {
        return Ok(Anodized::Fetched);
    }

    Err(format!(
        "the environment's anodizer output does not match this checkout \
         (expected {expected:016x}); has something changed since the sync?"
    )
    .into())
}

/// Fetch the anodizer output the environment published, and keep it if it
/// was made from exactly this checkout's sources.
///
/// Checked rather than trusted, for the reason the helios instance checks
/// it: the store holds whatever was anodized last, which may have been
/// another checkout's design.
async fn fetch_anodized(
    session: &cloud::Session,
    target: &cloud::Target,
    workspace: &Utf8Path,
    expected: u64,
) -> Result<bool, Error> {
    let name = format!("anodizer/gen/{}", vw_lib::ANODIZED_FILE);

    let available = vw_api_client::retrying(|| {
        session
            .client
            .get_artifacts(&target.environment, &target.workspace)
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let Some(artifact) = available
        .iter()
        .find(|a| a.kind == TargetKind::Vivado && a.name == name)
    else {
        return Ok(false);
    };

    // Into a directory of its own first, so a mismatched file never sits
    // where the editor would read it.
    let staging = workspace.join("target/anodizer/.fetched");
    let _ = std::fs::remove_dir_all(&staging);
    cloud::download(session, target, artifact, &staging).await?;
    let fetched = staging.join(&name);

    let found = std::fs::read_to_string(&fetched)
        .ok()
        .and_then(|text| vw_lib::anodized_fingerprint_of(&text));
    if found != Some(expected) {
        let _ = std::fs::remove_dir_all(&staging);
        return Ok(false);
    }

    let path = vw_lib::anodized_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&fetched, &path)?;
    let _ = std::fs::remove_dir_all(&staging);

    println!(
        "{} anodizer output fetched ({expected:016x})",
        "\u{2713}".bright_green(),
    );
    Ok(true)
}

/// Say what, in this workspace's own files, keeps rust-analyzer from reading
/// anodizer's output: in detail on stderr, and one line each in the result.
///
/// Advice rather than edits: these are the project's files, and the fix is a
/// line or two the developer should see.
fn advise(workspace: &Utf8Path) -> Vec<String> {
    let direct = direct_includes(workspace);
    let mut problems = Vec::new();

    // rust-analyzer reads only files inside a crate, or in the `OUT_DIR` of
    // its build script. Anodizer output is under `target/` at the workspace
    // root, which is neither, so an `include!` naming it directly cannot be
    // resolved there however the variable is set.
    if !direct.is_empty() {
        problems.push(format!(
            "{} includes anodizer output straight from {}, which \
             rust-analyzer cannot read; include it through OUT_DIR",
            direct
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            vw_lib::ANODIZED_ENV,
        ));
        eprintln!(
            "{} rust-analyzer cannot read anodizer output included straight \
             from `{}`: it only reads files inside a crate or in its build \
             script's OUT_DIR. Include it through OUT_DIR instead:",
            "warning:".yellow(),
            vw_lib::ANODIZED_ENV,
        );
        for file in &direct {
            eprintln!("    {file}");
        }
        eprintln!(
            "\n  in the crate's build.rs:\n\n{}\n\n  and in place of the \
             include:\n\n    include!(concat!(env!(\"OUT_DIR\"), \"/{}\"));\n",
            build_rs_recipe(),
            vw_lib::ANODIZED_FILE,
        );
    }

    // Everything that compiles the driver or a bench outside vw — the
    // editor, plain cargo — needs the variable from cargo's own
    // configuration. vw sets it itself, which overrides this.
    let uses = !direct.is_empty() || uses_anodized_anywhere(workspace);
    if uses {
        let missing: Vec<Utf8PathBuf> = [
            workspace.join(vw_remote::driver::DIRECTORY),
            workspace.join("bench"),
        ]
        .into_iter()
        .filter(|dir| dir.is_dir() && !cargo_env_names(dir))
        .collect();
        if !missing.is_empty() {
            problems.push(format!(
                "cargo configuration does not set {} for {}; add an [env] \
                 entry to .cargo/config.toml",
                vw_lib::ANODIZED_ENV,
                missing
                    .iter()
                    .map(|dir| {
                        dir.strip_prefix(workspace).unwrap_or(dir).to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(" and "),
            ));
            eprintln!(
                "{} cargo does not know where anodizer output is when run \
                 from {}. Add to {}/.cargo/config.toml:\n\n{CARGO_ENV}\n",
                "warning:".yellow(),
                missing
                    .iter()
                    .map(|dir| {
                        dir.strip_prefix(workspace).unwrap_or(dir).to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(" and "),
                workspace,
            );
        }
    }

    if problems.is_empty() && uses {
        println!(
            "{} rust-analyzer can read anodizer output",
            "\u{2713}".bright_green(),
        );
    }
    problems
}

/// The build script that brings anodizer's output into a crate's `OUT_DIR`.
fn build_rs_recipe() -> String {
    format!(
        "    fn main() {{\n        \
         println!(\"cargo::rerun-if-env-changed={env}\");\n        \
         let dir = std::env::var(\"{env}\").expect(\"{env}\");\n        \
         let from = std::path::Path::new(&dir).join(\"{file}\");\n        \
         println!(\"cargo::rerun-if-changed={{}}\", from.display());\n        \
         let out = std::env::var(\"OUT_DIR\").unwrap();\n        \
         std::fs::copy(&from, std::path::Path::new(&out).join(\"{file}\"))\
         .unwrap();\n    }}",
        env = vw_lib::ANODIZED_ENV,
        file = vw_lib::ANODIZED_FILE,
    )
}

/// Every Rust file, other than a build script, that names anodizer's
/// directory in an `env!` — which can only be there to include from it.
fn direct_includes(workspace: &Utf8Path) -> Vec<Utf8PathBuf> {
    let needle = format!("env!(\"{}\")", vw_lib::ANODIZED_ENV);
    let mut found = Vec::new();
    rust_files(workspace, &mut |path, text| {
        if path.file_name() != Some("build.rs") && text.contains(&needle) {
            found.push(path.strip_prefix(workspace).unwrap_or(path).to_owned());
        }
    });
    found.sort();
    found
}

/// Whether anything in the workspace uses anodizer's output at all.
fn uses_anodized_anywhere(workspace: &Utf8Path) -> bool {
    let mut uses = false;
    rust_files(workspace, &mut |_, text| {
        uses |= text.contains(vw_lib::ANODIZED_ENV);
    });
    uses
}

/// Visit every Rust source in the workspace, skipping build output and
/// hidden directories.
fn rust_files(dir: &Utf8Path, visit: &mut dyn FnMut(&Utf8Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        let name = path.file_name().unwrap_or_default();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rust_files(&path, visit);
            }
        } else if path.extension() == Some("rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                visit(&path, &text);
            }
        }
    }
}

/// Whether cargo, run from `dir`, sets anodizer's variable from its own
/// configuration.
///
/// Cargo reads `.cargo/config.toml` (or `.cargo/config`) from the directory
/// it runs in and every ancestor, so any of them will do.
fn cargo_env_names(dir: &Utf8Path) -> bool {
    dir.ancestors().any(|ancestor| {
        ["config.toml", "config"].iter().any(|name| {
            let Ok(text) =
                std::fs::read_to_string(ancestor.join(".cargo").join(name))
            else {
                return false;
            };
            text.parse::<toml::Table>().is_ok_and(|config| {
                config
                    .get("env")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|env| env.contains_key(vw_lib::ANODIZED_ENV))
            })
        })
    })
}

/// Take stdout for cargo's JSON, sending everything else written to it to
/// stderr.
///
/// An editor reads a check command's stdout as cargo messages, one JSON
/// object per line. Everything vw says on the way there — the sync, the
/// anodize phase, cargo's progress — is for a person, and is printed from
/// all over. Moving the descriptor once is the one way to be sure none of it
/// lands where the editor is parsing.
#[cfg(unix)]
pub fn claim_stdout() -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;

    std::io::stdout().flush()?;
    // SAFETY: plain descriptor duplication; the result is checked, and the
    // new descriptor is owned by the `File` returned and nothing else.
    unsafe {
        let original = libc::dup(libc::STDOUT_FILENO);
        if original < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            let error = std::io::Error::last_os_error();
            libc::close(original);
            return Err(error);
        }
        Ok(std::fs::File::from_raw_fd(original))
    }
}

/// Check the driver on the helios instance, for an editor.
///
/// Run with the editor's workspace as the current directory. Only the driver
/// is checked in the cloud — it is the Rust that cannot be compiled here — so
/// run from anywhere that is not part of it, this reports nothing.
pub async fn check(
    cwd: &Utf8Path,
    named: Option<&str>,
    insecure: bool,
    clippy: bool,
    args: &[String],
    out: &mut dyn Write,
) -> Result<bool, Error> {
    let workspace = vw_lib::find_workspace_dir(cwd.as_std_path()).ok_or(
        "no vw workspace here; run this from one, or from a directory inside it",
    )?;

    if !is_driver(&workspace, cwd) {
        eprintln!(
            "{} {cwd} is not part of the driver; nothing to check in the cloud",
            "note:".bright_cyan(),
        );
        return Ok(true);
    }

    let command = if clippy {
        DriverCommand::Clippy
    } else {
        DriverCommand::Check
    };
    crate::driver_cargo(
        cwd,
        command,
        false,
        named,
        insecure,
        false,
        args,
        Some(out),
    )
    .await
}

/// Whether `dir` is the driver, inside it, or a crate the driver depends on
/// by path — every place an editor might be rooted while working on driver
/// code.
fn is_driver(workspace: &Utf8Path, dir: &Utf8Path) -> bool {
    let canonical = |path: &Utf8Path| {
        std::fs::canonicalize(path)
            .ok()
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
            .unwrap_or_else(|| path.to_owned())
    };
    let driver = workspace.join(vw_remote::driver::DIRECTORY);
    if !driver.is_dir() {
        return false;
    }
    let dir = canonical(dir);
    std::iter::once(driver.clone())
        .chain(vw_lib::path_dependency_dirs(&driver))
        .any(|root| dir.starts_with(canonical(&root)))
}

#[cfg(test)]
mod test {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        std::fs::write(root.join("vw.toml"), "[workspace]\nname = \"w\"\n")
            .expect("write");
        (dir, root)
    }

    fn write(path: &Utf8Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn an_include_straight_from_the_variable_is_found() {
        let (_dir, ws) = workspace();
        write(
            &ws.join("common/u/src/lib.rs"),
            "include!(concat!(env!(\"VW_ANODIZED_DIR\"), \"/hdl.rs\"));",
        );
        // A build script reading it is the fix, not the problem.
        write(
            &ws.join("common/u/build.rs"),
            "let d = env!(\"VW_ANODIZED_DIR\");",
        );
        // Build output is not the project's.
        write(&ws.join("target/x/src/lib.rs"), "env!(\"VW_ANODIZED_DIR\")");

        assert_eq!(
            direct_includes(&ws),
            [Utf8PathBuf::from("common/u/src/lib.rs")]
        );
    }

    #[test]
    fn cargo_configuration_anywhere_above_counts() {
        let (_dir, ws) = workspace();
        std::fs::create_dir_all(ws.join("driver")).expect("mkdir");
        assert!(!cargo_env_names(&ws.join("driver")));

        write(&ws.join(".cargo/config.toml"), CARGO_ENV);
        assert!(cargo_env_names(&ws.join("driver")));
    }

    #[test]
    fn the_driver_and_what_it_depends_on_by_path_are_the_driver() {
        let (_dir, ws) = workspace();
        write(
            &ws.join("driver/Cargo.toml"),
            "[workspace]\nmembers = [\"rh\"]\n",
        );
        write(
            &ws.join("driver/rh/Cargo.toml"),
            "[package]\nname = \"rh\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\nu = { path = \"../../common/u\" }\n",
        );
        write(
            &ws.join("common/u/Cargo.toml"),
            "[package]\nname = \"u\"\nversion = \"0.1.0\"\n",
        );
        std::fs::create_dir_all(ws.join("bench")).expect("mkdir");

        assert!(is_driver(&ws, &ws.join("driver")));
        assert!(is_driver(&ws, &ws.join("driver/rh")));
        assert!(is_driver(&ws, &ws.join("common/u")));
        assert!(!is_driver(&ws, &ws.join("bench")));
    }
}
