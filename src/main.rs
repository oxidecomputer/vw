// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum VhdlStandard {
    #[value(name = "2008")]
    Vhdl2008,
    #[value(name = "2019")]
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

#[derive(Parser)]
#[command(name = "vw")]
#[command(about = "A VHDL workspace management tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Initialize a new workspace")]
    Init {
        #[arg(help = "Workspace name")]
        name: String,
    },
    #[command(about = "Update workspace dependencies")]
    Update,
    #[command(about = "Add a new dependency")]
    Add {
        #[arg(help = "Git repository URL")]
        repo: String,
        #[arg(long, help = "Branch name", conflicts_with = "commit")]
        branch: Option<String>,
        #[arg(long, help = "Commit hash", conflicts_with = "branch")]
        commit: Option<String>,
        #[arg(long, help = "Source path within the repository")]
        src: Option<String>,
        #[arg(long, help = "Dependency name (defaults to repository name)")]
        name: Option<String>,
    },
    #[command(about = "Remove a dependency")]
    Remove {
        #[arg(help = "Name of the dependency to remove")]
        name: String,
    },
    #[command(about = "Clear all cached repositories")]
    Clear,
    #[command(about = "List workspace dependencies")]
    List,
    #[command(about = "Run testbench using NVC")]
    Test {
        #[arg(help = "Name of the testbench entity to run")]
        testbench: Option<String>,
        #[arg(long, help = "VHDL standard", default_value_t = VhdlStandard::Vhdl2019)]
        std: VhdlStandard,
        #[arg(long, help = "List all available testbenches")]
        list: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkspaceConfig {
    #[allow(dead_code)]
    workspace: WorkspaceInfo,
    dependencies: HashMap<String, Dependency>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkspaceInfo {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    version: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Dependency {
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    src: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    dependencies: HashMap<String, LockedDependency>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockedDependency {
    repo: String,
    commit: String,
    src: String,
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct VhdlLsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    standard: Option<String>,
    libraries: HashMap<String, VhdlLsLibrary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lint: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct VhdlLsLibrary {
    files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_third_party: Option<bool>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => {
            if let Err(e) = init_workspace(name) {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Update => {
            if let Err(e) = update_workspace().await {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Add {
            repo,
            branch,
            commit,
            src,
            name,
        } => {
            if let Err(e) =
                add_dependency(repo, branch, commit, src, name).await
            {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Remove { name } => {
            if let Err(e) = remove_dependency(name) {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Clear => {
            if let Err(e) = clear_cache() {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::List => {
            if let Err(e) = list_dependencies() {
                eprintln!("{} {e}", "Error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Test {
            testbench,
            std,
            list,
        } => {
            if list {
                if let Err(e) = list_testbenches() {
                    eprintln!("{} {e}", "Error:".bright_red());
                    process::exit(1);
                }
            } else if let Some(testbench_name) = testbench {
                if let Err(e) = run_testbench(testbench_name, std).await {
                    eprintln!("{} {e}", "Error:".bright_red());
                    process::exit(1);
                }
            } else {
                eprintln!(
                    "{} Must specify testbench name or use --list",
                    "Error:".bright_red()
                );
                process::exit(1);
            }
        }
    }
}

async fn update_workspace() -> Result<()> {
    let config = load_workspace_config()?;
    let deps_dir = get_deps_directory()?;

    let mut lock_file = LockFile {
        dependencies: HashMap::new(),
    };

    let mut vhdl_ls_config = VhdlLsConfig {
        standard: None,
        libraries: HashMap::new(),
        lint: None,
    };

    for (name, dep) in &config.dependencies {
        println!("Processing dependency: {}", name.cyan());

        let commit_sha =
            resolve_dependency_commit(&dep.repo, &dep.branch, &dep.commit)
                .await
                .with_context(|| {
                    format!("Failed to resolve commit for dependency '{name}'")
                })?;

        let dep_path = deps_dir.join(format!("{name}-{commit_sha}"));

        if !dep_path.exists() {
            println!("Downloading {} at {}", name.cyan(), commit_sha.cyan());
            download_dependency(&dep.repo, &commit_sha, &dep.src, &dep_path)
                .await
                .with_context(|| {
                    format!("Failed to download dependency '{name}'")
                })?;
        } else {
            println!(
                "Using cached version of {} at {}",
                name.cyan(),
                commit_sha.cyan()
            );
        }

        lock_file.dependencies.insert(
            name.clone(),
            LockedDependency {
                repo: dep.repo.clone(),
                commit: commit_sha.clone(),
                src: dep.src.clone(),
                path: dep_path.clone(),
            },
        );

        // Find VHDL files in the cached dependency directory
        let vhdl_files = find_vhdl_files(&dep_path)?;
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

    write_lock_file(&lock_file)?;
    write_vhdl_ls_config(&vhdl_ls_config)?;

    println!("{} Workspace updated successfully!", "✓".bright_green());
    Ok(())
}

fn init_workspace(name: String) -> Result<()> {
    let config_path = Path::new("vw.toml");
    if config_path.exists() {
        return Err(anyhow!(
            "{} vw.toml already exists in current directory",
            "✗".bright_red()
        ));
    }

    let config = WorkspaceConfig {
        workspace: WorkspaceInfo {
            name: name.clone(),
            version: "0.1.0".to_string(),
        },
        dependencies: HashMap::new(),
    };

    save_workspace_config(&config)?;

    println!(
        "{} Initialized workspace: {}",
        "✓".bright_green(),
        name.cyan()
    );

    Ok(())
}

async fn add_dependency(
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
) -> Result<()> {
    let mut config =
        load_workspace_config().unwrap_or_else(|_| WorkspaceConfig {
            workspace: WorkspaceInfo {
                name: "workspace".to_string(),
                version: "0.1.0".to_string(),
            },
            dependencies: HashMap::new(),
        });

    // Validate that either branch or commit is provided
    if branch.is_none() && commit.is_none() {
        return Err(anyhow!(
            "{} Must specify either --branch or --commit",
            "✗".bright_red()
        ));
    }

    let dep_name = name.unwrap_or_else(|| extract_repo_name(&repo));
    let src_path = src.unwrap_or_else(|| ".".to_string());

    let dependency = Dependency {
        repo: repo.clone(),
        branch,
        commit,
        src: src_path,
    };

    config.dependencies.insert(dep_name.clone(), dependency);

    save_workspace_config(&config)?;

    println!("Added dependency: {}", dep_name.cyan());
    println!("Run {} to download and configure", "vw update".cyan());

    Ok(())
}

fn remove_dependency(name: String) -> Result<()> {
    let mut config = load_workspace_config()?;

    if config.dependencies.remove(&name).is_some() {
        save_workspace_config(&config)?;
        println!("Removed dependency: {}", name.cyan());
        println!("Run {} to update configuration", "vw update".cyan());
    } else {
        return Err(anyhow!(
            "{} Dependency '{}' not found",
            "✗".bright_red(),
            name
        ));
    }

    Ok(())
}

fn clear_cache() -> Result<()> {
    let config = load_workspace_config()?;
    let deps_dir = get_deps_directory()?;

    let mut cleared_count = 0;

    // Get all dependencies from the current workspace
    for name in config.dependencies.keys() {
        if let Ok(entries) = fs::read_dir(&deps_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if let Some(file_name_str) = file_name.to_str() {
                    if file_name_str.starts_with(&format!("{name}-")) {
                        let dep_path = entry.path();
                        if dep_path.is_dir() {
                            println!(
                                "Removing cached dependency: {}",
                                file_name_str.cyan()
                            );
                            fs::remove_dir_all(&dep_path)
                                .with_context(|| format!("Failed to remove cached dependency at {dep_path:?}"))?;
                            cleared_count += 1;
                        }
                    }
                }
            }
        }
    }

    if cleared_count > 0 {
        println!(
            "{} Cleared {} cached repositories",
            "✓".bright_green(),
            cleared_count
        );
    } else {
        println!("No cached repositories found to clear");
    }

    Ok(())
}

fn list_dependencies() -> Result<()> {
    let config = load_workspace_config()?;
    if config.dependencies.is_empty() {
        println!("No dependencies found in workspace");
        return Ok(());
    }

    // Try to load lock file to get resolved versions
    let lock_file = load_lock_file().ok();

    println!("Dependencies:");
    for (name, dep) in &config.dependencies {
        let version_info = match &lock_file {
            Some(lock) => {
                if let Some(locked_dep) = lock.dependencies.get(name) {
                    format!(" ({})", &locked_dep.commit[..8])
                } else {
                    // Not yet resolved, show branch/commit from config
                    match (&dep.branch, &dep.commit) {
                        (Some(branch), None) => format!(" (branch: {branch})"),
                        (None, Some(commit)) => format!(" ({})", &commit[..8]),
                        _ => String::new(),
                    }
                }
            }
            None => {
                // No lock file, show branch/commit from config
                match (&dep.branch, &dep.commit) {
                    (Some(branch), None) => format!(" (branch: {branch})"),
                    (None, Some(commit)) => format!(" ({})", &commit[..8]),
                    _ => String::new(),
                }
            }
        };

        println!(
            "  {} - {}{}",
            name.cyan(),
            dep.repo,
            version_info.bright_black()
        );
    }

    Ok(())
}

fn list_testbenches() -> Result<()> {
    let bench_dir = Path::new("bench");
    if !bench_dir.exists() {
        println!("No 'bench' directory found in current workspace");
        return Ok(());
    }

    let mut testbenches = Vec::new();

    for entry in
        fs::read_dir(bench_dir).context("Failed to read bench directory")?
    {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "vhd" || extension == "vhdl" {
                    let entities = find_entities_in_file(&path)?;
                    for entity in entities {
                        testbenches.push((entity, path.clone()));
                    }
                }
            }
        }
    }

    if testbenches.is_empty() {
        println!("No testbenches found in bench directory");
    } else {
        println!("Available testbenches:");
        for (entity, file_path) in &testbenches {
            println!(
                "  {} - {}",
                entity.cyan(),
                file_path.display().to_string().bright_black()
            );
        }
    }

    Ok(())
}

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
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {file_path:?}"))?;

    let mut dependencies = HashSet::new();

    // Find 'use work.package_name' statements
    let use_work_pattern = r"(?i)use\s+work\.(\w+)";
    let use_work_re = regex::Regex::new(use_work_pattern)
        .context("Failed to compile use work regex")?;

    for captures in use_work_re.captures_iter(&content) {
        if let Some(package_name) = captures.get(1) {
            dependencies.insert(package_name.as_str().to_string());
        }
    }

    // Find direct entity instantiations (instance_name: entity work.entity_name)
    let entity_inst_pattern = r"(?i)\w+\s*:\s*entity\s+work\.(\w+)";
    let entity_inst_re = regex::Regex::new(entity_inst_pattern)
        .context("Failed to compile entity instantiation regex")?;

    for captures in entity_inst_re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(1) {
            dependencies.insert(entity_name.as_str().to_string());
        }
    }

    // Find component instantiations (component_name : entity_name)
    let component_pattern = r"(?i)(\w+)\s*:\s*(\w+)";
    let component_re = regex::Regex::new(component_pattern)
        .context("Failed to compile component regex")?;

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
    let comp_decl_re = regex::Regex::new(comp_decl_pattern)
        .context("Failed to compile component declaration regex")?;

    for captures in comp_decl_re.captures_iter(&content) {
        if let Some(comp_name) = captures.get(1) {
            dependencies.insert(comp_name.as_str().to_string());
        }
    }

    Ok(dependencies.into_iter().collect())
}

fn file_provides_symbol(file_path: &Path, symbol: &str) -> Result<bool> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {file_path:?}"))?;

    // Check for package declaration
    let package_pattern =
        format!(r"(?i)\bpackage\s+{}\s+is\b", regex::escape(symbol));
    let package_re = regex::Regex::new(&package_pattern)
        .context("Failed to compile package regex")?;

    if package_re.is_match(&content) {
        return Ok(true);
    }

    // Check for entity declaration
    let entity_pattern =
        format!(r"(?i)\bentity\s+{}\s+is\b", regex::escape(symbol));
    let entity_re = regex::Regex::new(&entity_pattern)
        .context("Failed to compile entity regex")?;

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
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {file_path:?}"))?;

    let mut symbols = Vec::new();

    // Find package declarations
    let package_pattern = r"(?i)\bpackage\s+(\w+)\s+is\b";
    let package_re = regex::Regex::new(package_pattern)
        .context("Failed to compile package regex")?;

    for captures in package_re.captures_iter(&content) {
        if let Some(package_name) = captures.get(1) {
            symbols.push(package_name.as_str().to_string());
        }
    }

    // Find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let entity_re = regex::Regex::new(entity_pattern)
        .context("Failed to compile entity regex")?;

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
    let mut queue = Vec::new();
    let mut result = Vec::new();

    // Add all nodes with in-degree 0 to queue
    for (file, &degree) in &in_degree {
        if degree == 0 {
            queue.push(file.clone());
        }
    }

    while let Some(current) = queue.pop() {
        result.push(current.clone());

        // For each neighbor of current
        if let Some(neighbors) = adj_list.get(&current) {
            for neighbor in neighbors {
                *in_degree.get_mut(neighbor).unwrap() -= 1;
                if in_degree[neighbor] == 0 {
                    queue.push(neighbor.clone());
                }
            }
        }
    }

    // Check for cycles
    if result.len() != files.len() {
        return Err(anyhow!("Circular dependency detected in VHDL files"));
    }

    Ok(result)
}

fn find_entities_in_file(file_path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {file_path:?}"))?;

    let mut entities = Vec::new();

    // Regex to find entity declarations
    let entity_pattern = r"(?i)\bentity\s+(\w+)\s+is\b";
    let re = regex::Regex::new(entity_pattern)
        .context("Failed to compile entity regex")?;

    for captures in re.captures_iter(&content) {
        if let Some(entity_name) = captures.get(1) {
            entities.push(entity_name.as_str().to_string());
        }
    }

    Ok(entities)
}

async fn run_testbench(
    testbench_name: String,
    vhdl_std: VhdlStandard,
) -> Result<()> {
    let vhdl_ls_config = load_existing_vhdl_ls_config()?;

    // First, analyze all non-defaultlib libraries
    for (lib_name, library) in &vhdl_ls_config.libraries {
        if lib_name != "defaultlib" {
            println!("Analyzing library: {}", lib_name.cyan());

            // Convert library name to be NVC-compatible (no hyphens)
            let nvc_lib_name = lib_name.replace('-', "_");

            let mut files = Vec::new();
            for file_path in &library.files {
                // Convert $HOME paths to absolute paths
                let expanded_path = if file_path.starts_with("$HOME") {
                    let home_dir = dirs::home_dir().ok_or_else(|| {
                        anyhow!("Could not determine home directory")
                    })?;
                    home_dir.join(
                        file_path.strip_prefix("$HOME/").unwrap_or(file_path),
                    )
                } else {
                    PathBuf::from(file_path)
                };
                files.push(expanded_path.to_string_lossy().to_string());
            }

            let mut nvc_cmd = tokio::process::Command::new("nvc");
            nvc_cmd
                .arg(format!("--std={vhdl_std}"))
                .arg(format!("--work={nvc_lib_name}"))
                .arg("-M")
                .arg("256m")
                .arg("-a");

            for file in &files {
                nvc_cmd.arg(file);
            }

            let output = nvc_cmd
                .output()
                .await
                .context("Failed to execute NVC analysis")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let cmd_str = format!(
                    "nvc --std={} --work={} -M 256m -a {}",
                    vhdl_std,
                    nvc_lib_name,
                    files.join(" ")
                );
                return Err(anyhow!("NVC analysis failed for library {}:\nCommand: {}\nError: {}", 
                    lib_name, cmd_str, stderr));
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
    let bench_dir = Path::new("bench");
    if !bench_dir.exists() {
        return Err(anyhow!("No 'bench' directory found in current workspace"));
    }

    let testbench_file = find_testbench_file(&testbench_name, bench_dir)?;
    println!(
        "Found testbench: {}",
        testbench_file.display().to_string().cyan()
    );

    // Filter defaultlib files to exclude OTHER testbenches but allow common bench code
    let filtered_defaultlib_files: Vec<PathBuf> = defaultlib_files
        .into_iter()
        .filter(|file_path| {
            // Convert to absolute path for comparison
            let absolute_path = if file_path.is_relative() {
                std::env::current_dir().unwrap_or_default().join(file_path)
            } else {
                file_path.clone()
            };

            // If it's not in the bench directory, include it
            if !absolute_path.starts_with(
                std::env::current_dir().unwrap_or_default().join("bench"),
            ) {
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
    println!("Running testbench: {}", testbench_name.cyan());

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

    let output = nvc_cmd
        .output()
        .await
        .context("Failed to execute NVC simulation")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

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
        return Err(anyhow!(
            "NVC simulation failed:\nCommand: {}\nstdout: {}\nstderr: {}",
            cmd_str,
            stdout,
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");

    println!(
        "{} Testbench '{}' completed successfully!",
        "✓".bright_green(),
        testbench_name
    );
    println!(
        "Waveform saved to: {}",
        format!("{testbench_name}.fst").cyan()
    );

    Ok(())
}

fn find_testbench_file(
    testbench_name: &str,
    bench_dir: &Path,
) -> Result<PathBuf> {
    let mut found_files = Vec::new();

    for entry in
        fs::read_dir(bench_dir).context("Failed to read bench directory")?
    {
        let entry = entry.context("Failed to read directory entry")?;
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
        0 => Err(anyhow!(
            "Testbench entity '{}' not found in bench directory",
            testbench_name
        )),
        1 => Ok(found_files.into_iter().next().unwrap()),
        _ => Err(anyhow!(
            "Multiple files contain entity '{}': {:?}",
            testbench_name,
            found_files
        )),
    }
}

fn file_contains_entity(file_path: &Path, entity_name: &str) -> Result<bool> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {file_path:?}"))?;

    // Simple regex to find entity declarations
    // This is a basic implementation that looks for "entity <name> is"
    let entity_pattern =
        format!(r"(?i)\bentity\s+{}\s+is\b", regex::escape(entity_name));
    let re = regex::Regex::new(&entity_pattern)
        .context("Failed to compile entity regex")?;

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

fn save_workspace_config(config: &WorkspaceConfig) -> Result<()> {
    let toml_content = toml::to_string_pretty(config)
        .context("Failed to serialize workspace config")?;

    fs::write("vw.toml", toml_content)
        .context("Failed to write vw.toml file")?;

    Ok(())
}

fn load_workspace_config() -> Result<WorkspaceConfig> {
    let config_path = Path::new("vw.toml");
    if !config_path.exists() {
        return Err(anyhow!(
            "{} No vw.toml file found in current directory",
            "✗".bright_red()
        ));
    }

    let config_content =
        fs::read_to_string(config_path).context("Failed to read vw.toml")?;

    let config: WorkspaceConfig =
        toml::from_str(&config_content).context("Failed to parse vw.toml")?;

    Ok(config)
}

fn load_lock_file() -> Result<LockFile> {
    let lock_path = Path::new("vw.lock");
    if !lock_path.exists() {
        return Err(anyhow!("No vw.lock file found"));
    }

    let lock_content =
        fs::read_to_string(lock_path).context("Failed to read vw.lock")?;

    let lock_file: LockFile =
        toml::from_str(&lock_content).context("Failed to parse vw.lock")?;

    Ok(lock_file)
}

fn get_deps_directory() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().ok_or_else(|| {
        anyhow!("{} Could not determine home directory", "✗".bright_red())
    })?;

    let deps_dir = home_dir.join(".vw").join("deps");
    fs::create_dir_all(&deps_dir)
        .context("Failed to create dependencies directory")?;

    Ok(deps_dir)
}

async fn resolve_dependency_commit(
    repo_url: &str,
    branch: &Option<String>,
    commit: &Option<String>,
) -> Result<String> {
    match (branch, commit) {
        (Some(_), Some(_)) => Err(anyhow!(
            "{} Cannot specify both branch and commit for dependency",
            "✗".bright_red()
        )),
        (None, None) => Err(anyhow!(
            "{} Must specify either branch or commit for dependency",
            "✗".bright_red()
        )),
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
        .context("Failed to execute git ls-remote")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{} Git ls-remote failed: {}",
            "✗".bright_red(),
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit = stdout.split_whitespace().next().ok_or_else(|| {
        anyhow!("{} Could not parse git ls-remote output", "✗".bright_red())
    })?;

    Ok(commit.to_string())
}

async fn download_dependency(
    repo_url: &str,
    commit: &str,
    src_path: &str,
    dest_path: &Path,
) -> Result<()> {
    let temp_dir =
        tempfile::tempdir().context("Failed to create temporary directory")?;

    let clone_output = tokio::process::Command::new("git")
        .args(["clone", repo_url, temp_dir.path().to_str().unwrap()])
        .output()
        .await
        .context("Failed to execute git clone")?;

    if !clone_output.status.success() {
        let stderr = String::from_utf8_lossy(&clone_output.stderr);
        return Err(anyhow!(
            "{} Git clone failed: {}",
            "✗".bright_red(),
            stderr
        ));
    }

    let checkout_output = tokio::process::Command::new("git")
        .current_dir(temp_dir.path())
        .args(["checkout", commit])
        .output()
        .await
        .context("Failed to execute git checkout")?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        return Err(anyhow!(
            "{} Git checkout failed: {}",
            "✗".bright_red(),
            stderr
        ));
    }

    let src_dir = temp_dir.path().join(src_path);
    if !src_dir.exists() {
        return Err(anyhow!(
            "{} Source path '{}' does not exist in repository",
            "✗".bright_red(),
            src_path
        ));
    }

    fs::create_dir_all(dest_path)
        .context("Failed to create destination directory")?;

    copy_vhdl_files(&src_dir, dest_path)?;

    Ok(())
}

fn copy_vhdl_files(src: &Path, dest: &Path) -> Result<()> {
    for entry in fs::read_dir(src).context("Failed to read source directory")? {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if path.is_dir() {
            let dest_subdir = dest.join(entry.file_name());
            fs::create_dir_all(&dest_subdir)
                .context("Failed to create subdirectory")?;
            copy_vhdl_files(&path, &dest_subdir)?;
        } else if let Some(ext) = path.extension() {
            if ext == "vhd" || ext == "vhdl" {
                let dest_file = dest.join(entry.file_name());
                fs::copy(&path, &dest_file)
                    .with_context(|| format!("Failed to copy file {path:?}"))?;
            }
        }
    }
    Ok(())
}

fn find_vhdl_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut vhdl_files = Vec::new();
    find_vhdl_files_recursive(dir, &mut vhdl_files)?;
    Ok(vhdl_files)
}

fn find_vhdl_files_recursive(
    dir: &Path,
    vhdl_files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(dir).context("Failed to read directory")? {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if path.is_dir() {
            find_vhdl_files_recursive(&path, vhdl_files)?;
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

fn write_lock_file(lock_file: &LockFile) -> Result<()> {
    let toml_content = toml::to_string_pretty(lock_file)
        .context("Failed to serialize lock file")?;

    fs::write("vw.lock", toml_content)
        .context("Failed to write vw.lock file")?;

    Ok(())
}

fn write_vhdl_ls_config(managed_config: &VhdlLsConfig) -> Result<()> {
    let mut existing_config = load_existing_vhdl_ls_config()?;

    // Remove any existing managed dependencies and add the new ones
    for (name, library) in &managed_config.libraries {
        existing_config
            .libraries
            .insert(name.clone(), library.clone());
    }

    let toml_content = toml::to_string_pretty(&existing_config)
        .context("Failed to serialize vhdl_ls.toml")?;

    fs::write("vhdl_ls.toml", toml_content)
        .context("Failed to write vhdl_ls.toml file")?;

    Ok(())
}

fn load_existing_vhdl_ls_config() -> Result<VhdlLsConfig> {
    let config_path = Path::new("vhdl_ls.toml");
    if config_path.exists() {
        let config_content = fs::read_to_string(config_path)
            .context("Failed to read existing vhdl_ls.toml")?;

        let config: VhdlLsConfig = toml::from_str(&config_content)
            .context("Failed to parse existing vhdl_ls.toml")?;

        Ok(config)
    } else {
        Ok(VhdlLsConfig {
            standard: None,
            libraries: HashMap::new(),
            lint: None,
        })
    }
}
