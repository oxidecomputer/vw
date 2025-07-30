// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use colored::*;
use serde::{Deserialize, Serialize};

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
