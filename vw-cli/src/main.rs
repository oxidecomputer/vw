// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use std::fmt;
use std::process;

use vw_lib::{
    add_dependency_with_token, clear_cache, extract_hostname_from_repo_url,
    generate_deps_tcl, get_access_credentials_from_netrc, init_workspace,
    list_dependencies, list_testbenches, load_workspace_config,
    remove_dependency, run_testbench, update_workspace_with_token, Credentials,
    VersionInfo, VhdlStandard,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliVhdlStandard {
    #[value(name = "2008")]
    Vhdl2008,
    #[value(name = "2019")]
    Vhdl2019,
}

impl fmt::Display for CliVhdlStandard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliVhdlStandard::Vhdl2008 => write!(f, "2008"),
            CliVhdlStandard::Vhdl2019 => write!(f, "2019"),
        }
    }
}

impl From<CliVhdlStandard> for VhdlStandard {
    fn from(std: CliVhdlStandard) -> Self {
        match std {
            CliVhdlStandard::Vhdl2008 => VhdlStandard::Vhdl2008,
            CliVhdlStandard::Vhdl2019 => VhdlStandard::Vhdl2019,
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
        #[arg(
            long,
            help = "Recursively include VHDL files from subdirectories"
        )]
        recursive: bool,
        #[arg(long, help = "Mark as simulation-only (excluded from deps.tcl)")]
        sim_only: bool,
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
    #[command(about = "Generate deps.tcl file with all dependency VHDL files")]
    DepsToTcl,
    #[command(about = "Run testbench using NVC")]
    Test {
        #[arg(help = "Name of the testbench entity to run")]
        testbench: Option<String>,
        #[arg(long, help = "VHDL standard", default_value_t = CliVhdlStandard::Vhdl2019)]
        std: CliVhdlStandard,
        #[arg(long, help = "List all available testbenches")]
        list: bool,
    },
}

/// Helper function to get access credentials for a repository URL from netrc if available
async fn get_access_credentials_for_repo(
    repo_url: &str,
) -> Option<Credentials> {
    if let Ok(hostname) = extract_hostname_from_repo_url(repo_url) {
        if let Ok(Some(creds)) = get_access_credentials_from_netrc(&hostname) {
            return Some(creds);
        }
    }
    None
}

/// Helper function to get access credentials for workspace dependencies from netrc
async fn get_access_credentials_for_workspace(
    workspace_dir: &camino::Utf8Path,
) -> Option<Credentials> {
    // Load workspace config and check if any dependencies might need authentication
    if let Ok(config) = load_workspace_config(workspace_dir) {
        for dep in config.dependencies.values() {
            if let Some(creds) =
                get_access_credentials_for_repo(&dep.repo).await
            {
                return Some(creds);
            }
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Get current working directory
    let cwd =
        Utf8PathBuf::try_from(std::env::current_dir().unwrap_or_else(|e| {
            eprintln!(
                "{} Failed to get current directory: {e}",
                "error:".bright_red()
            );
            process::exit(1);
        }))
        .unwrap_or_else(|e| {
            eprintln!(
                "{} Current directory path is not valid UTF-8: {e}",
                "error:".bright_red()
            );
            process::exit(1);
        });

    match cli.command {
        Commands::Init { name } => {
            if let Err(e) = init_workspace(&cwd, name.clone()) {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
            println!(
                "{} Initialized workspace: {}",
                "✓".bright_green(),
                name.cyan()
            );
        }
        Commands::Update => {
            let access_creds = get_access_credentials_for_workspace(&cwd).await;
            match update_workspace_with_token(&cwd, access_creds).await {
                Ok(result) => {
                    for dep in result.dependencies {
                        println!("Processing dependency: {}", dep.name.cyan());
                        if dep.was_cached {
                            println!(
                                "Using cached version of {} at {}",
                                dep.name.cyan(),
                                dep.commit.cyan()
                            );
                        } else {
                            println!(
                                "Downloaded {} at {}",
                                dep.name.cyan(),
                                dep.commit.cyan()
                            );
                        }
                    }
                    println!(
                        "{} Workspace updated successfully!",
                        "✓".bright_green()
                    );
                }
                Err(e) => {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        }
        Commands::Add {
            repo,
            branch,
            commit,
            src,
            name,
            recursive,
            sim_only,
        } => {
            let access_creds = get_access_credentials_for_repo(&repo).await;
            match add_dependency_with_token(
                &cwd,
                repo.clone(),
                branch,
                commit,
                src,
                name.clone(),
                recursive,
                sim_only,
                access_creds,
            )
            .await
            {
                Ok(()) => {
                    let dep_name = name.unwrap_or_else(|| {
                        repo.trim_end_matches(".git")
                            .split('/')
                            .next_back()
                            .unwrap_or("dependency")
                            .to_string()
                    });
                    println!("Added dependency: {}", dep_name.cyan());
                    println!(
                        "Run {} to download and configure",
                        "vw update".cyan()
                    );
                }
                Err(e) => {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        }
        Commands::Remove { name } => {
            match remove_dependency(&cwd, name.clone()) {
                Ok(()) => {
                    println!("Removed dependency: {}", name.cyan());
                    println!(
                        "Run {} to update configuration",
                        "vw update".cyan()
                    );
                }
                Err(e) => {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        }
        Commands::Clear => match clear_cache(&cwd) {
            Ok(cleared) => {
                if !cleared.is_empty() {
                    for dep in &cleared {
                        println!("Removing cached dependency: {}", dep.cyan());
                    }
                    println!(
                        "{} Cleared {} cached repositories",
                        "✓".bright_green(),
                        cleared.len()
                    );
                } else {
                    println!("No cached repositories found to clear");
                }
            }
            Err(e) => {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        },
        Commands::List => match list_dependencies(&cwd) {
            Ok(deps) => {
                if deps.is_empty() {
                    println!("No dependencies found in workspace");
                } else {
                    println!("Dependencies:");
                    for dep in deps {
                        let version_info = match dep.version {
                            VersionInfo::Branch { branch } => {
                                format!(" (branch: {branch})")
                            }
                            VersionInfo::Commit { commit } => {
                                format!(" ({})", &commit[..8.min(commit.len())])
                            }
                            VersionInfo::Locked { commit } => {
                                format!(" ({})", &commit[..8.min(commit.len())])
                            }
                            VersionInfo::Unknown => String::new(),
                        };

                        println!(
                            "  {} - {}{}",
                            dep.name.cyan(),
                            dep.repo,
                            version_info.bright_black()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        },
        Commands::DepsToTcl => match generate_deps_tcl(&cwd) {
            Ok(()) => {
                println!(
                    "{} Generated deps.tcl with dependency VHDL files",
                    "✓".bright_green()
                );
            }
            Err(e) => {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        },
        Commands::Test {
            testbench,
            std,
            list,
        } => {
            if list {
                match list_testbenches(&cwd) {
                    Ok(testbenches) => {
                        if testbenches.is_empty() {
                            println!("No testbenches found in bench directory");
                        } else {
                            println!("Available testbenches:");
                            for tb in testbenches {
                                println!(
                                    "  {} - {}",
                                    tb.name.cyan(),
                                    tb.path
                                        .display()
                                        .to_string()
                                        .bright_black()
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                }
            } else if let Some(testbench_name) = testbench {
                println!("Running testbench: {}", testbench_name.cyan());
                match run_testbench(&cwd, testbench_name.clone(), std.into())
                    .await
                {
                    Ok(()) => {
                        println!(
                            "{} Testbench '{}' completed successfully!",
                            "✓".bright_green(),
                            testbench_name
                        );
                        println!(
                            "Waveform saved to: {}",
                            format!("{testbench_name}.fst").cyan()
                        );
                    }
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                }
            } else {
                eprintln!(
                    "{} Must specify testbench name or use --list",
                    "error:".bright_red()
                );
                process::exit(1);
            }
        }
    }
}
