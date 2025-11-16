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
//! use camino::Utf8Path;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let workspace_dir = Utf8Path::new(".");
//!
//! // Initialize a new workspace
//! init_workspace(workspace_dir, "my_project".to_string())?;
//!
//! // Update dependencies
//! update_workspace(workspace_dir).await?;
//! # Ok(())
//! # }
//! ```

use std::cell::RefCell;
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
// Credentials
// ============================================================================

/// Credentials for authenticating with git repositories.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// Create new credentials from username and password.
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
}

// ============================================================================
// Authentication Helpers
// ============================================================================

/// Get access token for a given host from the netrc file.
///
/// This function reads the user's .netrc file and looks for credentials
/// for the specified host. For GitHub, it returns the password field
/// which should contain the personal access token.
/// Get access credentials (username, password) for a given host from the netrc file.
pub fn get_access_credentials_from_netrc(
    host: &str,
) -> Result<Option<Credentials>> {
    let home_dir = dirs::home_dir().ok_or_else(|| VwError::FileSystem {
        message: "Could not determine home directory".to_string(),
    })?;

    let netrc_path = home_dir.join(".netrc");
    if !netrc_path.exists() {
        return Ok(None);
    }

    let netrc_content = std::fs::read_to_string(&netrc_path).map_err(|e| {
        VwError::FileSystem {
            message: format!("Failed to read .netrc file: {e}"),
        }
    })?;

    let netrc = netrc::Netrc::parse(netrc_content.as_bytes()).map_err(|e| {
        VwError::FileSystem {
            message: format!("Failed to parse .netrc file: {e:?}"),
        }
    })?;

    // netrc.hosts is a Vec<(String, Machine)>, so we need to iterate
    for (hostname, machine) in &netrc.hosts {
        if hostname == host {
            // Return both login and password if both are present
            if let Some(password) = &machine.password {
                let login = machine.login.clone();
                return Ok(Some(Credentials::new(login, password.clone())));
            }
        }
    }

    Ok(None)
}

/// Get access token for a given host from the netrc file.
///
/// This function reads the user's .netrc file and looks for credentials
/// for the specified host. For GitHub, it returns the password field
/// which should contain the personal access token.
pub fn get_access_token_from_netrc(host: &str) -> Result<Option<String>> {
    if let Some(creds) = get_access_credentials_from_netrc(host)? {
        Ok(Some(creds.password))
    } else {
        Ok(None)
    }
}

/// Extract hostname from a git repository URL.
///
/// Supports both HTTPS and SSH URLs:
/// - https://github.com/user/repo.git -> github.com
/// - git@github.com:user/repo.git -> github.com
pub fn extract_hostname_from_repo_url(repo_url: &str) -> Result<String> {
    if repo_url.starts_with("https://") {
        let url = url::Url::parse(repo_url).map_err(|e| VwError::Config {
            message: format!("Invalid repository URL '{repo_url}': {e}"),
        })?;
        Ok(url.host_str().unwrap_or("").to_string())
    } else if repo_url.starts_with("git@") {
        // Parse SSH format: git@hostname:path
        if let Some(at_pos) = repo_url.find('@') {
            if let Some(colon_pos) = repo_url[at_pos..].find(':') {
                let hostname = &repo_url[at_pos + 1..at_pos + colon_pos];
                return Ok(hostname.to_string());
            }
        }
        Err(VwError::Config {
            message: format!("Invalid SSH repository URL format: {repo_url}"),
        })
    } else {
        Err(VwError::Config {
            message: format!("Unsupported repository URL format: {repo_url}"),
        })
    }
}

// ============================================================================
// Public API - Workspace Management
// ============================================================================

/// Initialize a new workspace with the given name.
pub fn init_workspace(workspace_dir: &Utf8Path, name: String) -> Result<()> {
    let config_path = workspace_dir.join("vw.toml");
    if config_path.exists() {
        return Err(VwError::Config {
            message: format!("vw.toml already exists in {workspace_dir}"),
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
    update_workspace_with_token(workspace_dir, None).await
}

/// Update workspace dependencies with optional credentials for private repositories.
///
/// # Arguments
/// * `workspace_dir` - Path to the workspace directory
/// * `credentials` - Optional credentials for authentication
pub async fn update_workspace_with_token(
    workspace_dir: &Utf8Path,
    credentials: Option<Credentials>,
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
        // Use credentials passed from caller
        let creds = credentials
            .as_ref()
            .map(|c| (c.username.as_str(), c.password.as_str()));

        let commit_sha = resolve_dependency_commit(
            &dep.repo,
            &dep.branch,
            &dep.commit,
            creds,
        )
        .await
        .map_err(|e| VwError::Dependency {
            message: format!(
                "Failed to resolve commit for dependency '{name}': {e}"
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
                creds,
            )
            .await
            .map_err(|e| VwError::Dependency {
                message: format!("Failed to download dependency '{name}': {e}"),
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
    add_dependency_with_token(
        workspace_dir,
        repo,
        branch,
        commit,
        src,
        name,
        recursive,
        None,
    )
    .await
}

/// Add a new dependency with optional credentials for private repositories.
///
/// # Arguments
/// * `workspace_dir` - Path to the workspace directory
/// * `repo` - Git repository URL
/// * `branch` - Optional branch name
/// * `commit` - Optional commit hash
/// * `src` - Optional source path within the repository
/// * `name` - Optional dependency name
/// * `recursive` - Whether to recursively include VHDL files
/// * `credentials` - Optional credentials for authentication
#[allow(clippy::too_many_arguments)]
pub async fn add_dependency_with_token(
    workspace_dir: &Utf8Path,
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
    recursive: bool,
    _credentials: Option<Credentials>,
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
        tcl_content.push_str(&format!("set dep_files({dep_name}) [list"));

        if !vhdl_files.is_empty() {
            tcl_content.push_str(" \\\n");
            for (i, file) in vhdl_files.iter().enumerate() {
                let path_str = file.to_string_lossy();
                tcl_content.push_str(&format!("    {path_str}"));

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
            message: format!("No 'bench' directory found in {workspace_dir}"),
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
            message: format!("No vw.toml file found in {workspace_dir}"),
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
            message: format!("No vw.lock file found in {workspace_dir}"),
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
    credentials: Option<(&str, &str)>, // (username, password)
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
        (Some(branch), None) => {
            get_branch_head_commit(repo_url, branch, credentials).await
        }
    }
}

async fn get_branch_head_commit(
    repo_url: &str,
    branch: &str,
    credentials: Option<(&str, &str)>, // (username, password)
) -> Result<String> {
    // Normalize repository URL to ensure it ends with .git for GitHub
    let normalized_repo_url =
        if repo_url.contains("github.com") && !repo_url.ends_with(".git") {
            format!("{repo_url}.git")
        } else {
            repo_url.to_string()
        };

    let branch = branch.to_string();
    let credentials = credentials.map(|(u, p)| (u.to_string(), p.to_string()));

    tokio::task::spawn_blocking(move || {
        // Create a temporary directory for the operation
        let temp_dir =
            tempfile::tempdir().map_err(|e| VwError::FileSystem {
                message: format!("Failed to create temporary directory: {e}"),
            })?;

        // Create an empty repository to work with remotes
        let repo =
            git2::Repository::init_bare(temp_dir.path()).map_err(|e| {
                VwError::Git {
                    message: format!(
                        "Failed to initialize temporary repository: {e}"
                    ),
                }
            })?;

        // Create a remote
        let mut remote =
            repo.remote_anonymous(&normalized_repo_url).map_err(|e| {
                VwError::Git {
                    message: format!("Failed to create remote: {e}"),
                }
            })?;

        // Connect and list references
        // Always set a credentials callback so git2 doesn't fail with "no callback set".
        // The callback will try explicit credentials first, then fall back to git's
        // credential helper system (which includes .netrc support).
        let mut callbacks = git2::RemoteCallbacks::new();
        let attempt_count = RefCell::new(0);

        callbacks.credentials(move |url, username_from_url, allowed_types| {
            let mut attempts = attempt_count.borrow_mut();
            *attempts += 1;

            // Limit attempts to prevent infinite loops
            if *attempts > 1 {
                return git2::Cred::default();
            }

            // First, try explicit credentials from netrc if available
            if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT)
            {
                if let Some((ref username, ref password)) = credentials {
                    // Use both username and password from netrc
                    return git2::Cred::userpass_plaintext(username, password);
                }
            }

            // Try SSH key if available
            if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                if let Some(username) = username_from_url {
                    if let Ok(cred) = git2::Cred::ssh_key_from_agent(username) {
                        return Ok(cred);
                    }
                }
            }

            // Fall back to git's credential helper system (includes .netrc)
            if let Ok(config) = git2::Config::open_default() {
                if let Ok(cred) = git2::Cred::credential_helper(
                    &config,
                    url,
                    username_from_url,
                ) {
                    return Ok(cred);
                }
            }

            git2::Cred::default()
        });

        remote
            .connect_auth(git2::Direction::Fetch, Some(callbacks), None)
            .map_err(|e| VwError::Git {
                message: format!("Failed to connect to remote: {e}"),
            })?;

        let refs = remote.list().map_err(|e| VwError::Git {
            message: format!("Failed to list remote references: {e}"),
        })?;

        // Look for the specific branch reference
        let ref_name = format!("refs/heads/{branch}");
        for remote_head in refs {
            if remote_head.name() == ref_name {
                return Ok(remote_head.oid().to_string());
            }
        }

        Err(VwError::Git {
            message: format!(
                "Branch '{branch}' not found in remote repository"
            ),
        })
    })
    .await
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git ls-remote task: {e}"),
    })?
}

async fn download_dependency(
    repo_url: &str,
    commit: &str,
    src_path: &str,
    dest_path: &Path,
    recursive: bool,
    credentials: Option<(&str, &str)>, // (username, password)
) -> Result<()> {
    let temp_dir = tempfile::tempdir().map_err(|e| VwError::FileSystem {
        message: format!("Failed to create temporary directory: {e}"),
    })?;

    // Normalize repository URL to ensure it ends with .git for GitHub
    let normalized_repo_url =
        if repo_url.contains("github.com") && !repo_url.ends_with(".git") {
            format!("{repo_url}.git")
        } else {
            repo_url.to_string()
        };

    let commit = commit.to_string();
    let temp_path = temp_dir.path().to_path_buf();
    let src_path = src_path.to_string();
    let credentials = credentials.map(|(u, p)| (u.to_string(), p.to_string()));

    tokio::task::spawn_blocking(move || {
        // Set up clone options with authentication
        let mut builder = git2::build::RepoBuilder::new();

        // Always set a credentials callback so git2 doesn't fail with "no callback set".
        // The callback will try explicit credentials first, then fall back to git's
        // credential helper system (which includes .netrc support).
        let mut callbacks = git2::RemoteCallbacks::new();
        let attempt_count = RefCell::new(0);

        callbacks.credentials(move |url, username_from_url, allowed_types| {
            let mut attempts = attempt_count.borrow_mut();
            *attempts += 1;

            // Limit attempts to prevent infinite loops
            if *attempts > 1 {
                return git2::Cred::default();
            }

            // First, try explicit credentials from netrc if available
            if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT)
            {
                if let Some((ref username, ref password)) = credentials {
                    // Use both username and password from netrc
                    return git2::Cred::userpass_plaintext(username, password);
                }
            }

            // Try SSH key if available
            if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                if let Some(username) = username_from_url {
                    if let Ok(cred) = git2::Cred::ssh_key_from_agent(username) {
                        return Ok(cred);
                    }
                }
            }

            // Fall back to git's credential helper system (includes .netrc)
            if let Ok(config) = git2::Config::open_default() {
                if let Ok(cred) = git2::Cred::credential_helper(
                    &config,
                    url,
                    username_from_url,
                ) {
                    return Ok(cred);
                }
            }

            git2::Cred::default()
        });

        let mut fetch_options = git2::FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        builder.fetch_options(fetch_options);

        // Clone the repository
        let repo =
            builder
                .clone(&normalized_repo_url, &temp_path)
                .map_err(|e| VwError::Git {
                    message: format!("Failed to clone repository: {e}"),
                })?;

        // Parse the commit SHA
        let commit_oid =
            git2::Oid::from_str(&commit).map_err(|e| VwError::Git {
                message: format!("Invalid commit SHA '{commit}': {e}"),
            })?;

        // Find the commit object
        let commit_obj =
            repo.find_commit(commit_oid).map_err(|e| VwError::Git {
                message: format!("Commit '{commit}' not found: {e}"),
            })?;

        // Checkout the specific commit
        repo.checkout_tree(commit_obj.as_object(), None)
            .map_err(|e| VwError::Git {
                message: format!("Failed to checkout commit '{commit}': {e}"),
            })?;

        // Set HEAD to the commit
        repo.set_head_detached(commit_oid)
            .map_err(|e| VwError::Git {
                message: format!(
                    "Failed to set HEAD to commit '{commit}': {e}"
                ),
            })?;

        Ok::<(), VwError>(())
    })
    .await
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git operations: {e}"),
    })??;

    fs::create_dir_all(dest_path).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create destination directory: {e}"),
    })?;

    // Treat all src values as globs (handles files, directories, and patterns)
    copy_vhdl_files_glob(temp_dir.path(), &src_path, dest_path, recursive)?;

    Ok(())
}

fn copy_vhdl_files_glob(
    repo_root: &Path,
    src_pattern: &str,
    dest: &Path,
    recursive: bool,
) -> Result<()> {
    // Build patterns to match
    let src_path = repo_root.join(src_pattern);
    let mut patterns = Vec::new();
    let strip_prefix: PathBuf;

    // Check if src_pattern points to a directory
    if src_path.is_dir() {
        // It's a directory - create appropriate glob patterns
        let base_pattern =
            src_path.to_str().ok_or_else(|| VwError::FileSystem {
                message: "Invalid UTF-8 in path".to_string(),
            })?;

        if recursive {
            // Recursively find all VHDL files
            patterns.push(format!("{base_pattern}/**/*.vhd"));
            patterns.push(format!("{base_pattern}/**/*.vhdl"));
        } else {
            // Only files directly in the directory
            patterns.push(format!("{base_pattern}/*.vhd"));
            patterns.push(format!("{base_pattern}/*.vhdl"));
        }
        // For directories, strip the src directory from paths
        strip_prefix = src_path;
    } else if src_path.is_file() {
        // It's a single file - use as-is
        patterns.push(
            src_path
                .to_str()
                .ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in path".to_string(),
                })?
                .to_string(),
        );
        // For single files, strip the parent directory
        strip_prefix = src_path
            .parent()
            .ok_or_else(|| VwError::FileSystem {
                message: "File has no parent directory".to_string(),
            })?
            .to_path_buf();
    } else {
        // It's a glob pattern or doesn't exist yet - use as-is
        patterns.push(
            src_path
                .to_str()
                .ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in glob pattern path".to_string(),
                })?
                .to_string(),
        );
        // For glob patterns, strip the repo root to preserve relative structure
        strip_prefix = repo_root.to_path_buf();
    }

    let mut copied_count = 0;
    for pattern_str in &patterns {
        // Use glob to find matching files
        let entries =
            glob::glob(pattern_str).map_err(|e| VwError::FileSystem {
                message: format!("Invalid glob pattern '{pattern_str}': {e}"),
            })?;

        for entry in entries {
            let path = entry.map_err(|e| VwError::FileSystem {
                message: format!("Error reading glob entry: {e}"),
            })?;

            // Only copy VHDL files
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "vhd" || ext == "vhdl" {
                        // Compute relative path based on strip_prefix
                        let relative_path =
                            path.strip_prefix(&strip_prefix).map_err(|e| {
                                VwError::FileSystem {
                                    message: format!(
                                    "Failed to compute relative path for {path:?}: {e}"
                                ),
                                }
                            })?;

                        let dest_file = dest.join(relative_path);

                        // Create parent directories if needed
                        if let Some(parent) = dest_file.parent() {
                            fs::create_dir_all(parent).map_err(|e| {
                                VwError::FileSystem {
                                    message: format!(
                                        "Failed to create directory {parent:?}: {e}"
                                    ),
                                }
                            })?;
                        }

                        fs::copy(&path, &dest_file).map_err(|e| {
                            VwError::FileSystem {
                                message: format!(
                                    "Failed to copy file {path:?}: {e}"
                                ),
                            }
                        })?;
                        copied_count += 1;
                    }
                }
            }
        }
    }

    if copied_count == 0 {
        return Err(VwError::Dependency {
            message: format!("No VHDL files matched pattern '{src_pattern}'"),
        });
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
