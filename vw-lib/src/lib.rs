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
use std::collections::{hash_map::Entry, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use vhdl_lang::{VHDLParser, VHDLStandard};

use petgraph::{
    algo::toposort,
    graph::{DiGraph, NodeIndex},
};

use crate::mapping::{FileData, VwSymbol, VwSymbolFinder};
use crate::nvc_helpers::{run_nvc_analysis, run_nvc_elab, run_nvc_sim};
use crate::visitor::walk_design_file;

pub mod mapping;
pub mod nvc_helpers;
pub mod sim;
pub mod visitor;

const BUILD_DIR: &str = "vw_build";

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
    NvcElab { command: String },
    NvcAnalysis { library: String, command: String },
    CodeGen { message: String },
    Simulation { message: String },
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
            VwError::NvcElab { command } => {
                writeln!(f, "NVC elaboration failed")?;
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
            VwError::CodeGen { message } => {
                write!(f, "Code generation failed: {message}")
            }
            VwError::Simulation { message } => {
                write!(f, "Simulation error: {message}")
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

impl From<VhdlStandard> for VHDLStandard {
    fn from(val: VhdlStandard) -> Self {
        match val {
            VhdlStandard::Vhdl2008 => VHDLStandard::VHDL2008,
            VhdlStandard::Vhdl2019 => VHDLStandard::VHDL2019,
        }
    }
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
    #[serde(default)]
    pub tools: Option<ToolsConfig>,
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
    #[serde(default)]
    pub src: Vec<String>,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub sim_only: bool,
    #[serde(default)]
    pub submodules: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub dependencies: HashMap<String, LockedDependency>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockedDependency {
    pub repo: String,
    pub commit: String,
    #[serde(default)]
    pub src: Vec<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub sim_only: bool,
    #[serde(default)]
    pub submodules: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VhdlLsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    pub libraries: HashMap<String, VhdlLsLibrary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Deserialize, Debug)]
struct CargoToml {
    package: CargoPackage,
}

#[derive(Deserialize, Debug)]
struct CargoPackage {
    name: String,
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
// Tool Configuration (workspace-wide [tools] section)
// ============================================================================

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolsConfig {
    #[serde(default)]
    pub xyce: Option<XyceConfig>,
    #[serde(default, rename = "rust-cosim")]
    pub rust_cosim: Option<RustCosimConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct XyceConfig {
    pub prefix: String,
    #[serde(rename = "trilinos-prefix")]
    pub trilinos_prefix: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RustCosimConfig {
    pub path: String,
}

// ============================================================================
// Mixed-Signal Configuration (per-bench mist.toml)
// ============================================================================

/// Configuration parsed from a per-bench `mist.toml` file.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MistConfig {
    /// Path to Xyce netlist, relative to the bench directory.
    pub netlist: String,
    /// VHDL entity name to co-simulate.
    pub entity: String,
    /// Clock frequency in Hz.
    pub clock: f64,
    /// Number of cycles to prime the pipeline before recording.
    #[serde(default, rename = "prime-cycles")]
    pub prime_cycles: Option<u32>,
    /// Port-to-DAC mappings.
    #[serde(default)]
    pub ports: HashMap<String, PortMapping>,
}

/// Maps a VHDL output port to a Xyce YDAC device.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PortMapping {
    /// Xyce YDAC device name (e.g., "dac_sym_main").
    pub dac: String,
    /// Encoding type: "pam4" or "unsigned".
    pub encoding: String,
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
        tools: None,
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
                &dep.exclude,
                dep.submodules,
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
                sim_only: dep.sim_only,
                submodules: dep.submodules,
                exclude: dep.exclude.clone(),
            },
        );

        // Find VHDL files in the cached dependency directory
        let vhdl_files =
            find_vhdl_files(&dep_path, dep.recursive, &dep.exclude)?;
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
#[allow(clippy::too_many_arguments)]
pub async fn add_dependency(
    workspace_dir: &Utf8Path,
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
    recursive: bool,
    sim_only: bool,
) -> Result<()> {
    add_dependency_with_token(
        workspace_dir,
        repo,
        branch,
        commit,
        src,
        name,
        recursive,
        sim_only,
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
/// * `sim_only` - Whether this dependency is only for simulation (excluded from deps.tcl)
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
    sim_only: bool,
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
                tools: None,
            }
        });

    // Validate that either branch or commit is provided
    if branch.is_none() && commit.is_none() {
        return Err(VwError::Config {
            message: "Must specify either --branch or --commit".to_string(),
        });
    }

    let dep_name = name.unwrap_or_else(|| extract_repo_name(&repo));
    let src_paths = vec![src.unwrap_or_else(|| ".".to_string())];

    let dependency = Dependency {
        repo: repo.clone(),
        branch,
        commit,
        src: src_paths,
        recursive,
        sim_only,
        submodules: false,
        exclude: Vec::new(),
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

/// Resolve dependency VHDL files from the lock file.
/// Returns a map of library name to list of absolute file paths,
/// skipping sim-only dependencies.
pub fn resolve_deps(
    workspace_dir: &Utf8Path,
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let lock_file = load_lock_file(workspace_dir)?;
    let mut deps = HashMap::new();

    for (dep_name, locked_dep) in &lock_file.dependencies {
        if locked_dep.sim_only {
            continue;
        }
        let vhdl_files = find_vhdl_files(
            &locked_dep.path,
            locked_dep.recursive,
            &locked_dep.exclude,
        )?;
        deps.insert(dep_name.clone(), vhdl_files);
    }

    Ok(deps)
}

/// Format a dependency map as a TCL associative array.
/// Each entry becomes `set dep_files(lib_name) [list file1 file2 ...]`.
pub fn format_deps_tcl(deps: &HashMap<String, Vec<PathBuf>>) -> String {
    let mut tcl_content = String::from("# Auto-generated by vw\n");
    tcl_content.push_str("# Associative array of dependency VHDL files\n");
    tcl_content
        .push_str("# Keys: library names, Values: lists of VHDL files\n\n");

    let mut dep_names: Vec<_> = deps.keys().collect();
    dep_names.sort();

    for dep_name in dep_names {
        let vhdl_files = &deps[dep_name];

        tcl_content.push_str(&format!("set dep_files({dep_name}) [list"));

        if !vhdl_files.is_empty() {
            tcl_content.push_str(" \\\n");
            for (i, file) in vhdl_files.iter().enumerate() {
                let path_str = file.to_string_lossy();
                tcl_content.push_str(&format!("    {path_str}"));

                if i < vhdl_files.len() - 1 {
                    tcl_content.push_str(" \\");
                }
                tcl_content.push('\n');
            }
        }

        tcl_content.push_str("]\n\n");
    }

    tcl_content
}

/// Generate a TCL file containing all dependency VHDL files.
/// Creates an associative array where keys are library names and values are lists of files.
pub fn generate_deps_tcl(workspace_dir: &Utf8Path) -> Result<()> {
    let deps = resolve_deps(workspace_dir)?;
    let tcl_content = format_deps_tcl(&deps);

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
    bench_dir: &Utf8Path,
    ignore_dirs: &HashSet<String>,
    recurse: bool,
) -> Result<Vec<TestbenchInfo>> {
    let mut entities_cache = HashMap::new();
    list_testbenches_impl(bench_dir, ignore_dirs, recurse, &mut entities_cache)
}

fn list_testbenches_impl(
    bench_dir: &Utf8Path,
    ignore_dirs: &HashSet<String>,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<Vec<TestbenchInfo>> {
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
                    let entities = get_cached_entities(&path, entities_cache)?;
                    for entity in entities {
                        testbenches.push(TestbenchInfo {
                            name: entity.clone(),
                            path: path.clone(),
                        });
                    }
                }
            }
        } else if recurse {
            let dir_path: Utf8PathBuf =
                path.try_into().map_err(|e| VwError::FileSystem {
                    message: format!("Failed to get dir path: {e}"),
                })?;
            if let Some(file_name) = dir_path.file_name() {
                if !ignore_dirs.contains(file_name) {
                    let mut lower_testbenches = list_testbenches_impl(
                        &dir_path,
                        ignore_dirs,
                        recurse,
                        entities_cache,
                    )?;
                    testbenches.append(&mut lower_testbenches);
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

pub struct RecordProcessor {
    pub vhdl_std: VhdlStandard,
    pub symbols: HashMap<String, VwSymbol>,
    pub symbol_to_file: HashMap<String, String>,
    pub tagged_names: HashSet<String>,
    pub file_info: HashMap<String, FileData>,
    pub target_attr: String,
}

const RECORD_PARSE_ATTRIBUTE: &str = "serialize_rust";
impl RecordProcessor {
    pub fn new(std: VhdlStandard) -> Self {
        Self {
            vhdl_std: std,
            symbols: HashMap::new(),
            symbol_to_file: HashMap::new(),
            tagged_names: HashSet::new(),
            file_info: HashMap::new(),
            target_attr: RECORD_PARSE_ATTRIBUTE.to_string(),
        }
    }
}

// ============================================================================
// File Cache - Reduces redundant file reads during build
// ============================================================================

/// Cache for parsed file data to avoid redundant parsing during builds.
/// Only caches parsed results, not raw file contents.
pub struct FileCache {
    dependencies: HashMap<PathBuf, Vec<VwSymbol>>,
    provided_symbols: HashMap<PathBuf, Vec<VwSymbol>>,
    entities: HashMap<PathBuf, Vec<String>>,
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            dependencies: HashMap::new(),
            provided_symbols: HashMap::new(),
            entities: HashMap::new(),
        }
    }

    /// Get cached file dependencies, reading and parsing file if not cached.
    pub fn get_dependencies(&mut self, path: &Path) -> Result<&Vec<VwSymbol>> {
        match self.dependencies.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let deps = parse_file_dependencies(&content)?;
                Ok(e.insert(deps))
            }
        }
    }

    /// Get cached provided symbols (packages and entities), reading and parsing if not cached.
    pub fn get_provided_symbols(
        &mut self,
        path: &Path,
    ) -> Result<&Vec<VwSymbol>> {
        match self.provided_symbols.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let symbols = parse_provided_symbols(&content)?;
                Ok(e.insert(symbols))
            }
        }
    }

    /// Get cached entities in file, reading and parsing if not cached.
    pub fn get_entities(&mut self, path: &Path) -> Result<&Vec<String>> {
        match self.entities.entry(path.to_path_buf()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let content = fs::read_to_string(path).map_err(|e| {
                    VwError::FileSystem {
                        message: format!("Failed to read file {path:?}: {e}"),
                    }
                })?;
                let entities = parse_entities(&content)?;
                Ok(e.insert(entities))
            }
        }
    }

    /// Get mutable access to the entities cache for functions that only need entity lookups.
    pub fn entities_cache_mut(&mut self) -> &mut HashMap<PathBuf, Vec<String>> {
        &mut self.entities
    }
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse dependencies from file content (extracted for use by FileCache).
fn parse_file_dependencies(content: &str) -> Result<Vec<VwSymbol>> {
    let mut dependencies = Vec::new();
    let mut seen = HashSet::new();

    // Package imports from "use work.package_name"
    let imports = get_package_imports(content)?;
    for pkg in imports {
        let key = format!("pkg:{}", pkg.to_lowercase());
        if seen.insert(key) {
            dependencies.push(VwSymbol::Package(pkg));
        }
    }

    // Find direct entity instantiations (instance_name: entity work.entity_name)
    let entity_inst_pattern = r"(?i)\w+\s*:\s*entity\s+work\.(\w+)";
    let entity_inst_re = regex::Regex::new(entity_inst_pattern)?;

    for captures in entity_inst_re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            let name = entity_name.as_str().to_string();
            let key = format!("ent:{}", name.to_lowercase());
            if seen.insert(key) {
                dependencies.push(VwSymbol::Entity(name));
            }
        }
    }

    // Find component declarations
    let comp_decl_pattern = r"(?i)component\s+(\w+)";
    let comp_decl_re = regex::Regex::new(comp_decl_pattern)?;

    for captures in comp_decl_re.captures_iter(content) {
        if let Some(comp_name) = captures.get(1) {
            let name = comp_name.as_str().to_string();
            let key = format!("ent:{}", name.to_lowercase());
            if seen.insert(key) {
                dependencies.push(VwSymbol::Entity(name));
            }
        }
    }

    Ok(dependencies)
}

/// Parse provided symbols (packages and entities) from file content.
fn parse_provided_symbols(content: &str) -> Result<Vec<VwSymbol>> {
    let mut symbols = Vec::new();

    // Find package declarations
    let package_pattern = r"(?i)\bpackage\s+(\w+)\s+is\b";
    let package_re = regex::Regex::new(package_pattern)?;

    for captures in package_re.captures_iter(content) {
        if let Some(package_name) = captures.get(1) {
            symbols.push(VwSymbol::Package(package_name.as_str().to_string()));
        }
    }

    // Find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let entity_re = regex::Regex::new(entity_pattern)?;

    for captures in entity_re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            symbols.push(VwSymbol::Entity(entity_name.as_str().to_string()));
        }
    }

    Ok(symbols)
}

/// Parse entity declarations from file content.
fn parse_entities(content: &str) -> Result<Vec<String>> {
    let mut entities = Vec::new();

    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let re = regex::Regex::new(entity_pattern)?;

    for captures in re.captures_iter(content) {
        if let Some(entity_name) = captures.get(1) {
            entities.push(entity_name.as_str().to_string());
        }
    }

    Ok(entities)
}

pub async fn analyze_ext_libraries(
    vhdl_ls_config: &VhdlLsConfig,
    processor: &mut RecordProcessor,
    vhdl_std: VhdlStandard,
    cache: &mut FileCache,
) -> Result<()> {
    // Collect non-defaultlib library names
    let ext_lib_names: Vec<String> = vhdl_ls_config
        .libraries
        .keys()
        .filter(|k| k.as_str() != "defaultlib")
        .cloned()
        .collect();

    // Build inter-library dependency graph by scanning for `library <name>;`
    let ext_lib_set: HashSet<String> = ext_lib_names.iter().cloned().collect();
    let mut lib_deps: HashMap<String, Vec<String>> = HashMap::new();
    for lib_name in &ext_lib_names {
        let mut deps = Vec::new();
        if let Some(library) = vhdl_ls_config.libraries.get(lib_name) {
            for file_path in &library.files {
                let expanded = if file_path.starts_with("$HOME") {
                    if let Some(home) = dirs::home_dir() {
                        home.join(
                            file_path
                                .strip_prefix("$HOME/")
                                .unwrap_or(file_path),
                        )
                    } else {
                        PathBuf::from(file_path)
                    }
                } else {
                    PathBuf::from(file_path)
                };
                if let Ok(contents) = fs::read_to_string(&expanded) {
                    for line in contents.lines() {
                        let trimmed = line.trim().to_lowercase();
                        if let Some(rest) = trimmed.strip_prefix("library ") {
                            let dep_lib = rest.trim_end_matches(';').trim();
                            if ext_lib_set.contains(dep_lib)
                                && dep_lib != lib_name.to_lowercase()
                            {
                                deps.push(dep_lib.to_string());
                            }
                        }
                    }
                }
            }
        }
        lib_deps.insert(lib_name.clone(), deps);
    }

    // Topological sort of library names (Kahn's algorithm)
    let mut in_degree: HashMap<String, usize> =
        ext_lib_names.iter().map(|n| (n.clone(), 0)).collect();
    let mut adj: HashMap<String, Vec<String>> = ext_lib_names
        .iter()
        .map(|n| (n.clone(), Vec::new()))
        .collect();
    for (lib, deps) in &lib_deps {
        for dep in deps {
            if let Some(neighbors) = adj.get_mut(dep) {
                neighbors.push(lib.clone());
            }
            if let Some(deg) = in_degree.get_mut(lib) {
                *deg += 1;
            }
        }
    }
    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(n, _)| n.clone())
        .collect();
    let mut sorted_libs = Vec::new();
    while let Some(current) = queue.pop_front() {
        sorted_libs.push(current.clone());
        if let Some(neighbors) = adj.get(&current) {
            for neighbor in neighbors {
                if let Some(deg) = in_degree.get_mut(neighbor) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }
    }
    // Fall back to unsorted if cycle detected
    if sorted_libs.len() != ext_lib_names.len() {
        sorted_libs = ext_lib_names;
    }

    // Analyze libraries in dependency order
    for lib_name in &sorted_libs {
        if let Some(library) = vhdl_ls_config.libraries.get(lib_name) {
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
            sort_files_by_dependencies(processor, &mut files, cache)?;

            let file_strings: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();

            run_nvc_analysis(
                vhdl_std,
                BUILD_DIR,
                &nvc_lib_name,
                &file_strings,
                false,
            )
            .await?;
        }
    }

    Ok(())
}

/// Run a testbench using NVC simulator.
pub async fn run_testbench(
    workspace_dir: &Utf8Path,
    testbench_name: String,
    vhdl_std: VhdlStandard,
    recurse: bool,
    runtime_flags: &[String],
    build_rust: bool,
    scaffold: bool,
) -> Result<()> {
    // Check for mixed-signal test (mist.toml in bench/<name>/)
    let bench_test_dir = workspace_dir.join("bench").join(&testbench_name);
    let mist_toml = bench_test_dir.join("mist.toml");
    if mist_toml.exists() {
        let ws_config = load_workspace_config(workspace_dir)?;
        let mist_content =
            fs::read_to_string(&mist_toml).map_err(|e| VwError::Config {
                message: format!("Failed to read mist.toml: {e}"),
            })?;
        let mist_config: MistConfig =
            toml::from_str(&mist_content).map_err(|e| VwError::Config {
                message: format!("Failed to parse mist.toml: {e}"),
            })?;
        if scaffold {
            return sim::scaffold(
                &bench_test_dir,
                &mist_config,
                &ws_config.tools,
            );
        }
        return sim::run_analog_test(
            workspace_dir,
            &testbench_name,
            &bench_test_dir,
            &mist_config,
            &ws_config.tools,
            vhdl_std,
        )
        .await;
    }

    let vhdl_ls_config = load_existing_vhdl_ls_config(workspace_dir)?;
    let mut processor = RecordProcessor::new(vhdl_std);
    let mut cache = FileCache::new();

    fs::create_dir_all(BUILD_DIR)?;

    // First, analyze all non-defaultlib libraries
    analyze_ext_libraries(
        &vhdl_ls_config,
        &mut processor,
        vhdl_std,
        &mut cache,
    )
    .await?;

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

    let testbench_file = find_testbench_file(
        &testbench_name,
        &bench_dir,
        recurse,
        cache.entities_cache_mut(),
    )?;

    // Filter defaultlib files to exclude OTHER testbenches but allow common bench code
    let bench_dir_abs = workspace_dir.as_std_path().join("bench");

    // Pre-compute entities for bench files to avoid mutable borrow in closure
    let mut bench_file_entities: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for file_path in &defaultlib_files {
        let absolute_path = if file_path.is_relative() {
            workspace_dir.as_std_path().join(file_path)
        } else {
            file_path.clone()
        };
        if absolute_path.starts_with(&bench_dir_abs) {
            if let Ok(entities) = cache.get_entities(&absolute_path) {
                bench_file_entities.insert(absolute_path, entities.clone());
            }
        }
    }

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
            if let Some(entities) = bench_file_entities.get(&absolute_path) {
                // Exclude files that contain testbench entities other than the one we're running
                for entity in entities {
                    if entity.to_lowercase().ends_with("_tb")
                        && entity != &testbench_name
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
    let mut referenced_files = find_referenced_files(
        &testbench_file,
        &filtered_defaultlib_files,
        &mut cache,
    )?;

    // Sort files in dependency order (dependencies first)
    sort_files_by_dependencies(
        &mut processor,
        &mut referenced_files,
        &mut cache,
    )?;

    let mut files: Vec<String> = referenced_files
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();

    files.push(testbench_file.to_string_lossy().to_string());

    run_nvc_analysis(vhdl_std, BUILD_DIR, "work", &files, false).await?;

    run_nvc_elab(vhdl_std, BUILD_DIR, "work", &testbench_name, false).await?;

    // Build Rust library if requested
    let rust_lib_path = if build_rust {
        Some(
            build_rust_library(&bench_dir, &testbench_file)
                .await?
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };

    // Run NVC simulation
    run_nvc_sim(
        vhdl_std,
        BUILD_DIR,
        "work",
        &testbench_name,
        rust_lib_path,
        &runtime_flags.to_vec(),
        false,
    )
    .await?;

    Ok(())
}

pub fn find_referenced_files(
    testbench_file: &Path,
    available_files: &[PathBuf],
    cache: &mut FileCache,
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

        let dependencies = cache.get_dependencies(&current_file)?.clone();

        // Find corresponding files for each dependency
        for dep in dependencies {
            for available_file in available_files {
                if file_provides_symbol(available_file, &dep, cache)? {
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

pub fn sort_files_by_dependencies(
    processor: &mut RecordProcessor,
    files: &mut Vec<PathBuf>,
    cache: &mut FileCache,
) -> Result<()> {
    // Build dependency graph
    let mut dependencies: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut all_symbols: HashMap<String, PathBuf> = HashMap::new();

    // First pass: collect all symbols provided by each file
    for file in files.iter() {
        let symbols = analyze_file(processor, file)?;
        for symbol in symbols {
            match symbol {
                VwSymbol::Package(name) => {
                    all_symbols.insert(name.clone(), file.clone());
                    let entry = processor
                        .file_info
                        .entry(file.to_string_lossy().to_string())
                        .or_default();
                    entry.add_defined_pkg(&name);

                    // Use cache to get package imports only
                    let deps = cache.get_dependencies(file)?;
                    for dep in deps {
                        if let VwSymbol::Package(pkg_name) = dep {
                            entry.add_imported_pkg(pkg_name);
                        }
                    }
                }
                VwSymbol::Entity(name) => {
                    all_symbols.insert(name, file.clone());
                }
                _ => {}
            }
        }
    }

    // Second pass: find dependencies for each file
    for file in files.iter() {
        let deps = cache.get_dependencies(file)?.clone();
        let mut file_deps = Vec::new();

        for dep in deps {
            let dep_name = match &dep {
                VwSymbol::Package(name) | VwSymbol::Entity(name) => name,
                _ => continue,
            };
            if let Some(provider_file) = all_symbols.get(dep_name) {
                if provider_file != file {
                    file_deps.push(provider_file.clone());
                }
            }
        }

        dependencies.insert(file.clone(), file_deps);
    }

    // Topological sort using Kahn's algorithm
    let sorted = topological_sort_files(files.clone(), dependencies)?;
    *files = sorted;

    Ok(())
}

pub fn load_existing_vhdl_ls_config(
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

// ============================================================================
// Internal Helper Functions
// ============================================================================

fn get_package_imports(content: &str) -> Result<Vec<String>> {
    // Find 'use work.package_name' statements
    let use_work_pattern = r"(?i)use\s+work\.(\w+)";
    let use_work_re = regex::Regex::new(use_work_pattern)?;
    let mut imports = Vec::new();

    for captures in use_work_re.captures_iter(content) {
        if let Some(package_name) = captures.get(1) {
            imports.push(package_name.as_str().to_string());
        }
    }
    Ok(imports)
}

fn file_provides_symbol(
    file_path: &Path,
    needed: &VwSymbol,
    cache: &mut FileCache,
) -> Result<bool> {
    let provided = cache.get_provided_symbols(file_path)?;
    Ok(provided.iter().any(|s| match (needed, s) {
        // Package dependency matches package declaration
        (VwSymbol::Package(need), VwSymbol::Package(have)) => {
            need.eq_ignore_ascii_case(have)
        }
        // Entity dependency matches entity declaration
        (VwSymbol::Entity(need), VwSymbol::Entity(have)) => {
            need.eq_ignore_ascii_case(have)
        }
        _ => false,
    }))
}

fn analyze_file(
    processor: &mut RecordProcessor,
    file: &Path,
) -> Result<Vec<VwSymbol>> {
    let parser = VHDLParser::new(processor.vhdl_std.into());
    let mut diagnostics = Vec::new();
    let (_, design_file) = parser.parse_design_file(file, &mut diagnostics)?;

    let mut file_finder = VwSymbolFinder::new(&processor.target_attr);
    walk_design_file(&mut file_finder, &design_file);

    let file_str = file.to_string_lossy().to_string();

    // Add records to symbols map
    for record in file_finder.get_records() {
        let name = record.get_name().to_string();
        processor
            .symbols
            .insert(name.clone(), VwSymbol::Record(record.clone()));
        processor.symbol_to_file.insert(name, file_str.clone());
    }

    // Add enums from symbols (they're already VwSymbol::Enum)
    for symbol in file_finder.get_symbols() {
        if let VwSymbol::Enum(enum_data) = symbol {
            let name = enum_data.get_name().to_string();
            processor.symbols.insert(name.clone(), symbol.clone());
            processor.symbol_to_file.insert(name, file_str.clone());
        }
    }

    for tagged_type in file_finder.get_tagged_types() {
        processor.tagged_names.insert(tagged_type.clone());
    }

    Ok(file_finder.get_symbols().clone())
}

fn topological_sort_files(
    files: Vec<PathBuf>,
    dependencies: HashMap<PathBuf, Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let mut dep_graph: DiGraph<PathBuf, ()> = DiGraph::default();
    let mut index_map: HashMap<PathBuf, NodeIndex> = HashMap::new();

    // initialize the nodes
    for file in &files {
        let index = dep_graph.add_node(file.clone());
        index_map.insert(file.clone(), index);
    }

    // now add edges from files to their dependencies
    for (file, deps) in &dependencies {
        let source_node = index_map.get(file).ok_or(VwError::Dependency {
            message: format!(
                "Index map somehow didn't contain file {:?}",
                file
            ),
        })?;
        // file depends on every dep in deps
        for dep in deps {
            let dst_node = index_map.get(dep).ok_or(VwError::Dependency {
                message: format!(
                    "Index map somehow didn't contain dep {:?}",
                    dep
                ),
            })?;
            dep_graph.add_edge(*source_node, *dst_node, ());
        }
    }

    // ok now topological sort
    let ordered_files =
        toposort(&dep_graph, None).map_err(|_| VwError::Dependency {
            message: "Got circular dependency".to_string(),
        })?;

    let result: Vec<PathBuf> = ordered_files
        .iter()
        .map(|&idx| dep_graph[idx].clone())
        .rev()
        .collect();
    Ok(result)
}

fn find_testbench_file_recurse(
    testbench_name: &str,
    bench_dir: &Utf8Path,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<Vec<PathBuf>> {
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
                    if file_contains_entity(
                        &path,
                        testbench_name,
                        entities_cache,
                    )? {
                        found_files.push(path);
                    }
                }
            }
        } else if recurse {
            let dir_path: Utf8PathBuf =
                path.try_into().map_err(|e| VwError::FileSystem {
                    message: format!("Failed to get dir path: {e}"),
                })?;
            let mut lower_testbenches = find_testbench_file_recurse(
                testbench_name,
                &dir_path,
                recurse,
                entities_cache,
            )?;
            found_files.append(&mut lower_testbenches);
        }
    }
    Ok(found_files)
}

fn find_testbench_file(
    testbench_name: &str,
    bench_dir: &Utf8Path,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<PathBuf> {
    let found_files = find_testbench_file_recurse(
        testbench_name,
        bench_dir,
        recurse,
        entities_cache,
    )?;

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

fn file_contains_entity(
    file_path: &Path,
    entity_name: &str,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<bool> {
    let entities = get_cached_entities(file_path, entities_cache)?;
    Ok(entities.iter().any(|e| e.eq_ignore_ascii_case(entity_name)))
}

/// Get entities from cache, parsing and caching if not present.
fn get_cached_entities<'a>(
    path: &Path,
    entities_cache: &'a mut HashMap<PathBuf, Vec<String>>,
) -> Result<&'a Vec<String>> {
    match entities_cache.entry(path.to_path_buf()) {
        Entry::Occupied(e) => Ok(e.into_mut()),
        Entry::Vacant(e) => {
            let content =
                fs::read_to_string(path).map_err(|e| VwError::FileSystem {
                    message: format!("Failed to read file {path:?}: {e}"),
                })?;
            let entities = parse_entities(&content)?;
            Ok(e.insert(entities))
        }
    }
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

    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            // Create a temporary directory for the operation
            let temp_dir =
                tempfile::tempdir().map_err(|e| VwError::FileSystem {
                    message: format!(
                        "Failed to create temporary directory: {e}"
                    ),
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
            let mut remote = repo
                .remote_anonymous(&normalized_repo_url)
                .map_err(|e| VwError::Git {
                    message: format!("Failed to create remote: {e}"),
                })?;

            // Connect and list references
            // Always set a credentials callback so git2 doesn't fail with "no callback set".
            // The callback will try explicit credentials first, then fall back to git's
            // credential helper system (which includes .netrc support).
            let mut callbacks = git2::RemoteCallbacks::new();
            let attempt_count = RefCell::new(0);

            callbacks.credentials(
                move |url, username_from_url, allowed_types| {
                    let mut attempts = attempt_count.borrow_mut();
                    *attempts += 1;

                    // Limit attempts to prevent infinite loops
                    if *attempts > 1 {
                        return git2::Cred::default();
                    }

                    // First, try explicit credentials from netrc if available
                    if allowed_types
                        .contains(git2::CredentialType::USER_PASS_PLAINTEXT)
                    {
                        if let Some((ref username, ref password)) = credentials
                        {
                            // Use both username and password from netrc
                            return git2::Cred::userpass_plaintext(
                                username, password,
                            );
                        }
                    }

                    // Try SSH key if available
                    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                        if let Some(username) = username_from_url {
                            if let Ok(cred) =
                                git2::Cred::ssh_key_from_agent(username)
                            {
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
                },
            );

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
        }),
    )
    .await
    .map_err(|_| VwError::Git {
        message: "Git ls-remote timed out after 30 seconds".to_string(),
    })?
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git ls-remote task: {e}"),
    })?
}

#[allow(clippy::too_many_arguments)]
async fn download_dependency(
    repo_url: &str,
    commit: &str,
    src_paths: &[String],
    dest_path: &Path,
    recursive: bool,
    exclude: &[String],
    submodules: bool,
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
    let src_paths = src_paths.to_vec();
    let credentials = credentials.map(|(u, p)| (u.to_string(), p.to_string()));

    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::task::spawn_blocking(move || {
            // Set up clone options with authentication
            let mut builder = git2::build::RepoBuilder::new();

            // Always set a credentials callback so git2 doesn't fail with "no callback set".
            // The callback will try explicit credentials first, then fall back to git's
            // credential helper system (which includes .netrc support).
            let mut callbacks = git2::RemoteCallbacks::new();
            let attempt_count = RefCell::new(0);

            callbacks.credentials(
                move |url, username_from_url, allowed_types| {
                    let mut attempts = attempt_count.borrow_mut();
                    *attempts += 1;

                    // Limit attempts to prevent infinite loops
                    if *attempts > 1 {
                        return git2::Cred::default();
                    }

                    // First, try explicit credentials from netrc if available
                    if allowed_types
                        .contains(git2::CredentialType::USER_PASS_PLAINTEXT)
                    {
                        if let Some((ref username, ref password)) = credentials
                        {
                            // Use both username and password from netrc
                            return git2::Cred::userpass_plaintext(
                                username, password,
                            );
                        }
                    }

                    // Try SSH key if available
                    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                        if let Some(username) = username_from_url {
                            if let Ok(cred) =
                                git2::Cred::ssh_key_from_agent(username)
                            {
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
                },
            );

            let mut fetch_options = git2::FetchOptions::new();
            fetch_options.depth(1); // shallow clone — only need one commit
            fetch_options.remote_callbacks(callbacks);
            builder.fetch_options(fetch_options);

            // Clone the repository
            let repo = builder
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
                    message: format!(
                        "Failed to checkout commit '{commit}': {e}"
                    ),
                })?;

            // Set HEAD to the commit
            repo.set_head_detached(commit_oid)
                .map_err(|e| VwError::Git {
                    message: format!(
                        "Failed to set HEAD to commit '{commit}': {e}"
                    ),
                })?;

            // Initialize and update submodules if requested
            if submodules {
                for mut submodule in
                    repo.submodules().map_err(|e| VwError::Git {
                        message: format!("Failed to list submodules: {e}"),
                    })?
                {
                    submodule.init(false).map_err(|e| VwError::Git {
                        message: format!(
                            "Failed to init submodule '{}': {e}",
                            submodule.name().unwrap_or("unknown")
                        ),
                    })?;
                    submodule.update(true, None).map_err(|e| VwError::Git {
                        message: format!(
                            "Failed to update submodule '{}': {e}",
                            submodule.name().unwrap_or("unknown")
                        ),
                    })?;
                }
            }

            Ok::<(), VwError>(())
        }),
    )
    .await
    .map_err(|_| VwError::Git {
        message: "Git clone timed out after 120 seconds".to_string(),
    })?
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git operations: {e}"),
    })??;

    fs::create_dir_all(dest_path).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create destination directory: {e}"),
    })?;

    // Treat all src values as globs (handles files, directories, and patterns)
    for src_path in &src_paths {
        copy_vhdl_files_glob(
            temp_dir.path(),
            src_path,
            dest_path,
            recursive,
            exclude,
        )?;
    }

    Ok(())
}

fn copy_vhdl_files_glob(
    repo_root: &Path,
    src_pattern: &str,
    dest: &Path,
    recursive: bool,
    exclude: &[String],
) -> Result<()> {
    // Build patterns to match
    let src_path = repo_root.join(src_pattern);
    let mut patterns = Vec::new();
    let strip_prefix: PathBuf;

    // Compile exclude patterns
    let exclude_patterns: Vec<glob::Pattern> = exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

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

                        // Check if file matches any exclude pattern
                        let path_str = relative_path.to_string_lossy();
                        if exclude_patterns.iter().any(|p| p.matches(&path_str))
                        {
                            continue; // Skip excluded files
                        }

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

fn find_vhdl_files(
    dir: &Path,
    recursive: bool,
    exclude: &[String],
) -> Result<Vec<PathBuf>> {
    let mut vhdl_files = Vec::new();
    find_vhdl_files_impl(dir, &mut vhdl_files, recursive)?;

    // Filter out excluded files
    if !exclude.is_empty() {
        let exclude_patterns: Vec<glob::Pattern> = exclude
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        vhdl_files.retain(|file| {
            // Match against path relative to the base directory
            let relative = file.strip_prefix(dir).unwrap_or(file);
            let path_str = relative.to_string_lossy();
            !exclude_patterns
                .iter()
                .any(|pattern| pattern.matches(&path_str))
        });
    }

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

/// Build a Rust library for a testbench.
/// Looks for Cargo.toml in the testbench directory, builds it, and returns the path to the .so file.
async fn build_rust_library(
    bench_dir: &Utf8Path,
    testbench_file: &Path,
) -> Result<PathBuf> {
    // Get the testbench directory
    let testbench_dir =
        testbench_file.parent().ok_or_else(|| VwError::Testbench {
            message: format!(
                "Testbench file {:?} has no parent directory???",
                testbench_file
            ),
        })?;

    // Look for Cargo.toml in the testbench directory
    let cargo_toml_path = testbench_dir.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        return Err(VwError::Testbench {
            message: format!(
                "Cargo.toml not found in testbench directory: {:?}",
                testbench_dir
            ),
        });
    }

    // Parse Cargo.toml to get the package name
    let cargo_toml_content =
        fs::read_to_string(&cargo_toml_path).map_err(|e| {
            VwError::FileSystem {
                message: format!("Failed to read Cargo.toml: {e}"),
            }
        })?;

    let cargo_toml: CargoToml = toml::from_str(&cargo_toml_content)?;
    let package_name = cargo_toml.package.name;

    // Run cargo build in the testbench directory
    let testbench_dir_owned = testbench_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(&testbench_dir_owned)
            .output()
            .map_err(|e| VwError::Testbench {
                message: format!("Failed to execute cargo build: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VwError::Testbench {
                message: format!("cargo build failed:\n{stderr}"),
            });
        }

        Ok::<(), VwError>(())
    })
    .await
    .map_err(|e| VwError::Testbench {
        message: format!("Failed to execute cargo build task: {e}"),
    })??;

    // Find the .so file in the workspace target directory (parent of testbench dir)
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let lib_name = format!("lib{}.{ext}", package_name.replace('-', "_"));
    let workspace_target = bench_dir.join("target").join("debug");

    let lib_path = workspace_target.join(&lib_name);

    if !lib_path.exists() {
        return Err(VwError::Testbench {
            message: format!(
                "Built Rust library not found at expected path: {:?}",
                lib_path
            ),
        });
    }

    Ok(lib_path.into())
}
