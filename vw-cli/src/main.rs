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
            short,
            long,
            help = "Forward Vivado's banner and info messages to stderr"
        )]
        verbose: bool,
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
    },
    #[command(
        about = "Parse and run analysis on htcl files without executing them"
    )]
    Check {
        #[arg(required = true, help = "One or more .htcl source files")]
        files: Vec<Utf8PathBuf>,
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
                            VersionInfo::Local => " (local)".to_string(),
                            VersionInfo::Unknown => String::new(),
                        };

                        println!(
                            "  {} - {}{}",
                            dep.name.cyan(),
                            dep.source,
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
            verbose,
        } => {
            if let Err(e) = run_htcl(&file, check, verbose).await {
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
        } => {
            if let Err(e) = vw_repl::run(vw_repl::ReplOptions {
                verbose,
                initial_load,
            })
            .await
            {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
        }
        Commands::Check { files } => {
            let mut had_errors = false;
            for file in &files {
                match check_htcl(file).await {
                    Ok(file_errs) => {
                        if file_errs {
                            had_errors = true;
                        }
                    }
                    Err(e) => {
                        had_errors = true;
                        eprintln!("{} {file}: {e}", "error:".bright_red());
                    }
                }
            }
            if had_errors {
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
            } => {
                if let Err(e) = run_ip_generate(
                    &input,
                    output.as_deref(),
                    include_internal,
                    &presets,
                    no_auto_presets,
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

    let opts = vw_ip::GenerateOptions {
        user_configurable_only: !include_internal,
        ..Default::default()
    };
    let text = vw_ip::generate(&component, &presets, &dict_schemas, &opts);
    match output {
        Some(path) => std::fs::write(path, &text)
            .map_err(|e| format!("writing {path}: {e}"))?,
        None => print!("{text}"),
    }
    Ok(())
}

/// Read `entry` and recursively resolve its `src` imports. Looks for
/// a `vw.toml` in the entry file's parent chain to discover the
/// workspace; falls back to an empty resolver (so relative/absolute
/// imports still work, but `@name/` imports fail with a clear error)
/// when no workspace is found.
///
/// A [`CliObserver`] is attached so the loader's progress prints in
/// real time as `Sourcing …` / `Checking …` lines.
fn load_htcl_program(
    entry: &Utf8Path,
) -> Result<vw_htcl::LoadedProgram, Box<dyn std::error::Error>> {
    let entry_path = std::path::Path::new(entry.as_str());
    let workspace_dir = find_workspace_dir(entry);
    let mut resolver = vw_htcl::Resolver::new();
    if let Some(ws) = workspace_dir.as_deref() {
        // Transitive resolution so a library's `src @other/...`
        // import works even when the consumer hasn't redeclared
        // `other` in their own `vw.toml`.
        if let Ok(paths) = vw_lib::transitive_dep_cache_paths(ws) {
            for (name, path) in paths {
                resolver = resolver.with_dep(name, path);
            }
        }
    }
    let mut observer = CliObserver;
    Ok(vw_htcl::load_program_with_observer(
        entry_path,
        &resolver,
        &mut observer,
    )?)
}

/// Prints Cargo-style `Sourcing …` and `Checking …` lines as the
/// loader walks the dependency tree.
struct CliObserver;

impl vw_htcl::LoadObserver for CliObserver {
    fn on_source(&mut self, raw: &str) {
        println!(
            "{:>12} {}",
            "Sourcing".bright_green().bold(),
            friendly_import(raw)
        );
    }
    fn on_parsed(&mut self, file: &std::path::Path, raw: Option<&str>) {
        let label = match raw {
            Some(r) => friendly_import(r),
            None => file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
        };
        println!("{:>12} {}", "Checking".bright_green().bold(), label);
    }
}

/// Trim the `@` prefix and any trailing `.htcl` from an import path
/// so the CLI shows `amd-htcl/cpm5` rather than `@amd-htcl/cpm5.htcl`
/// or a long filesystem path.
fn friendly_import(raw: &str) -> String {
    raw.trim_start_matches('@')
        .trim_end_matches(".htcl")
        .to_string()
}

/// Walk up from `start`'s parent directory looking for a `vw.toml`.
fn find_workspace_dir(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut cur = start.parent()?.to_path_buf();
    loop {
        if cur.join("vw.toml").exists() {
            return Some(cur);
        }
        cur = cur.parent()?.to_path_buf();
    }
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
async fn check_htcl(
    file: &camino::Utf8Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let program = load_htcl_program(file)?;
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

/// For every monomorphized generic encountered while walking `ty`
/// (recursing through user-newtype underlyings), emit its repr to
/// the backend exactly once. Dedup is owned by the caller so
/// repeated invocations across signatures don't re-ship the same
/// proc.
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
            vw_vivado::StreamKind::Stdout => line.to_string(),
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
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let program = load_htcl_program(file)?;
    // Keep `program` alive — the stack-frame rewriting needs the
    // LoadedProgram for body-span resolution. We borrow `source`
    // from it instead of moving.
    let source = program.source.clone();
    let parsed = vw_htcl::parse(&source);
    let line_index = vw_htcl::LineIndex::new(&source);

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

    let mut backend =
        vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
            verbose,
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
    let (full_sigs, overload_table) =
        vw_htcl::build_signature_table_with_overloads(
            &parsed.document,
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
    let line_index = vw_htcl::LineIndex::new(&source);
    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        // Snapshot the origin of THIS statement before shipping
        // it, so the stream sink can tag any traceless warning
        // Vivado emits during the eval with the right "what was
        // running" anchor. Mirrors the REPL's pending_origins +
        // pending_eval_index mechanism.
        {
            let (line, _) = line_index.range(cmd.span);
            let snippet = source
                [cmd.span.start as usize..cmd.span.end as usize]
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            let file_path = program
                .locate_span(cmd.span)
                .map(|(idx, _)| program.files[idx].path.clone());
            if let Ok(mut g) = current_origin.lock() {
                *g = Some(vw_repl::Origin {
                    file: file_path,
                    line: line.line + 1,
                    snippet,
                    via: Vec::new(),
                });
            }
        }
        // Overload specializations lower under their mangled
        // names so the dispatcher's switch arms can find them.
        let lowered = match overload_specialization_mangle(cmd, &overload_table)
        {
            Some(mangled) => {
                let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
                    unreachable!()
                };
                vw_htcl::lower_proc_decl_with_name(
                    proc,
                    &source,
                    &table,
                    Some(&mangled),
                )
            }
            None => vw_htcl::lower_command(cmd, &source, &table),
        };
        // Rewrite `extern::name` → `::name` (the textual pass the
        // REPL also runs) so calls to runtime-Tcl/Vivado procs
        // reach Vivado as the bare native name instead of the
        // literal `extern::` text — without this, every wrapper
        // body that forwards via `extern::` errors out at runtime
        // with `invalid command name "extern::create_project"`.
        let tcl = vw_htcl::rewrite_externs(&lowered).text;
        match backend.eval(&tcl).await {
            Ok(out) => {
                // Puts output already streamed to stdout via the
                // sink; `out.stdout` is empty here by contract. The
                // eval's return value gets a newline only when it's
                // not already empty.
                if !out.value.is_empty() {
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
