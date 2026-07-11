// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use std::collections::HashSet;
use std::fmt;
use std::process;

use vw_eda::EdaBackend;
use vw_lib::{
    add_dependency_with_token, clear_cache, extract_hostname_from_repo_url,
    generate_deps_tcl, get_access_credentials_from_netrc, init_workspace,
    list_dependencies, list_testbenches, load_workspace_config,
    remove_dependency, run_testbench, update_workspace_with_token, Credentials,
    VersionInfo, VhdlStandard,
};

mod htcl_test;
mod parallel_load;

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
    #[command(about = "Run VHDL testbench using NVC")]
    Bench {
        #[arg(help = "Name of the testbench entity to run")]
        testbench: Option<String>,
        #[arg(long, help = "VHDL standard", default_value_t = CliVhdlStandard::Vhdl2019)]
        std: CliVhdlStandard,
        #[arg(long, help = "List all available testbenches")]
        list: bool,
        #[arg(
            long,
            help = "Enable recursive search when looking for testbenches"
        )]
        recurse: bool,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Ignore directories matching these names (comma-separated or use multiple times)"
        )]
        ignore: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Runtime flags to pass to NVC (comma-separated or use multiple times)",
            requires = "testbench"
        )]
        runtime_flags: Vec<String>,
        #[arg(
            long,
            help = "Build Rust library for testbench before running",
            requires = "testbench"
        )]
        build_rust: bool,
        #[arg(
            long,
            help = "Generate/regenerate mixed-signal scaffolding from mist.toml",
            requires = "testbench"
        )]
        scaffold: bool,
    },
    #[command(about = "Run an htcl script against a Vivado worker")]
    Run {
        #[arg(help = "Path to an .htcl source file")]
        file: Utf8PathBuf,
        #[arg(
            long,
            help = "Parse and print diagnostics only; don't launch Vivado"
        )]
        check: bool,
        #[arg(
            long,
            value_name = "ID",
            help = "Select a non-default `[[target-parts]]` entry by full \
                    part ID or unique substring (e.g. `--part 3HP`)"
        )]
        part: Option<String>,
        #[arg(
            short,
            long,
            help = "Forward Vivado's banner and info messages to stderr"
        )]
        verbose: bool,
        #[arg(
            long = "info-with-stack",
            help = "Attach the Tcl call stack to INFO messages too \
                    (WARNING / ERROR / CRITICAL always include the stack)"
        )]
        info_with_stack: bool,
    },
    #[command(about = "Launch the vw analyzer LSP server on stdio")]
    Analyzer,
    #[command(
        about = "Interactive htcl REPL backed by a long-lived Vivado worker"
    )]
    Repl {
        #[arg(
            short,
            long,
            help = "Forward Vivado's banner / info chatter to scrollback"
        )]
        verbose: bool,
        #[arg(
            long = "load",
            value_name = "FILE",
            help = "Source FILE into the session as soon as Vivado is up"
        )]
        initial_load: Option<Utf8PathBuf>,
        #[arg(
            long,
            value_name = "ID",
            help = "Select a non-default `[[target-parts]]` entry by full \
                    part ID or unique substring"
        )]
        part: Option<String>,
        #[arg(
            long = "info-with-stack",
            help = "Attach the Tcl call stack to INFO messages too \
                    (WARNING / ERROR / CRITICAL always include the stack)"
        )]
        info_with_stack: bool,
    },
    #[command(about = "Parse and analyze htcl. With no args, discovers the \
                 workspace's module.htcl AND test/*.htcl and checks both")]
    Check {
        #[arg(help = "One or more .htcl source files. Empty → discover \
                    from the workspace root.")]
        files: Vec<Utf8PathBuf>,
        #[arg(
            long,
            value_name = "ID",
            conflicts_with = "all_parts",
            help = "Check against a specific `[[target-parts]]` entry by \
                    full part ID or unique substring. Default: the workspace's \
                    default part."
        )]
        part: Option<String>,
        #[arg(
            long = "all-parts",
            conflicts_with = "part",
            help = "Check against every declared `[[target-parts]]` entry \
                    instead of just the default"
        )]
        all_parts: bool,
    },
    #[command(about = "Run htcl-level tests (@test procs under test/)")]
    Test {
        #[arg(help = "Substring filter — run only tests whose name matches")]
        filter: Option<String>,
        #[arg(long, help = "List discovered tests without running them")]
        list: bool,
        #[arg(
            long,
            help = "Max concurrent dedicated-eda Vivado processes",
            default_value_t = 2
        )]
        test_threads: usize,
        #[arg(
            long,
            value_name = "ID",
            help = "Select the workspace-default `[[target-parts]]` entry \
                    for tests without their own `@test(target=…)`; matches \
                    by full ID or unique substring"
        )]
        part: Option<String>,
        #[arg(
            short,
            long,
            help = "Forward Vivado banner/info to stderr during test evals"
        )]
        verbose: bool,
        #[arg(
            long = "info-with-stack",
            help = "Attach the Tcl call stack to INFO messages too"
        )]
        info_with_stack: bool,
    },
    #[command(subcommand, about = "IP-XACT tooling")]
    Ip(IpCommand),
    #[command(
        subcommand,
        name = "htcl-cmd",
        about = "Generate htcl wrappers from Vivado command references"
    )]
    HtclCmd(HtclCmdCommand),
}

#[derive(Subcommand)]
enum HtclCmdCommand {
    #[command(
        about = "Generate an htcl wrapper from a Vivado man-page command \
                 reference"
    )]
    Generate {
        #[arg(help = "Path to a Vivado man-page file (e.g. \
                      <Vivado>/doc/eng/man/add_files)")]
        input: Utf8PathBuf,
        #[arg(short, long, help = "Output file (defaults to stdout)")]
        output: Option<Utf8PathBuf>,
        #[arg(
            long,
            help = "Command name to wrap (defaults to the input file stem)"
        )]
        name: Option<String>,
        #[arg(
            long,
            value_name = "FILE",
            help = "Per-command constraint overrides (TOML)"
        )]
        constraints: Option<Utf8PathBuf>,
    },
}

#[derive(Subcommand)]
enum IpCommand {
    #[command(about = "Generate an htcl wrapper from an IP-XACT component")]
    Generate {
        #[arg(help = "Path to an IP-XACT component XML file")]
        input: Utf8PathBuf,
        #[arg(short, long, help = "Output file (defaults to stdout)")]
        output: Option<Utf8PathBuf>,
        #[arg(
            long,
            help = "Include parameters whose resolve attribute is not 'user'"
        )]
        include_internal: bool,
        #[arg(
            long = "preset",
            value_name = "FILE",
            help = "Supplementary Vivado preset XML file (`<preset param=... \
                    name=...\\>` format). May be given multiple times. The \
                    declared values are merged into `@enum(...)` lists in the \
                    generated wrapper, on top of the IP-XACT `<choice>` \
                    entries."
        )]
        presets: Vec<Utf8PathBuf>,
        #[arg(
            long,
            help = "Skip auto-discovery of preset files under the Vivado \
                    `data/versal/ps_pmc/<ip-name>/` tree. Use this if the \
                    discovered files are wrong or you only want the explicit \
                    `--preset` ones."
        )]
        no_auto_presets: bool,
        #[arg(
            long,
            value_name = "FILE",
            help = "Per-IP TOML overrides file. Refines XML-derived \
                    dict-schemas with `@enum(...)` restrictions and \
                    per-field default overrides. Silently ignored when \
                    the file doesn't exist."
        )]
        overrides: Option<Utf8PathBuf>,
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
            let Some(repo) = dep.repo() else { continue };
            if let Some(creds) = get_access_credentials_for_repo(repo).await {
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
                    let render = |dep: &vw_lib::DependencyInfo| {
                        let version_info = match &dep.version {
                            VersionInfo::Branch { branch } => {
                                format!(" (branch: {branch})")
                            }
                            VersionInfo::Commit { commit } => {
                                format!(" ({})", &commit[..8.min(commit.len())])
                            }
                            VersionInfo::Locked { commit } => {
                                format!(" ({})", &commit[..8.min(commit.len())])
                            }
                            VersionInfo::Local => " (local)".to_string(),
                            VersionInfo::Unknown => String::new(),
                        };
                        println!(
                            "  {} - {}{}",
                            dep.name.cyan(),
                            dep.source,
                            version_info.bright_black()
                        );
                    };
                    let (test_deps, regular_deps): (Vec<_>, Vec<_>) =
                        deps.into_iter().partition(|d| d.is_test);
                    if !regular_deps.is_empty() {
                        println!("Dependencies:");
                        for dep in &regular_deps {
                            render(dep);
                        }
                    }
                    if !test_deps.is_empty() {
                        if !regular_deps.is_empty() {
                            println!();
                        }
                        println!("Test dependencies:");
                        for dep in &test_deps {
                            render(dep);
                        }
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
        Commands::Bench {
            testbench,
            std,
            list,
            recurse,
            ignore,
            runtime_flags,
            build_rust,
            scaffold,
        } => {
            if list {
                let bench_dir = cwd.join("bench");
                if !bench_dir.exists() {
                    println!("No bench dir found in {:}", bench_dir.as_str());
                } else {
                    let mut ignore_set: HashSet<String> = HashSet::new();
                    for ignore_pattern in ignore {
                        ignore_set.insert(ignore_pattern);
                    }

                    let mist_configs =
                        vw_lib::sim::find_mist_configs(&bench_dir)
                            .unwrap_or_default();

                    match list_testbenches(&bench_dir, &ignore_set, recurse) {
                        Ok(testbenches) => {
                            if testbenches.is_empty() && mist_configs.is_empty()
                            {
                                println!(
                                    "No testbenches found in bench directory"
                                );
                            } else {
                                println!("Available testbenches:");
                                for (name, config) in &mist_configs {
                                    println!(
                                        "  {} - {} (mixed-signal: {})",
                                        name.cyan(),
                                        config.entity.bright_black(),
                                        config.netlist.bright_black()
                                    );
                                }
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
                }
            } else if let Some(testbench_name) = testbench {
                println!("Running testbench: {}", testbench_name.cyan());
                match run_testbench(
                    &cwd,
                    testbench_name.clone(),
                    std.into(),
                    recurse,
                    &runtime_flags,
                    build_rust,
                    scaffold,
                )
                .await
                {
                    Ok(()) => {
                        if scaffold {
                            println!(
                                "{} Scaffolding generated for '{}'",
                                "✓".bright_green(),
                                testbench_name
                            );
                        } else {
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
        Commands::Run {
            file,
            check,
            part,
            verbose,
            info_with_stack,
        } => {
            if let Err(e) = run_htcl(
                &file,
                check,
                part.as_deref(),
                verbose,
                info_with_stack,
            )
            .await
            {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Analyzer => {
            init_analyzer_logging();
            vw_analyzer::run_stdio().await;
        }
        Commands::Repl {
            verbose,
            initial_load,
            part,
            info_with_stack,
        } => {
            if let Err(e) = vw_repl::run(vw_repl::ReplOptions {
                verbose,
                initial_load,
                part,
                info_with_stack,
            })
            .await
            {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Check {
            files,
            part,
            all_parts,
        } => {
            // Two shapes:
            //   `vw check FILE [FILE...]` — check the explicit list.
            //   `vw check`               — discover from workspace:
            //     - `<ws>/module.htcl` in normal mode.
            //     - Every `<ws>/test/**/*.htcl` in test-mode
            //       (test-deps + self-injection visible so `src @<self>`
            //       and `src @<test-dep>` both resolve).
            let discovered = if files.is_empty() {
                match discover_check_targets(&cwd) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                }
            } else {
                files
                    .into_iter()
                    .map(|f| CheckTarget {
                        path: f,
                        include_test_deps: false,
                    })
                    .collect()
            };
            if discovered.is_empty() {
                eprintln!(
                    "{} nothing to check — pass a file, or run from a \
                     directory with a `vw.toml` that has a `module.htcl` \
                     or `test/*.htcl`",
                    "note:".bright_yellow(),
                );
                return;
            }
            let mut had_errors = false;
            let part_selector =
                PartSelector::from_flags(part.as_deref(), all_parts);
            for target in &discovered {
                let res = check_htcl_with_mode(
                    &target.path,
                    target.include_test_deps,
                    &part_selector,
                )
                .await;
                match res {
                    Ok(file_errs) => {
                        if file_errs {
                            had_errors = true;
                        }
                    }
                    Err(e) => {
                        had_errors = true;
                        eprintln!(
                            "{} {}: {e}",
                            "error:".bright_red(),
                            target.path,
                        );
                    }
                }
            }
            if had_errors {
                process::exit(1);
            }
        }
        Commands::Test {
            filter,
            list,
            test_threads,
            part,
            verbose,
            info_with_stack,
        } => {
            if let Err(e) = htcl_test::run_htcl_tests(
                &cwd,
                filter,
                list,
                test_threads,
                part.as_deref(),
                verbose,
                info_with_stack,
            )
            .await
            {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Ip(cmd) => match cmd {
            IpCommand::Generate {
                input,
                output,
                include_internal,
                presets,
                no_auto_presets,
                overrides,
            } => {
                if let Err(e) = run_ip_generate(
                    &input,
                    output.as_deref(),
                    include_internal,
                    &presets,
                    no_auto_presets,
                    overrides.as_deref(),
                ) {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        },
        Commands::HtclCmd(cmd) => match cmd {
            HtclCmdCommand::Generate {
                input,
                output,
                name,
                constraints,
            } => {
                if let Err(e) = run_htcl_cmd_generate(
                    &input,
                    output.as_deref(),
                    name.as_deref(),
                    constraints.as_deref(),
                ) {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        },
    }
}

fn run_htcl_cmd_generate(
    input: &Utf8Path,
    output: Option<&Utf8Path>,
    name: Option<&str>,
    constraints_path: Option<&Utf8Path>,
) -> Result<(), String> {
    let page = vw_htcl_cmd::load(input.as_std_path(), name)
        .map_err(|e| format!("loading {input}: {e}"))?;
    let constraints = match constraints_path {
        Some(p) => vw_htcl_cmd::ConstraintsTable::load(p.as_std_path())
            .map_err(|e| format!("loading constraints: {e}"))?,
        None => vw_htcl_cmd::ConstraintsTable::empty(),
    };
    let opts = vw_htcl_cmd::GenerateOptions {
        constraints,
        ..Default::default()
    };
    let text = vw_htcl_cmd::generate(&page, &opts);
    match output {
        Some(path) => std::fs::write(path, &text)
            .map_err(|e| format!("writing {path}: {e}"))?,
        None => print!("{text}"),
    }
    Ok(())
}

fn run_ip_generate(
    input: &Utf8Path,
    output: Option<&Utf8Path>,
    include_internal: bool,
    explicit_presets: &[Utf8PathBuf],
    no_auto_presets: bool,
    overrides_path: Option<&Utf8Path>,
) -> Result<(), String> {
    let component =
        vw_ip::load(input).map_err(|e| format!("loading {input}: {e}"))?;

    // Combine explicit `--preset` files with what we can auto-discover
    // under Vivado's `data/versal/ps_pmc/<ip>/` tree.
    let mut preset_paths: Vec<std::path::PathBuf> = explicit_presets
        .iter()
        .map(|p| std::path::PathBuf::from(p.as_str()))
        .collect();
    if !no_auto_presets {
        let discovered =
            vw_ip::discover_presets(std::path::Path::new(input.as_str()));
        for p in discovered {
            if !preset_paths.contains(&p) {
                preset_paths.push(p);
            }
        }
    }
    for p in &preset_paths {
        eprintln!("{:>12} {}", "Sourcing".bright_green().bold(), p.display());
    }
    let presets = if preset_paths.is_empty() {
        vw_ip::PresetMap::new()
    } else {
        vw_ip::load_presets(&preset_paths)
            .map_err(|e| format!("loading presets: {e}"))?
    };

    // Sub-proc schemas for Xilinx `structured_tcldict` parameters
    // (PS_PMC_CONFIG, etc.). Empty when the component isn't a CIPS
    // and doesn't have an accompanying schema tree.
    let dict_schemas =
        vw_ip::load_cips_dict_schemas(std::path::Path::new(input.as_str()));
    for name in dict_schemas.keys() {
        eprintln!(
            "{:>12} schema for {name} ({} fields)",
            "Loaded".bright_green().bold(),
            dict_schemas[name].fields.len()
        );
    }

    // Load per-IP TOML overrides. Missing file → empty overrides;
    // the generator falls back to XML-only defaults everywhere.
    let overrides = match overrides_path {
        Some(p) => {
            let ov =
                vw_ip::overrides::OverridesFile::load_from(p.as_std_path())
                    .map_err(|e| format!("{e}"))?;
            if !ov.is_empty() {
                eprintln!(
                    "{:>12} {} shape refinement(s) from {p}",
                    "Loaded".bright_green().bold(),
                    ov.shapes.len()
                );
            }
            ov
        }
        None => vw_ip::overrides::OverridesFile::default(),
    };

    let opts = vw_ip::GenerateOptions {
        user_configurable_only: !include_internal,
        overrides,
        ..Default::default()
    };
    let out = vw_ip::generate(&component, &presets, &dict_schemas, &opts);
    match output {
        Some(path) => {
            let module_dir = match path.parent() {
                Some(p) if !p.as_str().is_empty() => p.to_path_buf(),
                _ => Utf8PathBuf::from("."),
            };
            std::fs::write(path, &out.main)
                .map_err(|e| format!("writing {path}: {e}"))?;
            // Sibling `.htcl` files that the main module sources.
            // For the split gtwiz-versal IP this is 8+5 = 13 files
            // (one per intfN, quadN). For small IPs (cips, dcmac)
            // the subfiles vec is empty and only main.htcl gets
            // written.
            for (basename, content) in &out.subfiles {
                let sub_path = module_dir.join(basename);
                std::fs::write(&sub_path, content)
                    .map_err(|e| format!("writing {sub_path}: {e}"))?;
            }
            if !out.subfiles.is_empty() {
                eprintln!(
                    "{:>12} main + {} subfile(s)",
                    "Wrote".bright_green().bold(),
                    out.subfiles.len()
                );
            }
            // Generated IP modules need a workspace toml so `vw test`,
            // `vw analyzer`, and the REPL can resolve `src @vivado-cmd`
            // (and any other helpers the wrapper calls into). Seed a
            // default one alongside `module.htcl` on first generation;
            // never clobber an existing user-edited toml.
            ensure_module_vw_toml(&module_dir)?;
            // Extract `<xilinx:supportedFamilies>` from the source
            // component.xml and write it into the module's vw.toml
            // `[targets] supported` list. This lets downstream
            // `vw check` catch device-family mismatches statically
            // — no Vivado runtime dependency.
            let targets = vw_ip::targets::extract_targets(
                std::path::Path::new(input.as_str()),
            );
            if !targets.supported.is_empty()
                || !targets.not_supported.is_empty()
            {
                if let Err(e) = upsert_targets_in_vw_toml(&module_dir, &targets)
                {
                    eprintln!(
                        "{} updating {}/vw.toml `[targets]`: {e}",
                        "warning:".bright_yellow(),
                        module_dir,
                    );
                }
            }
        }
        None => {
            // stdout mode has no way to represent multiple files;
            // fall back to the concatenated single-file form so
            // manual `vw ip generate ... > file.htcl` invocations
            // still work.
            print!("{}", out.into_single());
        }
    }
    Ok(())
}

/// Write a default `vw.toml` to `dir` if one doesn't already exist.
///
/// Generated IP wrappers all `src @vivado-cmd` for the `ip::check` /
/// `log::error` / property-helper procs that drive their bodies, so
/// every module dir needs a workspace toml that points at the
/// vivado-cmd module. Without it, the analyzer and REPL flag
/// `undefined proc ip::check` on the first line of every freshly-
/// generated module.
///
/// The function is idempotent: an existing `vw.toml` is left alone so
/// the user can edit it (add deps, rename the workspace) without
/// having their changes overwritten the next time `regenerate.sh`
/// runs.
/// Serialize a `[targets] supported = [...]` section into `dir/vw.
/// toml`, upserting: if the file already declares `[targets]`,
/// replace the `supported` list; otherwise append a fresh section.
/// Other sections (workspace, dependencies) are preserved verbatim.
///
/// Uses a text-level upsert rather than a serde round-trip so
/// user-authored comments and formatting in other sections don't
/// get flattened.
fn upsert_targets_in_vw_toml(
    dir: &Utf8Path,
    targets: &vw_ip::targets::ExtractedTargets,
) -> Result<(), String> {
    let toml_path = dir.join("vw.toml");
    let mut existing = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("reading {toml_path}: {e}"))?;
    // Render the new block once — same shape whether we're
    // inserting or replacing. `not-supported` is written only when
    // non-empty so IPs whose XML is uniformly blessed produce a
    // tidy vw.toml with just `supported`.
    let mut new_block = String::from("[targets]\nsupported = [\n");
    for t in &targets.supported {
        new_block.push_str(&format!("    \"{t}\",\n"));
    }
    new_block.push_str("]\n");
    if !targets.not_supported.is_empty() {
        new_block.push_str("not-supported = [\n");
        for t in &targets.not_supported {
            new_block.push_str(&format!("    \"{t}\",\n"));
        }
        new_block.push_str("]\n");
    }

    // If a `[targets]` section already exists, replace it in place
    // (from its header line up to but not including the next
    // `[section]` header or EOF). Otherwise append at the bottom.
    if let Some(section_start) = existing.find("[targets]") {
        // Find end: the byte just before the next top-level
        // section header, or EOF.
        let after_hdr = section_start + "[targets]".len();
        let mut end = existing.len();
        let mut cur = after_hdr;
        let bytes = existing.as_bytes();
        while cur < bytes.len() {
            if bytes[cur] == b'\n'
                && cur + 1 < bytes.len()
                && bytes[cur + 1] == b'['
            {
                end = cur + 1;
                break;
            }
            cur += 1;
        }
        existing.replace_range(section_start..end, &new_block);
    } else {
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
        if !existing.ends_with("\n\n") {
            existing.push('\n');
        }
        existing.push_str(&new_block);
    }
    std::fs::write(&toml_path, existing)
        .map_err(|e| format!("writing {toml_path}: {e}"))?;
    Ok(())
}

fn ensure_module_vw_toml(dir: &Utf8Path) -> Result<(), String> {
    let toml_path = dir.join("vw.toml");
    if toml_path.exists() {
        return Ok(());
    }
    let name = dir
        .canonicalize_utf8()
        .ok()
        .as_deref()
        .and_then(|p| p.file_name())
        .or_else(|| dir.file_name())
        .unwrap_or("module")
        .to_string();
    let (dep_line, note) = match discover_sibling_vivado_cmd(dir) {
        Some(p) => (format!("path = \"{p}\""), None),
        None => (
            "path = \"../vivado-cmd\"".to_string(),
            Some("# TODO: adjust to your vivado-cmd module path"),
        ),
    };
    let mut content = format!(
        "[workspace]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         \n\
         [dependencies.vivado-cmd]\n"
    );
    if let Some(n) = note {
        content.push_str(n);
        content.push('\n');
    }
    content.push_str(&dep_line);
    content.push('\n');
    std::fs::write(&toml_path, content)
        .map_err(|e| format!("writing {toml_path}: {e}"))?;
    eprintln!("{:>12} {}", "Created".bright_green().bold(), toml_path);
    Ok(())
}

/// Walk up from `start` looking for a sibling `vivado-cmd/vw.toml`.
/// Returns the canonical absolute path to the directory if found.
/// Used by [`ensure_module_vw_toml`] to seed the dep path for
/// freshly-generated IP modules.
fn discover_sibling_vivado_cmd(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let abs = start.canonicalize_utf8().ok()?;
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        let candidate = parent.join("vivado-cmd");
        if candidate.join("vw.toml").is_file() {
            return Some(candidate);
        }
        cur = parent;
    }
    None
}

/// Read `entry` and recursively resolve its `src` imports. Looks for
/// a `vw.toml` in the entry file's parent chain to discover the
/// workspace; falls back to an empty resolver (so relative/absolute
/// imports still work, but `@name/` imports fail with a clear error)
/// when no workspace is found.
///
/// A [`CliObserver`] is attached so the loader's progress prints in
/// real time as `Sourcing …` / `Checking …` lines.
async fn load_htcl_program(
    entry: &Utf8Path,
) -> Result<vw_htcl::LoadedProgram, Box<dyn std::error::Error>> {
    load_htcl_program_with_mode(entry, false).await
}

/// Same as [`load_htcl_program`] but resolves
/// `[test-dependencies]` from the entry's workspace too. Used by
/// `vw test`. Cargo-parity: only the ENTRY workspace's test-deps
/// are pulled in — transitive workspaces don't leak their own
/// test-deps into the resolver.
#[allow(dead_code)] // wired via `crate::htcl_test`
pub(crate) async fn load_htcl_program_for_test(
    entry: &Utf8Path,
) -> Result<vw_htcl::LoadedProgram, Box<dyn std::error::Error>> {
    load_htcl_program_with_mode(entry, true).await
}

async fn load_htcl_program_with_mode(
    entry: &Utf8Path,
    include_test_deps: bool,
) -> Result<vw_htcl::LoadedProgram, Box<dyn std::error::Error>> {
    let entry_path = std::path::Path::new(entry.as_str()).to_path_buf();
    let workspace_dir =
        entry_path.parent().and_then(vw_lib::find_workspace_dir);
    let mut resolver = vw_htcl::Resolver::new();
    // Load the workspace config once — its `name` field feeds both
    // the progress bar's label AND the self-injection at the
    // bottom of this block.
    let workspace_cfg = workspace_dir
        .as_deref()
        .and_then(|ws| vw_lib::load_workspace_config(ws).ok());
    if let Some(ws) = workspace_dir.as_deref() {
        // Transitive resolution so a library's `src @other/...`
        // import works even when the consumer hasn't redeclared
        // `other` in their own `vw.toml`.
        if let Ok(paths) =
            vw_lib::transitive_dep_cache_paths_with_test(ws, include_test_deps)
        {
            for (name, path) in paths {
                resolver = resolver.with_dep(name, path);
            }
        }
        // Cargo-parity self-reference: a library named `foo` can
        // `src @foo/bar` to reach its own siblings without the
        // user having to declare `foo` as a dep of itself. Uses
        // `with_dep_if_absent` so a legitimately-declared external
        // `foo` still wins.
        if let Some(cfg) = &workspace_cfg {
            resolver = resolver.with_dep_if_absent(
                cfg.workspace.name.clone(),
                ws.as_std_path().to_path_buf(),
            );
        }
    }
    let dep_paths: Vec<(String, std::path::PathBuf)> = workspace_dir
        .as_deref()
        .and_then(|ws| vw_lib::transitive_dep_cache_paths(ws).ok())
        .map(|paths| paths.into_iter().collect())
        .unwrap_or_default();
    // Pick up the workspace name from `vw.toml` so the local
    // (non-@dep) files' bar shows `Checking metroid` instead of
    // `Checking workspace`. Falls back to the literal `workspace`
    // when there's no vw.toml or no `name = "…"` field.
    let workspace_label = workspace_cfg
        .as_ref()
        .map(|cfg| cfg.workspace.name.clone())
        .unwrap_or_else(|| "workspace".to_string());
    let observer = std::sync::Arc::new(
        parallel_load::MultiProgressObserver::new(dep_paths, workspace_label),
    );
    let obs_for_load: std::sync::Arc<dyn parallel_load::ParallelObserver> =
        observer.clone();
    let result = parallel_load::load_parallel(
        &entry_path,
        std::sync::Arc::new(resolver),
        obs_for_load,
        std::collections::HashMap::new(),
    )
    .await;
    observer.finish();
    Ok(result?)
}

/// `entry`. Used by `check_htcl` to pre-flight `src @<name>`
/// imports and produce spanned diagnostics before the loader's
/// hard-abort path fires. Returns an empty set when `entry` isn't
/// inside a workspace or the dep cache can't be read — the caller
/// treats empty as "skip the check", matching the validator's own
/// short-circuit.
#[allow(dead_code)] // legacy entry — new callers use `_with_mode`
fn collect_dep_names(entry: &Utf8Path) -> std::collections::HashSet<String> {
    collect_dep_names_with_mode(entry, false)
}

fn collect_dep_names_with_mode(
    entry: &Utf8Path,
    include_test_deps: bool,
) -> std::collections::HashSet<String> {
    let entry_path = std::path::Path::new(entry.as_str());
    let Some(ws) = entry_path.parent().and_then(vw_lib::find_workspace_dir)
    else {
        return std::collections::HashSet::new();
    };
    let Ok(paths) =
        vw_lib::transitive_dep_cache_paths_with_test(&ws, include_test_deps)
    else {
        return std::collections::HashSet::new();
    };
    let mut names: std::collections::HashSet<String> =
        paths.into_keys().collect();
    // Mirror `load_htcl_program`'s self-injection: a library named
    // `foo` can `src @foo/bar` at check time even though `foo`
    // isn't in its own [dependencies].
    if let Ok(cfg) = vw_lib::load_workspace_config(&ws) {
        names.insert(cfg.workspace.name);
    }
    names
}

fn init_analyzer_logging() {
    // Silent by default — see the matching note in vw-analyzer's main.
    let filter = tracing_subscriber::EnvFilter::try_from_env("VW_ANALYZER_LOG")
        .unwrap_or_else(|_| "vw_analyzer=off".into());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
}

/// Run parse + signature validation on `file`. Returns `Ok(true)`
/// if any error-severity diagnostics were reported, `Ok(false)` for
/// clean. Warnings don't flip the return value but still print.
#[allow(dead_code)] // legacy entry — new callers use `_with_mode`
async fn check_htcl(
    file: &camino::Utf8Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    check_htcl_with_mode(file, false, &PartSelector::Default).await
}

/// One entry in the `vw check` (no-args) discovery result. Test
/// files get `include_test_deps = true` so the validator can see
/// `@<test-dep>` and `@<workspace_self>` imports without the user
/// spelling them out in the vw.toml `[dependencies]` section.
struct CheckTarget {
    path: Utf8PathBuf,
    include_test_deps: bool,
}

/// Discover files to check when the user runs `vw check` from a
/// workspace directory with no explicit file list. Adds
/// `<ws>/module.htcl` when present (checked in normal mode) plus
/// every `<ws>/test/**/*.htcl` (checked in test-mode).
///
/// Errors when we can't find the enclosing workspace at all —
/// otherwise returns an empty vec, letting the caller print
/// "nothing to check" without treating it as a hard failure.
fn discover_check_targets(
    cwd: &Utf8Path,
) -> Result<Vec<CheckTarget>, Box<dyn std::error::Error>> {
    let ws = vw_lib::find_workspace_dir(cwd.as_std_path())
        .ok_or("not in a vw workspace (no vw.toml in the parent chain)")?;
    let mut targets = Vec::new();
    let module = ws.join("module.htcl");
    if module.is_file() {
        targets.push(CheckTarget {
            path: module,
            include_test_deps: false,
        });
    }
    for path in vw_lib::list_htcl_tests(&ws)? {
        let Ok(path) = Utf8PathBuf::from_path_buf(path) else {
            continue;
        };
        targets.push(CheckTarget {
            path,
            include_test_deps: true,
        });
    }
    Ok(targets)
}

/// How `vw check` decides which `[[target-parts]]` entries to
/// evaluate. Constructed from the CLI's `--part` / `--all-parts`
/// flags; consumed by [`check_htcl_with_mode`] to iterate the
/// selected parts through the compatibility check.
#[derive(Debug, Clone)]
enum PartSelector {
    /// No flag → use the workspace's default part.
    Default,
    /// `--part <id>` → use the matching entry.
    Explicit(String),
    /// `--all-parts` → iterate every entry.
    All,
}

impl PartSelector {
    fn from_flags(part: Option<&str>, all_parts: bool) -> Self {
        if all_parts {
            Self::All
        } else if let Some(p) = part {
            Self::Explicit(p.to_string())
        } else {
            Self::Default
        }
    }

    /// Resolve to a concrete list of part strings against `ws_info`.
    /// Returns an empty vec for a library workspace (no target
    /// parts declared) — caller treats that as "no compat check."
    fn resolve<'a>(
        &self,
        ws_info: &'a vw_lib::WorkspaceInfo,
    ) -> std::result::Result<Vec<&'a str>, vw_lib::TargetSelectError> {
        if ws_info.target_parts.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            Self::Default => {
                Ok(ws_info.default_target_part()?.into_iter().collect())
            }
            Self::Explicit(q) => {
                Ok(ws_info.select_target_part(Some(q))?.into_iter().collect())
            }
            Self::All => Ok(ws_info
                .target_parts
                .iter()
                .map(|p| p.part.as_str())
                .collect()),
        }
    }
}

async fn check_htcl_with_mode(
    file: &camino::Utf8Path,
    include_test_deps: bool,
    part_selector: &PartSelector,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Pre-flight: run the validator's src-import check on the entry
    // file BEFORE handing to `load_htcl_program`, which would
    // hard-abort on the first unresolved `src @<name>` with a bare
    // non-span error. The pre-flight produces the same spanned
    // diagnostics the LSP shows, so `vw check` and the editor agree
    // on where the missing dep is and how to fix it. Only kicks in
    // when the workspace has a `vw.toml` (otherwise dep-names is
    // empty and the check is a no-op).
    let dep_names = collect_dep_names_with_mode(file, include_test_deps);
    if !dep_names.is_empty() {
        let entry_text = std::fs::read_to_string(file.as_str())?;
        let entry_parsed = vw_htcl::parse(&entry_text);
        let pre_diags = vw_htcl::validate_with_all_extras_and_vars(
            &entry_parsed.document,
            &entry_text,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &dep_names,
        );
        let src_errs: Vec<_> = pre_diags
            .iter()
            .filter(|d| {
                d.severity == vw_htcl::Severity::Error
                    && d.message.starts_with("unknown src module")
            })
            .collect();
        if !src_errs.is_empty() {
            let idx = vw_htcl::LineIndex::new(&entry_text);
            let cwd_owned = std::env::current_dir().ok();
            let cwd = cwd_owned.as_deref();
            let display_path =
                render_path(std::path::Path::new(file.as_str()), cwd);
            for d in &src_errs {
                let (start, _) = idx.range(d.span);
                eprintln!(
                    "{} {display_path}:{}:{}: {}",
                    "error:".bright_red(),
                    start.line + 1,
                    start.character + 1,
                    d.message,
                );
            }
            eprintln!("{file}: {} error(s), 0 warning(s)", src_errs.len());
            return Ok(true);
        }
    }

    let program = if include_test_deps {
        load_htcl_program_for_test(file).await?
    } else {
        load_htcl_program(file).await?
    };
    let parsed = vw_htcl::parse(&program.source);
    let validator_diags = vw_htcl::validate(&parsed.document, &program.source);

    // Build a per-file `LineIndex` lazily so we only pay for files
    // that actually have diagnostics. Keyed by `file_index`.
    let cwd_owned = std::env::current_dir().ok();
    let cwd = cwd_owned.as_deref();
    let mut indices: std::collections::HashMap<usize, vw_htcl::LineIndex> =
        std::collections::HashMap::new();

    let mut error_count = 0usize;
    let mut warning_count = 0usize;
    let mut emit = |severity: Option<vw_htcl::Severity>,
                    message: &str,
                    span: vw_htcl::Span| {
        let level = match severity {
            None | Some(vw_htcl::Severity::Error) => {
                error_count += 1;
                "error:".bright_red()
            }
            Some(vw_htcl::Severity::Warning) => {
                warning_count += 1;
                "warning:".bright_yellow()
            }
        };
        // Map the span back to its originating file's line/col so the
        // displayed location is the file the user actually wrote — not
        // the flat dependency-concatenated source the loader produced.
        let (display_path, line, col) = match program.locate_span(span) {
            Some((idx, file_span)) => {
                let loaded = &program.files[idx];
                let index = indices
                    .entry(idx)
                    .or_insert_with(|| vw_htcl::LineIndex::new(&loaded.source));
                let (start, _) = index.range(file_span);
                (
                    render_path(&loaded.path, cwd),
                    start.line + 1,
                    start.character + 1,
                )
            }
            None => (file.to_string(), 0, 0),
        };
        eprintln!("{} {display_path}:{line}:{col}: {message}", level);
    };

    for err in &parsed.errors {
        emit(None, &err.message, err.span);
    }
    for d in &validator_diags {
        emit(Some(d.severity), &d.message, d.span);
    }

    // Target-compatibility check. Iterates over the parts the
    // selector picked (default, explicit `--part`, or every
    // `[[target-parts]]` entry with `--all-parts`). Library
    // workspaces (no target parts) skip silently.
    let entry_path = std::path::Path::new(file.as_str());
    if let Some(ws) = entry_path.parent().and_then(vw_lib::find_workspace_dir) {
        if let Ok(cfg) = vw_lib::load_workspace_config(&ws) {
            let parts = match part_selector.resolve(&cfg.workspace) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{} {e}", "error:".bright_red());
                    error_count += 1;
                    Vec::new()
                }
            };
            if !parts.is_empty() {
                let dep_targets = vw_lib::collect_dep_targets(&ws);
                for (dep, err) in &dep_targets.errors {
                    eprintln!(
                        "{} bad `[targets]` pattern in dep `{dep}`: {err}",
                        "warning:".bright_yellow(),
                    );
                    warning_count += 1;
                }
                for target_part in &parts {
                    let mismatches = vw_lib::check_target_compatibility(
                        Some(target_part),
                        &dep_targets,
                    );
                    for m in &mismatches {
                        match m.kind {
                            vw_lib::TargetMismatchKind::NotSupported => {
                                eprintln!(
                                    "{} target-part `{}` matches dep `{}`'s \
                                     `not-supported` list — Xilinx has \
                                     attested the IP is not usable on this \
                                     part ({})",
                                    "error:".bright_red(),
                                    m.target_part,
                                    m.dep,
                                    target_mismatch_families_hint(m),
                                );
                                error_count += 1;
                            }
                            vw_lib::TargetMismatchKind::Unblessed => {
                                eprintln!(
                                    "{} target-part `{}` isn't blessed by \
                                     dep `{}` ({}); the IP may still work \
                                     but Xilinx hasn't blessed the \
                                     combination",
                                    "warning:".bright_yellow(),
                                    m.target_part,
                                    m.dep,
                                    target_mismatch_families_hint(m),
                                );
                                warning_count += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if error_count > 0 || warning_count > 0 {
        eprintln!("{file}: {error_count} error(s), {warning_count} warning(s)");
    }
    Ok(error_count > 0)
}

/// Render `path` relative to `cwd` when it sits underneath, otherwise
/// fall back to the absolute path. Keeps diagnostic locations short
/// and click-through-able in editors / terminals.
fn render_path(
    path: &std::path::Path,
    cwd: Option<&std::path::Path>,
) -> String {
    if let Some(cwd) = cwd {
        if let Ok(rel) = path.strip_prefix(cwd) {
            return rel.display().to_string();
        }
    }
    path.display().to_string()
}

/// Human-readable summary of what a dep declared in its
/// `[targets]` block. Splits blessed vs. banned families so a dep
/// with only a `not-supported` list (e.g. clk-wizard v1.0, where
/// every entry is `Not-Supported`) doesn't get misreported as
/// "declared families: versal" — the versal families are BANNED
/// there, not blessed.
fn target_mismatch_families_hint(m: &vw_lib::TargetMismatch) -> String {
    match (
        m.supported_families.is_empty(),
        m.not_supported_families.is_empty(),
    ) {
        (true, true) => {
            "no `[targets]` families declared — the dep has patterns \
             but none carry family names"
                .to_string()
        }
        (false, true) => {
            format!("blessed families: {}", m.supported_families.join(", "),)
        }
        (true, false) => {
            format!(
                "no blessed families — only `not-supported` entries for {}",
                m.not_supported_families.join(", "),
            )
        }
        (false, false) => {
            format!(
                "blessed families: {}; also `not-supported` entries for {}",
                m.supported_families.join(", "),
                m.not_supported_families.join(", "),
            )
        }
    }
}

/// For every monomorphized generic encountered while walking `ty`
/// (recursing through user-newtype underlyings), emit its repr to
/// the backend exactly once. Dedup is owned by the caller so
/// repeated invocations across signatures don't re-ship the same
/// proc.
/// Turn a sequence of [`vw_repl::highlight::Piece`]s (a repr-line
/// highlight result) into an ANSI-colored string using the same
/// palette the REPL's ratatui renderer uses.
///
/// Emits raw 24-bit truecolor escapes (`\x1b[38;2;R;G;Bm`)
/// directly instead of going through `colored`'s `.truecolor()`
/// — `colored` 2.0 downgrades RGB to the nearest ANSI-16 code
/// unless `COLORTERM` explicitly announces truecolor, which
/// happens to map our REPL palette to shades that read as grey
/// (e.g. RGB(120, 200, 120) → `\e[90m` bright_black). The REPL's
/// ratatui backend emits raw truecolor unconditionally and looks
/// correct on every modern terminal; matching its behavior here
/// keeps `vw run` visually aligned with the REPL.
///
/// Suppression on `NO_COLOR` / non-TTY stdout is preserved via
/// `colored`'s global gate.
fn ansi_from_pieces(pieces: &[vw_repl::highlight::Piece]) -> String {
    use vw_repl::highlight::StyleKind;
    let colorize = colored::control::SHOULD_COLORIZE.should_colorize();
    let mut out = String::new();
    for p in pieces {
        if !colorize {
            out.push_str(&p.text);
            continue;
        }
        // Palette constants mirror the ratatui RGB values in
        // `vw-repl/src/highlight.rs::{key_style, variant_style,
        // scalar_style}` and the DIM modifier on `punct_style`.
        // Keep the two backends in sync — if the ratatui palette
        // changes, this needs the same edit.
        let escape = match p.kind {
            StyleKind::Plain => None,
            StyleKind::Key => Some("\x1b[38;2;80;150;255m"),
            StyleKind::Variant => Some("\x1b[38;2;100;200;200m"),
            StyleKind::Punct => Some("\x1b[2m"),
            StyleKind::Scalar => Some("\x1b[38;2;120;200;120m"),
        };
        match escape {
            None => out.push_str(&p.text),
            Some(prefix) => {
                out.push_str(prefix);
                out.push_str(&p.text);
                out.push_str("\x1b[0m");
            }
        }
    }
    out
}

/// Stream-sink rendering for `vw run`. Mirrors the REPL's
/// scrollback colors + stack-frame rewriting so both surfaces
/// look the same:
///
/// - **Stream kind → ANSI color/prefix**
///   - `Error`   → `✗ ` red bold
///   - `Warning` → `⚠ ` orange (Rgb 255,140,0)
///   - `Info`    → `· ` dark gray
///   - `Stdout`  → no prefix, no color
///
/// - **Stack-frame rewriting**: lines matching `  at <input>:N
///   in ::proc` are mapped to the real htcl source via
///   [`vw_repl::resolve_stack_frames_with`] + `proc_table`.
///   Adjacent frames pointing at the same proc collapse to one.
///
/// - **Origin tagging**: warnings/errors that arrive without an
///   `\n  at …` trace get one appended pointing at the
///   currently-executing top-level statement (`origin`). Mirrors
///   the REPL's `tag_streamed_message` — Vivado C++ paths
///   bypass `::common::send_msg_id` and emit traceless messages
///   we'd otherwise have no anchor for.
fn render_chunk(
    kind: vw_vivado::StreamKind,
    chunk: &str,
    proc_table: &std::collections::HashMap<String, vw_repl::ProcLocation>,
    origin: Option<&vw_repl::Origin>,
    input_file: Option<&std::path::Path>,
) {
    use colored::Colorize;
    use std::io::Write;
    // Drop a single trailing newline so the per-message layout
    // doesn't insert a blank gap. The shim's `puts` already
    // preserves user-side newlines inside the message.
    let trimmed = chunk.trim_end_matches('\n');
    if trimmed.is_empty() {
        return;
    }
    let resolved = vw_repl::resolve_stack_frames_with(
        trimmed,
        |name| proc_table.get(name).cloned(),
        input_file,
    );
    // Tag traceless warnings/errors with the currently-executing
    // statement's origin.
    let tagged = match kind {
        vw_vivado::StreamKind::Warning | vw_vivado::StreamKind::Error
            if !resolved.contains("\n  at ") =>
        {
            match origin {
                Some(o) => {
                    let path = o
                        .file
                        .as_deref()
                        .map(vw_repl::display_path)
                        .unwrap_or_else(|| {
                            input_file
                                .map(vw_repl::display_path)
                                .unwrap_or_else(|| "<input>".into())
                        });
                    format!("{resolved}\n  at {path}:{}", o.line)
                }
                None => resolved,
            }
        }
        _ => resolved,
    };
    let prefix = match kind {
        vw_vivado::StreamKind::Error => "✗ ",
        vw_vivado::StreamKind::Warning => "⚠ ",
        vw_vivado::StreamKind::Info => "· ",
        vw_vivado::StreamKind::Stdout => "",
    };
    let mut out = std::io::stdout().lock();
    for (i, line) in tagged.lines().enumerate() {
        let leading = if i == 0 || prefix.is_empty() {
            prefix
        } else {
            "  "
        };
        let styled_prefix: String = match kind {
            vw_vivado::StreamKind::Error => leading.red().bold().to_string(),
            vw_vivado::StreamKind::Warning => {
                leading.truecolor(255, 140, 0).bold().to_string()
            }
            vw_vivado::StreamKind::Info => leading.bright_black().to_string(),
            vw_vivado::StreamKind::Stdout => leading.to_string(),
        };
        let styled_line: String = match kind {
            vw_vivado::StreamKind::Error => line.red().to_string(),
            vw_vivado::StreamKind::Warning => {
                line.truecolor(255, 140, 0).to_string()
            }
            vw_vivado::StreamKind::Info => line.bright_black().to_string(),
            // Stdout is where the shim's `puts` output lands,
            // including compiler-emitted enum reprs like
            //   CPM_PCIE0_MODES Scalar(None)
            //   CONFIG Nested(
            //     …
            //   )
            // Route it through the REPL's shape-based highlighter
            // so `vw run` and the REPL scrollback look identical
            // for repr-shaped lines. Non-repr text (plain `puts`
            // messages, error messages that reach Stdout, etc.)
            // fails the parse and falls through to the raw line.
            // The `colored` crate that underpins ansi_from_pieces
            // already respects NO_COLOR + tty-detection, so the
            // integration inherits the standard "quiet when piped
            // or NO_COLOR is set" behavior for free.
            vw_vivado::StreamKind::Stdout => {
                match vw_repl::highlight::highlight_line_pieces(line) {
                    Some(pieces) => ansi_from_pieces(&pieces),
                    None => line.to_string(),
                }
            }
        };
        let _ = writeln!(out, "{styled_prefix}{styled_line}");
    }
    let _ = out.flush();
}

async fn ship_generic_reprs(
    backend: &mut vw_vivado::VivadoBackend,
    ty: &vw_htcl::TypeExpr,
    types: &std::collections::HashMap<String, &vw_htcl::TypeDecl>,
    emitted: &mut std::collections::HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // TypeExpr::Qualified appears only on overloaded-handler
    // first-args; the validator forbids it anywhere else, and
    // codegen doesn't need a repr for it.
    if matches!(ty, vw_htcl::TypeExpr::Qualified { .. }) {
        return Ok(());
    }
    let emission = vw_htcl::emit_repr_with_types(ty, types);
    for p in &emission.procs {
        // The procs are emitted in dependency order; the body of
        // each instantiation may reference earlier ones in the
        // same emission, so we ship them sequentially through
        // the same eval channel.
        if emitted.insert(p.clone()) {
            backend.eval(p).await?;
        }
    }
    Ok(())
}

/// Mirror of `vw-repl/src/lower.rs::overload_specialization_mangle`.
/// If `cmd` is a top-level `proc` whose name is an overload public
/// name AND whose first arg is a qualified-variant annotation,
/// return the mangled internal name to lower it under. Keeps
/// `vw run` in step with the REPL's specialization-rerouting.
fn overload_specialization_mangle(
    cmd: &vw_htcl::Command,
    overloads: &vw_htcl::OverloadTable,
) -> Option<String> {
    let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
        return None;
    };
    let name = proc.name.as_deref()?;
    if !overloads.contains_key(name) {
        return None;
    }
    let sig = proc.signature.as_ref()?;
    let first = sig.args.first()?;
    let vw_htcl::TypeExpr::Qualified { variant, .. } =
        first.type_annotation.as_ref()?
    else {
        return None;
    };
    Some(vw_htcl::mangle_specialization(name, variant))
}

async fn run_htcl(
    file: &camino::Utf8Path,
    check_only: bool,
    part: Option<&str>,
    verbose: bool,
    info_with_stack: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let program = load_htcl_program(file).await?;
    // Keep `program` alive — the stack-frame rewriting needs the
    // LoadedProgram for body-span resolution. We borrow `source`
    // from it instead of moving.
    let source = program.source.clone();
    let parsed = vw_htcl::parse(&source);
    let line_index = vw_htcl::LineIndex::new(&source);
    // Compile-time `putr` rewrite map: every `putr <expr>` command
    // in the document gets a replacement Tcl string keyed by its
    // span. `vw_htcl::lower_command_with_putr` consults the map at
    // emit time. See `vw-htcl/src/putr.rs` for the walker.
    let putr_map = vw_htcl::putr::rewrite(&source, &parsed.document);

    let mut had_errors = false;
    for err in &parsed.errors {
        had_errors = true;
        let (start, _end) = line_index.range(err.span);
        eprintln!(
            "{} {}:{}:{}: {}",
            "error:".bright_red(),
            file,
            start.line + 1,
            start.character + 1,
            err.message
        );
    }
    if had_errors {
        return Err(format!(
            "{} parse error(s); aborting",
            parsed.errors.len()
        )
        .into());
    }

    // Validator gate. `vw check prime.htcl` runs `vw_htcl::validate`
    // and returns non-zero on any error; `vw run prime.htcl` used
    // to skip the validator entirely and hand the (possibly
    // type-broken) program to Vivado, letting silent runtime
    // divergence hide real bugs the checker had already found. Run
    // the same validator here and abort on any error before
    // spawning Vivado — same behavior `check` shows, same exit
    // path. Warnings still emit but don't gate execution.
    let validator_diags = vw_htcl::validate(&parsed.document, &source);
    let mut error_count = 0usize;
    let mut warning_count = 0usize;
    let cwd_owned = std::env::current_dir().ok();
    let cwd = cwd_owned.as_deref();
    let mut indices: std::collections::HashMap<usize, vw_htcl::LineIndex> =
        std::collections::HashMap::new();
    for d in &validator_diags {
        let (display_path, line, col) = match program.locate_span(d.span) {
            Some((idx, file_span)) => {
                let loaded = &program.files[idx];
                let index = indices
                    .entry(idx)
                    .or_insert_with(|| vw_htcl::LineIndex::new(&loaded.source));
                let (start, _) = index.range(file_span);
                (
                    render_path(&loaded.path, cwd),
                    start.line + 1,
                    start.character + 1,
                )
            }
            None => (file.to_string(), 0, 0),
        };
        match d.severity {
            vw_htcl::Severity::Error => {
                error_count += 1;
                eprintln!(
                    "{} {display_path}:{line}:{col}: {}",
                    "error:".bright_red(),
                    d.message
                );
            }
            vw_htcl::Severity::Warning => {
                warning_count += 1;
                eprintln!(
                    "{} {display_path}:{line}:{col}: {}",
                    "warning:".bright_yellow(),
                    d.message
                );
            }
        }
    }
    if error_count > 0 {
        eprintln!("{file}: {error_count} error(s), {warning_count} warning(s)");
        return Err(
            format!("{error_count} validation error(s); aborting",).into()
        );
    }

    if check_only {
        let cmd_count = parsed
            .document
            .stmts
            .iter()
            .filter(|s| matches!(s, vw_htcl::Stmt::Command(_)))
            .count();
        println!(
            "{} {file}: parsed OK ({cmd_count} command(s))",
            "✓".bright_green()
        );
        return Ok(());
    }

    // RPC handler — serves htcl `vw::…` calls whose answers live
    // on the tool side (workspace root, design source list, …).
    // Constructed with whatever workspace state we can discover
    // from the entry file; when no `vw.toml` is found the
    // handler still exists but returns an error for `workspace_
    // root`, matching how the same call behaves in the LSP.
    let file_path = std::path::Path::new(file.as_str());
    let rpc_workspace_root: Option<std::path::PathBuf> = file_path
        .parent()
        .and_then(vw_lib::find_workspace_dir)
        .map(|p| p.into_std_path_buf());
    // Auto-project bootstrap: if the enclosing workspace declares
    // `[[target-parts]]`, ship a `create_project -in_memory` down
    // the wire before user code runs. `--part <id>` picks a
    // non-default entry; otherwise we use the workspace default.
    // Eliminates the "no open project" failure mode for
    // `ip::check` / `get_ipdefs` / etc., and gives the whole
    // session a stable part context for downstream
    // implementation/timing steps. `select_target_part` errors
    // out on bad selectors — surface those before booting Vivado.
    let auto_project = rpc_workspace_root
        .as_deref()
        .and_then(camino::Utf8Path::from_path)
        .and_then(|ws| vw_lib::load_workspace_config(ws).ok())
        .map(|cfg| {
            cfg.workspace.select_target_part(part).map(|maybe_part| {
                maybe_part.map(|p| vw_vivado::AutoProject {
                    name: cfg.workspace.name.clone(),
                    part: p.to_string(),
                })
            })
        })
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten();
    let rpc_handler = vw_vivado::make_handler(rpc_workspace_root);
    let mut backend =
        vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
            verbose,
            info_with_stack,
            rpc_handler: Some(rpc_handler),
            auto_project,
            ..Default::default()
        })
        .await
        .map_err(|e| format!("failed to start Vivado worker: {e}"))?;

    // Build the proc-location table the stream sink uses to map
    // Tcl `<input>:N in ::proc` frames back to real htcl source.
    // Mirrors what `vw-repl` does per batch — we use the same
    // shared helpers (`vw_repl::trace::*`) so REPL and CLI render
    // the same. The entry file IS the scratch from build_proc_locations'
    // perspective.
    let entry_std_path = std::path::Path::new(file.as_str()).to_path_buf();
    let proc_table = std::sync::Arc::new(vw_repl::build_proc_locations(
        &parsed.document,
        &program,
        &entry_std_path,
    ));
    let input_file_for_stack = std::sync::Arc::new(entry_std_path.clone());
    // Shared between the main loop (writes the current origin
    // before each eval) and the stream sink (reads it to tag
    // unattributed warnings — e.g. Vivado IP-Flow C++ messages
    // that bypass `::common::send_msg_id`). Same trick the REPL
    // uses with `pending_origins[pending_eval_index]`.
    let current_origin =
        std::sync::Arc::new(std::sync::Mutex::new(None::<vw_repl::Origin>));
    {
        let procs = std::sync::Arc::clone(&proc_table);
        let input_file = std::sync::Arc::clone(&input_file_for_stack);
        let origin = std::sync::Arc::clone(&current_origin);
        backend.set_stdout_sink(move |kind, chunk: &str| {
            let cur_origin = origin.lock().ok().and_then(|g| g.clone());
            render_chunk(
                kind,
                chunk,
                &procs,
                cur_origin.as_ref(),
                Some(input_file.as_path()),
            );
        });
    }

    // Lower structured proc declarations and call sites to plain Tcl
    // before sending. Generic commands pass through unchanged.
    let table = vw_htcl::signature_table(&parsed.document);
    // Ship enum preludes + overload dispatchers before any user
    // statements run — same shape as the REPL's prepare path.
    // Without these, calls to `Property::Scalar` or to an
    // overloaded `handle` would fail at runtime with `invalid
    // command name`.
    let mut _ignored = Vec::new();
    let enum_decl_table =
        vw_htcl::build_enum_decl_table(&parsed.document, &mut _ignored);
    let type_decl_table =
        vw_htcl::build_type_decl_table(&parsed.document, &mut _ignored);
    let type_decl_names: std::collections::HashSet<String> =
        type_decl_table.keys().cloned().collect();
    let (full_sigs, overload_table) =
        vw_htcl::build_signature_table_with_overloads(
            &parsed.document,
            &type_decl_names,
            &mut _ignored,
        );
    // Always ship the primitive prelude so user-written newtype
    // reprs can call e.g. `string::repr -v $v` for their inner
    // values.
    for p in vw_htcl::emit_primitive_prelude() {
        let _ = backend.eval(&p).await?;
    }
    for ed in enum_decl_table.values() {
        let prelude = vw_htcl::emit_enum_prelude(ed);
        if !prelude.trim().is_empty() {
            let _ = backend.eval(&prelude).await?;
        }
    }
    for info in overload_table.values() {
        let dispatcher = vw_htcl::emit_dispatcher(info);
        let _ = backend.eval(&dispatcher).await?;
    }
    // Ship monomorphized generic reprs for every type expression
    // referenced in any proc signature. This covers user newtypes
    // that delegate to a generic repr (e.g. `Properties::repr`
    // delegates to `dict_string_Property::repr`); without these
    // the user's repr body errors at runtime with `invalid
    // command name`.
    let mut emitted_generics: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for sig in full_sigs.values() {
        if let Some(ret) = sig.return_type.as_ref() {
            ship_generic_reprs(
                &mut backend,
                ret,
                &type_decl_table,
                &mut emitted_generics,
            )
            .await?;
        }
        for arg in &sig.args {
            if let Some(ty) = arg.type_annotation.as_ref() {
                ship_generic_reprs(
                    &mut backend,
                    ty,
                    &type_decl_table,
                    &mut emitted_generics,
                )
                .await?;
            }
        }
    }
    // Per-file LineIndex cache so traceless-warning origins can be
    // reported in the *originating file's* coordinates rather than the
    // flattened LoadedProgram's. Without this, a warning anchored at
    // the entry file ends up rendered at the merged-source line
    // (something like `prime.htcl:119767`), which is meaningless.
    let merged_line_index = vw_htcl::LineIndex::new(&source);
    let mut per_file_line_index: std::collections::HashMap<
        usize,
        vw_htcl::LineIndex,
    > = std::collections::HashMap::new();
    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        // Snapshot the origin of THIS statement before shipping
        // it, so the stream sink can tag any traceless warning
        // Vivado emits during the eval with the right "what was
        // running" anchor. Mirrors the REPL's pending_origins +
        // pending_eval_index mechanism.
        let stmt_origin = {
            let (file_path, line, snippet) = match program.locate_span(cmd.span)
            {
                Some((idx, local)) => {
                    let file_src = &program.files[idx].source;
                    let li = per_file_line_index
                        .entry(idx)
                        .or_insert_with(|| vw_htcl::LineIndex::new(file_src));
                    let (lc, _) = li.range(local);
                    let snippet = file_src
                        [local.start as usize..local.end as usize]
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    (
                        Some(program.files[idx].path.clone()),
                        lc.line + 1,
                        snippet,
                    )
                }
                None => {
                    // Synthetic span that doesn't lie in any loaded
                    // file (e.g. a generated dispatcher). Fall back to
                    // the merged-source line; the entry-file label is
                    // attached by the renderer when `file` is None.
                    let (lc, _) = merged_line_index.range(cmd.span);
                    let snippet = source
                        [cmd.span.start as usize..cmd.span.end as usize]
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    (None, lc.line + 1, snippet)
                }
            };
            let origin = vw_repl::Origin {
                file: file_path,
                line,
                snippet,
                via: Vec::new(),
            };
            if let Ok(mut g) = current_origin.lock() {
                *g = Some(origin.clone());
            }
            origin
        };
        // Overload specializations lower under their mangled
        // names so the dispatcher's switch arms can find them.
        let lowered = match overload_specialization_mangle(cmd, &overload_table)
        {
            Some(mangled) => {
                let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
                    unreachable!()
                };
                vw_htcl::lower_proc_decl_with_name_and_index(
                    proc,
                    &source,
                    &table,
                    Some(&mangled),
                    &putr_map,
                    &merged_line_index,
                )
            }
            None => vw_htcl::lower_command_with_putr_and_index(
                cmd,
                &source,
                &table,
                &putr_map,
                &merged_line_index,
            ),
        };
        // Rewrite `extern::name` → `::name` (the textual pass the
        // REPL also runs) so calls to runtime-Tcl/Vivado procs
        // reach Vivado as the bare native name instead of the
        // literal `extern::` text — without this, every wrapper
        // body that forwards via `extern::` errors out at runtime
        // with `invalid command name "extern::create_project"`.
        let tcl = vw_htcl::rewrite_externs(&lowered).text;
        // Wrap with a shim-side origin marker so any traceless
        // warning emitted during THIS eval stays anchored to
        // `stmt_origin` — see [`vw_repl::wrap_tcl_with_origin_marker`]
        // for the race this fixes.
        let tcl = vw_repl::wrap_tcl_with_origin_marker(&tcl, &stmt_origin);
        // `set VAR <expr>` is a binding — the user asked to name a
        // value, not to display it. Vivado's Tcl returns the
        // bound value as the eval result, and echoing that would
        // leak the raw internal form (e.g. `metroid` from
        // `set proj [create_project … -name metroid]` in
        // `project.htcl`). Suppress the result echo for set
        // bindings so the batch path matches the REPL's
        // `is_set_binding` policy (`vw-repl/src/app.rs:2437`).
        let is_set_binding = matches!(cmd.kind, vw_htcl::CommandKind::Set);
        match backend.eval(&tcl).await {
            Ok(out) => {
                // Puts output already streamed to stdout via the
                // sink; `out.stdout` is empty here by contract. The
                // eval's return value gets a newline only when it's
                // not already empty AND the source command wasn't a
                // set binding.
                if !out.value.is_empty() && !is_set_binding {
                    println!("{}", out.value);
                }
            }
            Err(vw_eda::BackendError::Tcl { message, .. }) => {
                eprintln!("{} {message}", "vivado:".bright_red());
            }
            Err(e) => {
                eprintln!("{} {e}", "vivado:".bright_red());
            }
        }
    }
    let _ = backend.shutdown().await;
    Ok(())
}
