// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Running a testbench that drives a design entity directly.
//!
//! The design entity *is* the top level. nvc elaborates it as it stands and
//! loads a Rust `cdylib` alongside; that library reaches the entity's ports
//! over VHPI and is the only thing driving them — clock included. There is no
//! VHDL harness in between, and nothing to keep in step with the design.
//!
//! `bench/<name>/cosim.toml` is what says which entity, the same way
//! `bench/<name>/mist.toml` does for a mixed-signal bench. Mixed-signal is
//! the same arrangement with a Xyce circuit attached, so the two share their
//! elaboration.
//!
//! **One thing the driver must do.** With no VHDL harness, nothing in the
//! design schedules an event of its own. If the driver's first `await` is not
//! a timed callback, the simulator finds no work at time zero and ends the
//! run before the test starts. The scaffolded driver awaits a `Timer` first
//! for exactly this reason.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::nvc_helpers::{
    run_nvc_analysis, run_nvc_driven, run_nvc_elab_with_generics,
};
use crate::{
    analyze_ext_libraries, find_referenced_files, render_vhdl_ls_config,
    sort_files_by_dependencies, FileCache, RecordProcessor, Result,
    VhdlStandard, VwError,
};

/// What `bench/<name>/cosim.toml` says.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CosimConfig {
    /// The design entity to elaborate and drive.
    pub entity: String,
    /// Clock frequency in Hz, for the period the generated driver ticks at.
    #[serde(default)]
    pub clock: Option<f64>,
    /// Instantiation labels inside `entity` that the driver implements rather
    /// than observes.
    ///
    /// This is the list, not the command line: `vw cosim init` adds to it and
    /// regeneration reads it, so a bench grows a component at a time without
    /// the flags having to be repeated. Removing one means deleting it here.
    #[serde(default, rename = "rust-components")]
    pub rust_components: Vec<String>,
    /// Elaboration-time generic overrides.
    ///
    /// The entity is the top level, so its generics take their declared
    /// defaults unless something says otherwise — and there is no wrapper to
    /// be that something. A generic declared without a default has to appear
    /// here or elaboration fails.
    #[serde(default)]
    pub generics: BTreeMap<String, String>,
}

/// Every directory under `bench/` holding a `cosim.toml`.
///
/// Top level only, matching how mixed-signal benches are found: a bench is a
/// directory in `bench/`, and one nested inside another bench's crate would
/// be a different thing entirely.
pub fn find_cosim_configs(
    bench_dir: &Utf8Path,
) -> Result<Vec<(String, CosimConfig)>> {
    let entries = match fs::read_dir(bench_dir.as_std_path()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(VwError::FileSystem {
                message: format!("reading {bench_dir}: {e}"),
            })
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let config_path = path.join("cosim.toml");
        if !config_path.exists() {
            continue;
        }
        let config = read_config(&config_path)?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        found.push((name, config));
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Read one `cosim.toml`.
pub fn read_config(path: &Path) -> Result<CosimConfig> {
    let text = fs::read_to_string(path).map_err(|e| VwError::Config {
        message: format!("reading {}: {e}", path.display()),
    })?;
    toml::from_str(&text).map_err(|e| VwError::Config {
        message: format!("parsing {}: {e}", path.display()),
    })
}

/// Compile the design and elaborate `entity_name` as the top level.
///
/// Shared with the mixed-signal path, which is the same arrangement with a
/// Xyce circuit attached. Only the sources `entity_name` actually references
/// are compiled — the same rule a pure-VHDL bench follows, so an unrelated
/// broken entity elsewhere in the design does not fail this bench.
pub async fn elaborate_entity_as_top(
    workspace_dir: &Utf8Path,
    entity_name: &str,
    vhdl_std: VhdlStandard,
    build_dir: &str,
    generics: &BTreeMap<String, String>,
) -> Result<()> {
    let vhdl_ls_config = render_vhdl_ls_config(workspace_dir, None, false)?;
    let mut processor = RecordProcessor::new(vhdl_std);
    let mut cache = FileCache::new();

    fs::create_dir_all(build_dir)?;

    // Analyze external libraries
    analyze_ext_libraries(
        &vhdl_ls_config,
        &mut processor,
        vhdl_std,
        build_dir,
        &mut cache,
    )
    .await?;

    // Get all defaultlib files
    let defaultlib_files = vhdl_ls_config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    // Find the entity source file in defaultlib
    let entity_file = find_entity_file(
        workspace_dir.as_std_path(),
        &defaultlib_files,
        entity_name,
        &mut cache,
    )?;

    // Find referenced files
    let mut referenced_files =
        find_referenced_files(&entity_file, &defaultlib_files, &mut cache)?;

    // Topological sort
    sort_files_by_dependencies(
        &mut processor,
        &mut referenced_files,
        &mut cache,
    )?;

    let mut files: Vec<String> = referenced_files
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    files.push(entity_file.to_string_lossy().to_string());

    // Compile VHDL
    run_nvc_analysis(vhdl_std, build_dir, "work", &files, false).await?;

    let generics: Vec<(String, String)> = generics
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    run_nvc_elab_with_generics(
        vhdl_std,
        build_dir,
        "work",
        entity_name,
        &generics,
        false,
    )
    .await?;

    Ok(())
}

/// Find a VHDL entity source file by searching through defaultlib files.
pub fn find_entity_file(
    workspace_dir: &Path,
    defaultlib_files: &[PathBuf],
    entity_name: &str,
    cache: &mut FileCache,
) -> Result<PathBuf> {
    for file_path in defaultlib_files {
        let absolute_path = if file_path.is_relative() {
            workspace_dir.join(file_path)
        } else {
            file_path.clone()
        };
        if !absolute_path.exists() {
            continue;
        }
        for entity in cache.get_entities(&absolute_path)?.clone() {
            if entity.eq_ignore_ascii_case(entity_name) {
                return Ok(absolute_path);
            }
        }
    }

    Err(VwError::Simulation {
        message: format!(
            "entity '{entity_name}' is not in the workspace's design sources"
        ),
    })
}

/// Run one direct-drive cosim testbench.
pub async fn run(
    workspace_dir: &Utf8Path,
    name: &str,
    bench_dir: &Utf8Path,
    config: &CosimConfig,
    vhdl_std: VhdlStandard,
    build_dir: &str,
    runtime_flags: &[String],
) -> Result<()> {
    // The driver's `build.rs` is generated and goes missing the same ways
    // any generated file does; put it back before cargo needs it.
    crate::bench_init::cosim::heal(bench_dir)?;

    elaborate_entity_as_top(
        workspace_dir,
        &config.entity,
        vhdl_std,
        build_dir,
        &config.generics,
    )
    .await?;

    let driver =
        build_driver_library(&workspace_dir.join("bench"), bench_dir).await?;

    let output_dir = crate::bench_output_dir(workspace_dir, name);
    fs::create_dir_all(&output_dir)?;

    run_nvc_driven(
        vhdl_std,
        build_dir,
        "work",
        &config.entity,
        driver.as_str(),
        output_dir.as_str(),
        runtime_flags,
        false,
    )
    .await?;

    Ok(())
}

/// Build a driver crate and return the shared library it produced.
async fn build_driver_library(
    bench_root: &Utf8Path,
    crate_dir: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let manifest = crate_dir.join("Cargo.toml");
    let text = fs::read_to_string(manifest.as_std_path()).map_err(|e| {
        VwError::Testbench {
            message: format!("reading {manifest}: {e}"),
        }
    })?;
    let package: toml::Value =
        toml::from_str(&text).map_err(|e| VwError::Testbench {
            message: format!("parsing {manifest}: {e}"),
        })?;
    let crate_name = package
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| VwError::Testbench {
            message: format!("{manifest} declares no package name"),
        })?
        .to_string();

    let dir = crate_dir.to_owned();
    let package_name = crate_name.clone();
    tokio::task::spawn_blocking(move || {
        let output = crate::cargo_command()
            .args(["build", "-p", &package_name])
            .current_dir(dir.as_std_path())
            .output()
            .map_err(|e| VwError::Testbench {
                message: format!("running cargo build: {e}"),
            })?;
        if !output.status.success() {
            return Err(VwError::Testbench {
                message: format!(
                    "cargo build failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            });
        }
        Ok::<(), VwError>(())
    })
    .await
    .map_err(|e| VwError::Testbench {
        message: format!("cargo build task failed: {e}"),
    })??;

    // Every bench crate is a member of the one cargo workspace under
    // `bench/`, so its artifacts land in that workspace's target directory
    // however deeply the crate itself is filed.
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let library = bench_root
        .join("target")
        .join("debug")
        .join(format!("lib{}.{extension}", crate_name.replace('-', "_"),));
    if !library.exists() {
        return Err(VwError::Testbench {
            message: format!(
                "{crate_name} built, but {library} is not there — is the \
                 crate `crate-type = [\"cdylib\"]`?"
            ),
        });
    }
    Ok(library)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minimum a `cosim.toml` has to say, and the one thing it may add.
    #[test]
    fn a_config_needs_only_an_entity() {
        let config: CosimConfig =
            toml::from_str("entity = \"flit_fifo\"\n").unwrap();
        assert_eq!(config.entity, "flit_fifo");
        assert!(config.generics.is_empty());

        let config: CosimConfig = toml::from_str(
            "entity = \"flit_fifo\"\n\n[generics]\nDATA_W = \"32\"\n",
        )
        .unwrap();
        assert_eq!(config.generics["DATA_W"], "32");
    }

    /// Discovery is by directory, so a bench's name is its directory's — the
    /// same rule mixed-signal benches follow.
    #[test]
    fn benches_are_found_by_their_directory() {
        let guard = tempfile::tempdir().unwrap();
        let bench =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();

        for name in ["zed", "alpha"] {
            fs::create_dir_all(bench.join(name).as_std_path()).unwrap();
            fs::write(
                bench.join(name).join("cosim.toml"),
                format!("entity = \"{name}_top\"\n"),
            )
            .unwrap();
        }
        // Not a bench: no cosim.toml.
        fs::create_dir_all(bench.join("helpers").as_std_path()).unwrap();

        let found = find_cosim_configs(&bench).unwrap();
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["alpha", "zed"]);
        assert_eq!(found[0].1.entity, "alpha_top");
    }

    /// A workspace with no `bench/` at all is not an error — it just has no
    /// benches.
    #[test]
    fn a_missing_bench_directory_finds_nothing() {
        let guard = tempfile::tempdir().unwrap();
        let missing =
            Utf8PathBuf::from_path_buf(guard.path().join("nope")).unwrap();
        assert!(find_cosim_configs(&missing).unwrap().is_empty());
    }
}
