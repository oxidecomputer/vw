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

use crate::nvc_helpers::run_nvc_cosim;
use crate::{MistConfig, VhdlStandard, VwError};

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
) -> crate::Result<()> {
    bridge::generate_scaffold(bench_dir.as_std_path(), mist_config)
}

/// Run a mixed-signal co-simulation test.
pub async fn run_analog_test(
    workspace_dir: &Utf8Path,
    name: &str,
    bench_dir: &Utf8Path,
    mist_config: &MistConfig,
    vhdl_std: VhdlStandard,
    build_dir: &str,
) -> crate::Result<()> {
    // The entity is the top level here too — a mixed-signal bench is a
    // direct-drive cosim bench with a Xyce circuit attached — so the two
    // share how the design is compiled and elaborated.
    let entity_name = &mist_config.entity;
    crate::cosim::elaborate_entity_as_top(
        workspace_dir,
        entity_name,
        vhdl_std,
        build_dir,
        &std::collections::BTreeMap::new(),
    )
    .await?;

    // Build the bridge crate
    let bridge_lib =
        build_bridge_library(bench_dir.as_std_path(), name).await?;
    let bridge_lib_str = bridge_lib.to_string_lossy().to_string();

    // Per-bench output directory under target/. The Xyce bridge writes its
    // `.prn` straight here (via rust_cosim::output_dir() -> Xyce's `-o` flag),
    // so nothing is copied out of the source tree afterward.
    let output_dir = crate::bench_output_dir(workspace_dir, name);
    fs::create_dir_all(&output_dir)?;
    let output_dir_abs = output_dir
        .canonicalize_utf8()
        .unwrap_or_else(|_| output_dir.clone());

    // Run co-simulation
    run_nvc_cosim(
        vhdl_std,
        build_dir,
        "work",
        entity_name,
        &bridge_lib_str,
        output_dir_abs.as_str(),
        false,
    )
    .await?;

    // Xyce has written <netlist>.prn into output_dir; generate plots from it.
    let netlist_path = bench_dir.as_std_path().join(&mist_config.netlist);
    let prn_name = netlist_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
        + ".prn";

    #[cfg(feature = "plot")]
    {
        let prn_path = output_dir.as_std_path().join(&prn_name);
        if prn_path.exists() {
            if let Err(e) = plot::generate_plots(
                &netlist_path,
                &prn_path,
                output_dir.as_std_path(),
            ) {
                eprintln!("Warning: plot generation failed: {e}");
            }
        }
    }

    Ok(())
}

/// Build the bridge Rust crate in a bench directory.
async fn build_bridge_library(
    bench_dir: &Path,
    name: &str,
) -> crate::Result<PathBuf> {
    let bench_dir_owned = bench_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let output = crate::cargo_command()
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
