// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Mixed-signal simulation support.
//!
//! This module provides scaffolding generation and co-simulation
//! orchestration for mixed-signal (VHDL + Xyce SPICE) testbenches.

pub mod bridge;
#[cfg(feature = "plot")]
pub mod plot;

use std::path::{Path, PathBuf};
use std::{fs, io};

use camino::Utf8Path;

use crate::nvc_helpers::{run_nvc_analysis, run_nvc_cosim, run_nvc_elab};
use crate::{
    analyze_ext_libraries, find_referenced_files, load_existing_vhdl_ls_config,
    sort_files_by_dependencies, FileCache, MistConfig, RecordProcessor,
    ToolsConfig, VhdlStandard, VwError,
};

/// Information about an available mixed-signal test.
pub struct MistTestInfo {
    pub name: String,
    pub entity: String,
    pub netlist: String,
}

/// Scan bench directory for subdirectories containing `mist.toml`.
pub fn find_mist_configs(
    bench_dir: &Utf8Path,
) -> crate::Result<Vec<(String, MistConfig)>> {
    let mut configs = Vec::new();

    let entries = match fs::read_dir(bench_dir.as_std_path()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(configs),
        Err(e) => {
            return Err(VwError::FileSystem {
                message: format!(
                    "Failed to read bench directory {}: {e}",
                    bench_dir
                ),
            })
        }
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let mist_toml = path.join("mist.toml");
            if mist_toml.exists() {
                let content = fs::read_to_string(&mist_toml).map_err(|e| {
                    VwError::Config {
                        message: format!(
                            "Failed to read {}: {e}",
                            mist_toml.display()
                        ),
                    }
                })?;
                let config: MistConfig =
                    toml::from_str(&content).map_err(|e| VwError::Config {
                        message: format!(
                            "Failed to parse {}: {e}",
                            mist_toml.display()
                        ),
                    })?;
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                configs.push((name, config));
            }
        }
    }

    configs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(configs)
}

/// Generate or regenerate mixed-signal scaffolding from `mist.toml`.
///
/// This creates all boilerplate files in the bench directory:
/// - `Cargo.toml`, `build.rs`, `xyce_cinterface.cpp` (always regenerated)
/// - `src/xyce.rs`, `src/generated.rs` (always regenerated)
/// - `src/lib.rs` (only created if missing — user owns this file)
pub fn scaffold(
    bench_dir: &Utf8Path,
    mist_config: &MistConfig,
    tools: &Option<ToolsConfig>,
) -> crate::Result<()> {
    bridge::generate_scaffold(bench_dir.as_std_path(), mist_config, tools)
}

/// Run a mixed-signal co-simulation test.
pub async fn run_analog_test(
    workspace_dir: &Utf8Path,
    name: &str,
    bench_dir: &Utf8Path,
    mist_config: &MistConfig,
    _tools: &Option<ToolsConfig>,
    vhdl_std: VhdlStandard,
) -> crate::Result<()> {
    let vhdl_ls_config = load_existing_vhdl_ls_config(workspace_dir)?;
    let mut processor = RecordProcessor::new(vhdl_std);
    let mut cache = FileCache::new();

    fs::create_dir_all(crate::BUILD_DIR)?;

    // Analyze external libraries
    analyze_ext_libraries(
        &vhdl_ls_config,
        &mut processor,
        vhdl_std,
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
    let entity_name = &mist_config.entity;
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
    run_nvc_analysis(vhdl_std, crate::BUILD_DIR, "work", &files, false).await?;
    run_nvc_elab(vhdl_std, crate::BUILD_DIR, "work", entity_name, false)
        .await?;

    // Build the bridge crate
    let bridge_lib =
        build_bridge_library(bench_dir.as_std_path(), name).await?;
    let bridge_lib_str = bridge_lib.to_string_lossy().to_string();

    // Run co-simulation
    run_nvc_cosim(
        vhdl_std,
        crate::BUILD_DIR,
        "work",
        entity_name,
        &bridge_lib_str,
        false,
    )
    .await?;

    // Collect output
    let output_dir = bench_dir.as_std_path().join("output");
    fs::create_dir_all(&output_dir)?;

    let netlist_path = bench_dir.as_std_path().join(&mist_config.netlist);
    let prn_name = netlist_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
        + ".prn";
    let prn_source = netlist_path.with_extension(
        netlist_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            + ".prn",
    );

    if prn_source.exists() {
        let prn_dest = output_dir.join(&prn_name);
        fs::copy(&prn_source, &prn_dest)?;
    }

    // Auto-generate plots
    #[cfg(feature = "plot")]
    {
        let prn_path = output_dir.join(&prn_name);
        if prn_path.exists() {
            if let Err(e) =
                plot::generate_plots(&netlist_path, &prn_path, &output_dir)
            {
                eprintln!("Warning: plot generation failed: {e}");
            }
        }
    }

    Ok(())
}

/// Find a VHDL entity source file by searching through defaultlib files.
fn find_entity_file(
    workspace_dir: &Path,
    defaultlib_files: &[PathBuf],
    entity_name: &str,
    cache: &mut FileCache,
) -> crate::Result<PathBuf> {
    for file_path in defaultlib_files {
        let absolute_path = if file_path.is_relative() {
            workspace_dir.join(file_path)
        } else {
            file_path.clone()
        };
        if !absolute_path.exists() {
            continue;
        }
        let entities = cache.get_entities(&absolute_path)?.clone();
        for entity in &entities {
            if entity.eq_ignore_ascii_case(entity_name) {
                return Ok(absolute_path);
            }
        }
    }

    Err(VwError::Simulation {
        message: format!(
            "Entity '{}' not found in defaultlib files",
            entity_name,
        ),
    })
}

/// Build the bridge Rust crate in a bench directory.
async fn build_bridge_library(
    bench_dir: &Path,
    name: &str,
) -> crate::Result<PathBuf> {
    let bench_dir_owned = bench_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("cargo")
            .arg("build")
            .arg("--release")
            .current_dir(&bench_dir_owned)
            .output()
            .map_err(|e| VwError::Simulation {
                message: format!("Failed to run cargo build: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VwError::Simulation {
                message: format!("cargo build failed:\n{stderr}"),
            });
        }

        Ok::<(), VwError>(())
    })
    .await
    .map_err(|e| VwError::Simulation {
        message: format!("Build task failed: {e}"),
    })??;

    // Find the built .so
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let crate_name = name.replace('-', "_");
    let lib_name = format!("lib{crate_name}.{ext}");
    let mut bench_dir = bench_dir.to_path_buf();
    // assume this is a workspace
    bench_dir.pop();
    let lib_path = bench_dir.join("target").join("release").join(&lib_name);

    if !lib_path.exists() {
        return Err(VwError::Simulation {
            message: format!(
                "Built library not found at: {}",
                lib_path.display()
            ),
        });
    }

    Ok(lib_path)
}
