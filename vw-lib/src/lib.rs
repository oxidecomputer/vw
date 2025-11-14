// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Core library for VHDL workspace management.
//!
//! This library provides functionality for:
//! - Managing VHDL project dependencies from git repositories
//! - Running testbenches with the NVC simulator
//! - Generating vhdl_ls configuration files
//!
//! # Example
//!
//! ```no_run
//! use vw_lib::{init_workspace, update_workspace};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize a new workspace
//! init_workspace("my_project".to_string())?;
//!
//! // Update dependencies
//! update_workspace().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

// ============================================================================
// Error Types
// ============================================================================

#[derive(Debug)]
pub enum VwError {
    Config { message: String },
    Dependency { message: String },
    Git { message: String },
    FileSystem { message: String },
    Testbench { message: String },
    NvcSimulation { command: String },
    NvcAnalysis { library: String, command: String },
    Io(std::io::Error),
    Serialization(toml::ser::Error),
    Deserialization(toml::de::Error),
    Regex(regex::Error),
}

impl std::error::Error for VwError {}

impl From<std::io::Error> for VwError {
    fn from(err: std::io::Error) -> Self {
        VwError::Io(err)
    }
}

impl From<toml::ser::Error> for VwError {
    fn from(err: toml::ser::Error) -> Self {
        VwError::Serialization(err)
    }
}

impl From<toml::de::Error> for VwError {
    fn from(err: toml::de::Error) -> Self {
        VwError::Deserialization(err)
    }
}

impl From<regex::Error> for VwError {
    fn from(err: regex::Error) -> Self {
        VwError::Regex(err)
    }
}

pub type Result<T> = std::result::Result<T, VwError>;

impl fmt::Display for VwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VwError::NvcSimulation { command } => {
                writeln!(f, "NVC simulation failed")?;
                writeln!(f, "command:")?;
                writeln!(f, "{command}")?;
                Ok(())
            }
            VwError::NvcAnalysis { library, command } => {
                writeln!(f, "NVC analysis failed for library '{library}'")?;
                writeln!(f, "command:")?;
                writeln!(f, "{command}")?;
                Ok(())
            }
            VwError::Config { message } => {
                write!(f, "Configuration error: {message}")
            }
            VwError::Dependency { message } => {
                write!(f, "Dependency error: {message}")
            }
            VwError::Git { message } => {
                write!(f, "Git operation failed: {message}")
            }
            VwError::FileSystem { message } => {
                write!(f, "File system error: {message}")
            }
            VwError::Testbench { message } => {
                write!(f, "Testbench error: {message}")
            }
            VwError::Io(e) => write!(f, "IO error: {e}"),
            VwError::Serialization(e) => write!(f, "Serialization error: {e}"),
            VwError::Deserialization(e) => {
                write!(f, "Deserialization error: {e}")
            }
            VwError::Regex(e) => write!(f, "Regex error: {e}"),
        }
    }
}

// ============================================================================
// VHDL Standard
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub enum VhdlStandard {
    Vhdl2008,
    Vhdl2019,
}

impl fmt::Display for VhdlStandard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VhdlStandard::Vhdl2008 => write!(f, "2008"),
            VhdlStandard::Vhdl2019 => write!(f, "2019"),
        }
    }
}

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    #[allow(dead_code)]
    pub workspace: WorkspaceInfo,
    pub dependencies: HashMap<String, Dependency>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspaceInfo {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Dependency {
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    pub src: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub dependencies: HashMap<String, LockedDependency>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockedDependency {
    pub repo: String,
    pub commit: String,
    pub src: String,
    pub path: PathBuf,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VhdlLsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    pub libraries: HashMap<String, VhdlLsLibrary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VhdlLsLibrary {
    pub files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_third_party: Option<bool>,
}

// ============================================================================
// Public API - Workspace Management
// ============================================================================

/// Initialize a new workspace with the given name.
pub fn init_workspace(workspace_dir: &Utf8Path, name: String) -> Result<()> {
    let config_path = workspace_dir.join("vw.toml");
    if config_path.exists() {
        return Err(VwError::Config {
            message: format!("vw.toml already exists in {}", workspace_dir),
        });
    }

    let config = WorkspaceConfig {
        workspace: WorkspaceInfo {
            name,
            version: "0.1.0".to_string(),
        },
        dependencies: HashMap::new(),
    };

    save_workspace_config(workspace_dir, &config)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub dependencies: Vec<DependencyUpdateInfo>,
}

#[derive(Debug, Clone)]
pub struct DependencyUpdateInfo {
    pub name: String,
    pub commit: String,
    pub was_cached: bool,
}

/// Update workspace dependencies by downloading them and generating configuration files.
pub async fn update_workspace(
    workspace_dir: &Utf8Path,
) -> Result<UpdateResult> {
    let config = load_workspace_config(workspace_dir)?;
    let deps_dir = get_deps_directory()?;

    let mut lock_file = LockFile {
        dependencies: HashMap::new(),
    };

    let mut vhdl_ls_config = VhdlLsConfig {
        standard: None,
        libraries: HashMap::new(),
        lint: None,
    };

    let mut update_info = Vec::new();

    for (name, dep) in &config.dependencies {
        let commit_sha =
            resolve_dependency_commit(&dep.repo, &dep.branch, &dep.commit)
                .await
                .map_err(|_| VwError::Dependency {
                    message: format!(
                        "Failed to resolve commit for dependency '{name}'"
                    ),
                })?;

        let dep_path = deps_dir.join(format!("{name}-{commit_sha}"));

        let was_cached = dep_path.exists();

        if !was_cached {
            download_dependency(
                &dep.repo,
                &commit_sha,
                &dep.src,
                &dep_path,
                dep.recursive,
            )
            .await
            .map_err(|_| VwError::Dependency {
                message: format!("Failed to download dependency '{name}'"),
            })?;
        }

        update_info.push(DependencyUpdateInfo {
            name: name.clone(),
            commit: commit_sha.clone(),
            was_cached,
        });

        lock_file.dependencies.insert(
            name.clone(),
            LockedDependency {
                repo: dep.repo.clone(),
                commit: commit_sha.clone(),
                src: dep.src.clone(),
                path: dep_path.clone(),
                recursive: dep.recursive,
            },
        );

        // Find VHDL files in the cached dependency directory
        let vhdl_files = find_vhdl_files(&dep_path, dep.recursive)?;
        if !vhdl_files.is_empty() {
            let portable_files =
                vhdl_files.into_iter().map(make_path_portable).collect();
            vhdl_ls_config.libraries.insert(
                name.clone(),
                VhdlLsLibrary {
                    files: portable_files,
                    exclude: None,
                    is_third_party: None,
                },
            );
        }
    }

    write_lock_file(workspace_dir, &lock_file)?;
    write_vhdl_ls_config(workspace_dir, &vhdl_ls_config)?;

    Ok(UpdateResult {
        dependencies: update_info,
    })
}

/// Add a new dependency to the workspace configuration.
pub async fn add_dependency(
    workspace_dir: &Utf8Path,
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
    recursive: bool,
) -> Result<()> {
    let mut config =
        load_workspace_config(workspace_dir).unwrap_or_else(|_| {
            WorkspaceConfig {
                workspace: WorkspaceInfo {
                    name: "workspace".to_string(),
                    version: "0.1.0".to_string(),
                },
                dependencies: HashMap::new(),
            }
        });

    // Validate that either branch or commit is provided
    if branch.is_none() && commit.is_none() {
        return Err(VwError::Config {
            message: "Must specify either --branch or --commit".to_string(),
        });
    }

    let dep_name = name.unwrap_or_else(|| extract_repo_name(&repo));
    let src_path = src.unwrap_or_else(|| ".".to_string());

    let dependency = Dependency {
        repo: repo.clone(),
        branch,
        commit,
        src: src_path,
        recursive,
    };

    config.dependencies.insert(dep_name.clone(), dependency);
    save_workspace_config(workspace_dir, &config)?;

    Ok(())
}

/// Remove a dependency from the workspace configuration.
pub fn remove_dependency(workspace_dir: &Utf8Path, name: String) -> Result<()> {
    let mut config = load_workspace_config(workspace_dir)?;

    if config.dependencies.remove(&name).is_some() {
        save_workspace_config(workspace_dir, &config)?;
        Ok(())
    } else {
        Err(VwError::Config {
            message: format!("Dependency '{name}' not found"),
        })
    }
}

/// Clear all cached repositories for the current workspace.
pub fn clear_cache(workspace_dir: &Utf8Path) -> Result<Vec<String>> {
    let config = load_workspace_config(workspace_dir)?;
    let deps_dir = get_deps_directory()?;

    let mut cleared = Vec::new();

    // Get all dependencies from the current workspace
    for name in config.dependencies.keys() {
        if let Ok(entries) = fs::read_dir(&deps_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if let Some(file_name_str) = file_name.to_str() {
                    if file_name_str.starts_with(&format!("{name}-")) {
                        let dep_path = entry.path();
                        if dep_path.is_dir() {
                            fs::remove_dir_all(&dep_path)
                                .map_err(|e| VwError::FileSystem {
                                    message: format!("Failed to remove cached dependency at {dep_path:?}: {e}")
                                })?;
                            cleared.push(file_name_str.to_string());
                        }
                    }
                }
            }
        }
    }

    Ok(cleared)
}

/// List all dependencies in the workspace.
pub fn list_dependencies(
    workspace_dir: &Utf8Path,
) -> Result<Vec<DependencyInfo>> {
    let config = load_workspace_config(workspace_dir)?;
    if config.dependencies.is_empty() {
        return Ok(Vec::new());
    }

    // Try to load lock file to get resolved versions
    let lock_file = load_lock_file(workspace_dir).ok();

    let mut deps = Vec::new();
    for (name, dep) in &config.dependencies {
        let version_info = match &lock_file {
            Some(lock) => {
                if let Some(locked_dep) = lock.dependencies.get(name) {
                    VersionInfo::Locked {
                        commit: locked_dep.commit.clone(),
                    }
                } else {
                    // Not yet resolved, show branch/commit from config
                    match (&dep.branch, &dep.commit) {
                        (Some(branch), None) => VersionInfo::Branch {
                            branch: branch.clone(),
                        },
                        (None, Some(commit)) => VersionInfo::Commit {
                            commit: commit.clone(),
                        },
                        _ => VersionInfo::Unknown,
                    }
                }
            }
            None => {
                // No lock file, show branch/commit from config
                match (&dep.branch, &dep.commit) {
                    (Some(branch), None) => VersionInfo::Branch {
                        branch: branch.clone(),
                    },
                    (None, Some(commit)) => VersionInfo::Commit {
                        commit: commit.clone(),
                    },
                    _ => VersionInfo::Unknown,
                }
            }
        };

        deps.push(DependencyInfo {
            name: name.clone(),
            repo: dep.repo.clone(),
            version: version_info,
        });
    }

    Ok(deps)
}

#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub name: String,
    pub repo: String,
    pub version: VersionInfo,
}

#[derive(Debug, Clone)]
pub enum VersionInfo {
    Branch { branch: String },
    Commit { commit: String },
    Locked { commit: String },
    Unknown,
}

/// Generate a TCL file containing all dependency VHDL files.
/// Creates an associative array where keys are library names and values are lists of files.
pub fn generate_deps_tcl(workspace_dir: &Utf8Path) -> Result<()> {
    let lock_file = load_lock_file(workspace_dir)?;

    // Generate TCL content
    let mut tcl_content = String::from("# Auto-generated by vw\n");
    tcl_content.push_str("# Associative array of dependency VHDL files\n");
    tcl_content
        .push_str("# Keys: library names, Values: lists of VHDL files\n\n");

    // Sort dependency names for consistent output
    let mut dep_names: Vec<_> = lock_file.dependencies.keys().collect();
    dep_names.sort();

    for dep_name in dep_names {
        let locked_dep = &lock_file.dependencies[dep_name];
        let vhdl_files =
            find_vhdl_files(&locked_dep.path, locked_dep.recursive)?;

        // Create array entry for this library
        tcl_content.push_str(&format!("set dep_files({}) [list", dep_name));

        if !vhdl_files.is_empty() {
            tcl_content.push_str(" \\\n");
            for (i, file) in vhdl_files.iter().enumerate() {
                let path_str = file.to_string_lossy();
                tcl_content.push_str(&format!("    {}", path_str));

                // Add backslash continuation for all but the last item
                if i < vhdl_files.len() - 1 {
                    tcl_content.push_str(" \\");
                }
                tcl_content.push('\n');
            }
        }

        tcl_content.push_str("]\n\n");
    }

    // Write to deps.tcl
    let tcl_path = workspace_dir.join("deps.tcl");
    fs::write(&tcl_path, tcl_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write deps.tcl file: {e}"),
    })?;

    Ok(())
}

// ============================================================================
// Public API - Testbench Management
// ============================================================================

/// List all available testbenches in the workspace.
pub fn list_testbenches(
    workspace_dir: &Utf8Path,
) -> Result<Vec<TestbenchInfo>> {
    let bench_dir = workspace_dir.join("bench");
    if !bench_dir.exists() {
        return Ok(Vec::new());
    }

    let mut testbenches = Vec::new();

    for entry in fs::read_dir(bench_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read bench directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "vhd" || extension == "vhdl" {
                    let entities = find_entities_in_file(&path)?;
                    for entity in entities {
                        testbenches.push(TestbenchInfo {
                            name: entity,
                            path: path.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(testbenches)
}

#[derive(Debug, Clone)]
pub struct TestbenchInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Run a testbench using NVC simulator.
pub async fn run_testbench(
    workspace_dir: &Utf8Path,
    testbench_name: String,
    vhdl_std: VhdlStandard,
) -> Result<()> {
    let vhdl_ls_config = load_existing_vhdl_ls_config(workspace_dir)?;

    // First, analyze all non-defaultlib libraries
    for (lib_name, library) in &vhdl_ls_config.libraries {
        if lib_name != "defaultlib" {
            // Convert library name to be NVC-compatible (no hyphens)
            let nvc_lib_name = lib_name.replace('-', "_");

            let mut files = Vec::new();
            for file_path in &library.files {
                // Convert $HOME paths to absolute paths
                let expanded_path = if file_path.starts_with("$HOME") {
                    let home_dir = dirs::home_dir().ok_or_else(|| {
                        VwError::FileSystem {
                            message: "Could not determine home directory"
                                .to_string(),
                        }
                    })?;
                    home_dir.join(
                        file_path.strip_prefix("$HOME/").unwrap_or(file_path),
                    )
                } else {
                    PathBuf::from(file_path)
                };
                files.push(expanded_path);
            }

            // Sort files in dependency order (dependencies first)
            sort_files_by_dependencies(&mut files)?;

            let file_strings: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();

            let mut nvc_cmd = tokio::process::Command::new("nvc");
            nvc_cmd
                .arg(format!("--std={vhdl_std}"))
                .arg(format!("--work={nvc_lib_name}"))
                .arg("-M")
                .arg("256m")
                .arg("-a");

            for file in &file_strings {
                nvc_cmd.arg(file);
            }

            let status =
                nvc_cmd.status().await.map_err(|e| VwError::Testbench {
                    message: format!("Failed to execute NVC analysis: {e}"),
                })?;

            if !status.success() {
                let cmd_str = format!(
                    "nvc --std={} --work={} -M 256m -a {}",
                    vhdl_std,
                    nvc_lib_name,
                    file_strings.join(" ")
                );
                return Err(VwError::NvcAnalysis {
                    library: lib_name.clone(),
                    command: cmd_str,
                });
            }
        }
    }

    // Get defaultlib files for later use
    let defaultlib_files = vhdl_ls_config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    // Look for the testbench file in bench folder
    let bench_dir = workspace_dir.join("bench");
    if !bench_dir.exists() {
        return Err(VwError::Testbench {
            message: format!("No 'bench' directory found in {}", workspace_dir),
        });
    }

    let testbench_file = find_testbench_file(&testbench_name, &bench_dir)?;

    // Filter defaultlib files to exclude OTHER testbenches but allow common bench code
    let bench_dir_abs = workspace_dir.as_std_path().join("bench");
    let filtered_defaultlib_files: Vec<PathBuf> = defaultlib_files
        .into_iter()
        .filter(|file_path| {
            // Convert to absolute path for comparison
            let absolute_path = if file_path.is_relative() {
                workspace_dir.as_std_path().join(file_path)
            } else {
                file_path.clone()
            };

            // If it's not in the bench directory, include it
            if !absolute_path.starts_with(&bench_dir_abs) {
                return true;
            }

            // If it's in the bench directory, check if it's a different testbench
            if let Ok(entities) = find_entities_in_file(&absolute_path) {
                // Exclude files that contain testbench entities other than the one we're running
                for entity in entities {
                    if entity.to_lowercase().ends_with("_tb")
                        && entity != testbench_name
                    {
                        return false; // This is a different testbench, exclude it
                    }
                }
            }

            // Include this file (it's either the current testbench or common bench code)
            true
        })
        .collect();

    // Find only the defaultlib files that are actually referenced by this testbench
    let mut referenced_files =
        find_referenced_files(&testbench_file, &filtered_defaultlib_files)?;

    // Sort files in dependency order (dependencies first)
    sort_files_by_dependencies(&mut referenced_files)?;

    // Run NVC simulation
    let mut nvc_cmd = tokio::process::Command::new("nvc");
    nvc_cmd
        .arg(format!("--std={vhdl_std}"))
        .arg("-M")
        .arg("256m")
        .arg("-L")
        .arg(".")
        .arg("-a")
        .arg("--check-synthesis");

    // Add only the defaultlib files that are referenced by this testbench
    for file_path in &referenced_files {
        nvc_cmd.arg(file_path.to_string_lossy().as_ref());
    }

    // Add testbench file
    nvc_cmd.arg(testbench_file.to_string_lossy().as_ref());

    // Elaborate and run
    nvc_cmd
        .arg("-e")
        .arg(&testbench_name)
        .arg("-r")
        .arg(&testbench_name)
        .arg("--dump-arrays")
        .arg("--format=fst")
        .arg(format!("--wave={testbench_name}.fst"));

    let status = nvc_cmd.status().await.map_err(|e| VwError::Testbench {
        message: format!("Failed to execute NVC simulation: {e}"),
    })?;

    if !status.success() {
        // Build command string for display
        let mut cmd_parts = vec!["nvc".to_string()];
        cmd_parts.push(format!("--std={vhdl_std}"));
        cmd_parts.push("-M".to_string());
        cmd_parts.push("256m".to_string());
        cmd_parts.push("-L".to_string());
        cmd_parts.push(".".to_string());
        cmd_parts.push("-a".to_string());
        cmd_parts.push("--check-synthesis".to_string());

        for file_path in &referenced_files {
            cmd_parts.push(file_path.to_string_lossy().to_string());
        }
        cmd_parts.push(testbench_file.to_string_lossy().to_string());
        cmd_parts.push("-e".to_string());
        cmd_parts.push(testbench_name.clone());
        cmd_parts.push("-r".to_string());
        cmd_parts.push(testbench_name.clone());
        cmd_parts.push("--dump-arrays".to_string());
        cmd_parts.push("--format=fst".to_string());
        cmd_parts.push(format!("--wave={testbench_name}.fst"));

        let cmd_str = cmd_parts.join(" ");
        return Err(VwError::NvcSimulation { command: cmd_str });
    }

    Ok(())
}

// ============================================================================
// Internal Helper Functions
// ============================================================================

fn find_referenced_files(
    testbench_file: &Path,
    available_files: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut referenced_files = Vec::new();
    let mut processed_files = HashSet::new();
    let mut files_to_process = vec![testbench_file.to_path_buf()];

    while let Some(current_file) = files_to_process.pop() {
        if processed_files.contains(&current_file) {
            continue;
        }
        processed_files.insert(current_file.clone());

        // Don't include the testbench file itself in the referenced files
        // (it will be added separately)
        if current_file != testbench_file {
            referenced_files.push(current_file.clone());
        }

        // Parse the file to find dependencies
        let dependencies = find_file_dependencies(&current_file)?;

        // Find corresponding files for each dependency
        for dep in dependencies {
            for available_file in available_files {
                if file_provides_symbol(available_file, &dep)? {
                    if !processed_files.contains(available_file) {
                        files_to_process.push(available_file.clone());
                    }
                    break;
                }
            }
        }
    }

    Ok(referenced_files)
}

fn find_file_dependencies(file_path: &Path) -> Result<Vec<String>> {
    let content =
        fs::read_to_string(file_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read file {file_path:?}: {e}"),
        })?;

    let mut dependencies = HashSet::new();

    // Find 'use work.package_name' statements
    let use_work_pattern = r"(?i)use\s+work\.(\w+)";
    let use_work_re = regex::Regex::new(use_work_pattern)?;

    for captures in use_work_re.captures_iter(&content) {
        if let Some(package_name) = captures.get(1) {
            dependencies.insert(package_name.as_str().to_string());
        }
    }

    // Find direct entity instantiations (instance_name: entity work.entity_name)
    let entity_inst_pattern = r"(?i)\w+\s*:\s*entity\s+work\.(\w+)";
    let entity_inst_re = regex::Regex::new(entity_inst_pattern)?;

    for captures in entity_inst_re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(1) {
            dependencies.insert(entity_name.as_str().to_string());
        }
    }

    // Find component instantiations (component_name : entity_name)
    let component_pattern = r"(?i)(\w+)\s*:\s*(\w+)";
    let component_re = regex::Regex::new(component_pattern)?;

    for captures in component_re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(2) {
            // Skip if this looks like an entity instantiation (already handled above)
            if !entity_name.as_str().eq_ignore_ascii_case("entity") {
                dependencies.insert(entity_name.as_str().to_string());
            }
        }
    }

    // Find component declarations
    let comp_decl_pattern = r"(?i)component\s+(\w+)";
    let comp_decl_re = regex::Regex::new(comp_decl_pattern)?;

    for captures in comp_decl_re.captures_iter(&content) {
        if let Some(comp_name) = captures.get(1) {
            dependencies.insert(comp_name.as_str().to_string());
        }
    }

    Ok(dependencies.into_iter().collect())
}

fn file_provides_symbol(file_path: &Path, symbol: &str) -> Result<bool> {
    let content =
        fs::read_to_string(file_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read file {file_path:?}: {e}"),
        })?;

    // Check for package declaration
    let package_pattern =
        format!(r"(?i)\bpackage\s+{}\s+is\b", regex::escape(symbol));
    let package_re = regex::Regex::new(&package_pattern)?;

    if package_re.is_match(&content) {
        return Ok(true);
    }

    // Check for entity declaration
    let entity_pattern =
        format!(r"(?i)\bentity\s+{}\s+is\b", regex::escape(symbol));
    let entity_re = regex::Regex::new(&entity_pattern)?;

    if entity_re.is_match(&content) {
        return Ok(true);
    }

    Ok(false)
}

fn sort_files_by_dependencies(files: &mut Vec<PathBuf>) -> Result<()> {
    // Build dependency graph
    let mut dependencies: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut all_symbols: HashMap<String, PathBuf> = HashMap::new();

    // First pass: collect all symbols provided by each file
    for file in files.iter() {
        let symbols = get_file_symbols(file)?;
        for symbol in symbols {
            all_symbols.insert(symbol, file.clone());
        }
    }

    // Second pass: find dependencies for each file
    for file in files.iter() {
        let deps = find_file_dependencies(file)?;
        let mut file_deps = Vec::new();

        for dep in deps {
            if let Some(provider_file) = all_symbols.get(&dep) {
                if provider_file != file {
                    file_deps.push(provider_file.clone());
                }
            }
        }

        dependencies.insert(file.clone(), file_deps);
    }

    // Topological sort using Kahn's algorithm
    let sorted = topological_sort(files.clone(), dependencies)?;
    *files = sorted;

    Ok(())
}

fn get_file_symbols(file_path: &Path) -> Result<Vec<String>> {
    let content =
        fs::read_to_string(file_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read file {file_path:?}: {e}"),
        })?;

    let mut symbols = Vec::new();

    // Find package declarations
    let package_pattern = r"(?i)\bpackage\s+(\w+)\s+is\b";
    let package_re = regex::Regex::new(package_pattern)?;

    for captures in package_re.captures_iter(&content) {
        if let Some(package_name) = captures.get(1) {
            symbols.push(package_name.as_str().to_string());
        }
    }

    // Find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let entity_re = regex::Regex::new(entity_pattern)?;

    for captures in entity_re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(1) {
            symbols.push(entity_name.as_str().to_string());
        }
    }

    Ok(symbols)
}

fn topological_sort(
    files: Vec<PathBuf>,
    dependencies: HashMap<PathBuf, Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let mut in_degree: HashMap<PathBuf, usize> = HashMap::new();
    let mut adj_list: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    // Initialize in-degree and adjacency list
    for file in &files {
        in_degree.insert(file.clone(), 0);
        adj_list.insert(file.clone(), Vec::new());
    }

    // Build the graph
    for (file, deps) in &dependencies {
        for dep in deps {
            if files.contains(dep) {
                adj_list.get_mut(dep).unwrap().push(file.clone());
                *in_degree.get_mut(file).unwrap() += 1;
            }
        }
    }

    // Kahn's algorithm
    let mut queue = VecDeque::new();
    let mut result = Vec::new();

    // Add all nodes with in-degree 0 to queue
    for (file, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(file.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        result.push(current.clone());

        // For each neighbor of current
        if let Some(neighbors) = adj_list.get(&current) {
            for neighbor in neighbors {
                *in_degree.get_mut(neighbor).unwrap() -= 1;
                if in_degree[neighbor] == 0 {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    // Check for cycles
    if result.len() != files.len() {
        return Err(VwError::Dependency {
            message: "Circular dependency detected in VHDL files".to_string(),
        });
    }

    Ok(result)
}

fn find_entities_in_file(file_path: &Path) -> Result<Vec<String>> {
    let content =
        fs::read_to_string(file_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read file {file_path:?}: {e}"),
        })?;

    let mut entities = Vec::new();

    // Regex to find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let re = regex::Regex::new(entity_pattern)?;

    for captures in re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(1) {
            entities.push(entity_name.as_str().to_string());
        }
    }

    Ok(entities)
}

fn find_testbench_file(
    testbench_name: &str,
    bench_dir: &Utf8Path,
) -> Result<PathBuf> {
    let mut found_files = Vec::new();

    for entry in fs::read_dir(bench_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read bench directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "vhd" || extension == "vhdl" {
                    // Check if this file contains the entity we're looking for
                    if file_contains_entity(&path, testbench_name)? {
                        found_files.push(path);
                    }
                }
            }
        }
    }

    match found_files.len() {
        0 => Err(VwError::Testbench {
            message: format!("Testbench entity '{testbench_name}' not found in bench directory")
        }),
        1 => Ok(found_files.into_iter().next().unwrap()),
        _ => Err(VwError::Testbench {
            message: format!("Multiple files contain entity '{testbench_name}': {found_files:?}")
        }),
    }
}

fn file_contains_entity(file_path: &Path, entity_name: &str) -> Result<bool> {
    let content =
        fs::read_to_string(file_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read file {file_path:?}: {e}"),
        })?;

    // Simple regex to find entity declarations
    // This is a basic implementation that looks for "entity <name> is"
    let entity_pattern =
        format!(r"(?i)\bentity\s+{}\s+is\b", regex::escape(entity_name));
    let re = regex::Regex::new(&entity_pattern)?;

    Ok(re.is_match(&content))
}

fn make_path_portable(path: PathBuf) -> PathBuf {
    if let Some(home_dir) = dirs::home_dir() {
        if let Ok(relative_path) = path.strip_prefix(&home_dir) {
            return PathBuf::from("$HOME").join(relative_path);
        }
    }
    path
}

fn extract_repo_name(repo_url: &str) -> String {
    repo_url
        .trim_end_matches(".git")
        .split('/')
        .next_back()
        .unwrap_or("dependency")
        .to_string()
}

fn save_workspace_config(
    workspace_dir: &Utf8Path,
    config: &WorkspaceConfig,
) -> Result<()> {
    let toml_content = toml::to_string_pretty(config)?;
    let config_path = workspace_dir.join("vw.toml");

    fs::write(&config_path, toml_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write vw.toml file: {e}"),
    })?;

    Ok(())
}

pub fn load_workspace_config(
    workspace_dir: &Utf8Path,
) -> Result<WorkspaceConfig> {
    let config_path = workspace_dir.join("vw.toml");
    if !config_path.exists() {
        return Err(VwError::Config {
            message: format!("No vw.toml file found in {}", workspace_dir),
        });
    }

    let config_content =
        fs::read_to_string(&config_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read vw.toml: {e}"),
        })?;

    let config: WorkspaceConfig = toml::from_str(&config_content)?;

    Ok(config)
}

fn load_lock_file(workspace_dir: &Utf8Path) -> Result<LockFile> {
    let lock_path = workspace_dir.join("vw.lock");
    if !lock_path.exists() {
        return Err(VwError::Config {
            message: format!("No vw.lock file found in {}", workspace_dir),
        });
    }

    let lock_content =
        fs::read_to_string(&lock_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read vw.lock: {e}"),
        })?;

    let lock_file: LockFile = toml::from_str(&lock_content)?;

    Ok(lock_file)
}

fn get_deps_directory() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().ok_or_else(|| VwError::FileSystem {
        message: "Could not determine home directory".to_string(),
    })?;

    let deps_dir = home_dir.join(".vw").join("deps");
    fs::create_dir_all(&deps_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create dependencies directory: {e}"),
    })?;

    Ok(deps_dir)
}

async fn resolve_dependency_commit(
    repo_url: &str,
    branch: &Option<String>,
    commit: &Option<String>,
) -> Result<String> {
    match (branch, commit) {
        (Some(_), Some(_)) => Err(VwError::Config {
            message: "Cannot specify both branch and commit for dependency"
                .to_string(),
        }),
        (None, None) => Err(VwError::Config {
            message: "Must specify either branch or commit for dependency"
                .to_string(),
        }),
        (None, Some(commit)) => Ok(commit.clone()),
        (Some(branch), None) => get_branch_head_commit(repo_url, branch).await,
    }
}

async fn get_branch_head_commit(
    repo_url: &str,
    branch: &str,
) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(["ls-remote", repo_url, branch])
        .output()
        .await
        .map_err(|e| VwError::Git {
            message: format!("Failed to execute git ls-remote: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(VwError::Git {
            message: format!("Git ls-remote failed: {stderr}"),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit =
        stdout
            .split_whitespace()
            .next()
            .ok_or_else(|| VwError::Git {
                message: "Could not parse git ls-remote output".to_string(),
            })?;

    Ok(commit.to_string())
}

async fn download_dependency(
    repo_url: &str,
    commit: &str,
    src_path: &str,
    dest_path: &Path,
    recursive: bool,
) -> Result<()> {
    let temp_dir = tempfile::tempdir().map_err(|e| VwError::FileSystem {
        message: format!("Failed to create temporary directory: {e}"),
    })?;

    let clone_output = tokio::process::Command::new("git")
        .args(["clone", repo_url, temp_dir.path().to_str().unwrap()])
        .output()
        .await
        .map_err(|e| VwError::Git {
            message: format!("Failed to execute git clone: {e}"),
        })?;

    if !clone_output.status.success() {
        let stderr = String::from_utf8_lossy(&clone_output.stderr);
        return Err(VwError::Git {
            message: format!("Git clone failed: {stderr}"),
        });
    }

    let checkout_output = tokio::process::Command::new("git")
        .current_dir(temp_dir.path())
        .args(["checkout", commit])
        .output()
        .await
        .map_err(|e| VwError::Git {
            message: format!("Failed to execute git checkout: {e}"),
        })?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        return Err(VwError::Git {
            message: format!("Git checkout failed: {stderr}"),
        });
    }

    let src_dir = temp_dir.path().join(src_path);
    if !src_dir.exists() {
        return Err(VwError::Dependency {
            message: format!(
                "Source path '{src_path}' does not exist in repository"
            ),
        });
    }

    fs::create_dir_all(dest_path).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create destination directory: {e}"),
    })?;

    copy_vhdl_files(&src_dir, dest_path, recursive)?;

    Ok(())
}

fn copy_vhdl_files(src: &Path, dest: &Path, recursive: bool) -> Result<()> {
    for entry in fs::read_dir(src).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read source directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_dir() {
            if recursive {
                let dest_subdir = dest.join(entry.file_name());
                fs::create_dir_all(&dest_subdir).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to create subdirectory: {e}"),
                    }
                })?;
                copy_vhdl_files(&path, &dest_subdir, recursive)?;
            }
        } else if let Some(ext) = path.extension() {
            if ext == "vhd" || ext == "vhdl" {
                let dest_file = dest.join(entry.file_name());
                fs::copy(&path, &dest_file).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to copy file {path:?}: {e}"),
                    }
                })?;
            }
        }
    }
    Ok(())
}

fn find_vhdl_files(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut vhdl_files = Vec::new();
    find_vhdl_files_impl(dir, &mut vhdl_files, recursive)?;
    Ok(vhdl_files)
}

fn find_vhdl_files_impl(
    dir: &Path,
    vhdl_files: &mut Vec<PathBuf>,
    recursive: bool,
) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_dir() {
            if recursive {
                find_vhdl_files_impl(&path, vhdl_files, recursive)?;
            }
        } else if let Some(extension) =
            path.extension().and_then(|ext| ext.to_str())
        {
            if extension == "vhd" || extension == "vhdl" {
                vhdl_files.push(path);
            }
        }
    }
    Ok(())
}

fn write_lock_file(
    workspace_dir: &Utf8Path,
    lock_file: &LockFile,
) -> Result<()> {
    let toml_content = toml::to_string_pretty(lock_file)?;
    let lock_path = workspace_dir.join("vw.lock");

    fs::write(&lock_path, toml_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write vw.lock file: {e}"),
    })?;

    Ok(())
}

fn write_vhdl_ls_config(
    workspace_dir: &Utf8Path,
    managed_config: &VhdlLsConfig,
) -> Result<()> {
    let mut existing_config = load_existing_vhdl_ls_config(workspace_dir)?;

    // Remove any existing managed dependencies and add the new ones
    for (name, library) in &managed_config.libraries {
        existing_config
            .libraries
            .insert(name.clone(), library.clone());
    }

    let toml_content = toml::to_string_pretty(&existing_config)?;
    let config_path = workspace_dir.join("vhdl_ls.toml");

    fs::write(&config_path, toml_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write vhdl_ls.toml file: {e}"),
    })?;

    Ok(())
}

fn load_existing_vhdl_ls_config(
    workspace_dir: &Utf8Path,
) -> Result<VhdlLsConfig> {
    let config_path = workspace_dir.join("vhdl_ls.toml");
    if config_path.exists() {
        let config_content = fs::read_to_string(&config_path).map_err(|e| {
            VwError::FileSystem {
                message: format!("Failed to read existing vhdl_ls.toml: {e}"),
            }
        })?;

        let config: VhdlLsConfig = toml::from_str(&config_content)?;

        Ok(config)
    } else {
        Ok(VhdlLsConfig {
            standard: None,
            libraries: HashMap::new(),
            lint: None,
        })
    }
}
