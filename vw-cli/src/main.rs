// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use std::fmt;
use std::process;

use vw_eda::EdaBackend;
use vw_lib::{
    add_dependency_with_token, clear_cache, generate_deps_tcl,
    get_access_credentials_for_repo, get_access_credentials_for_workspace,
    init_workspace, list_dependencies, remove_dependency, run_testbench,
    update_workspace_with_token, VersionInfo, VhdlStandard,
};

mod bench_runner;
mod htcl_test;
mod parallel_load;
mod part_picker;
mod test_ui;

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

/// Clap ValueEnum mirror of [`vw_vivado::LogLevel`]. Lives here
/// rather than in vw-vivado so the backend crate stays clap-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CliLogLevel {
    /// Show every block raw — including Vivado's banners, tables,
    /// hierarchy summaries, and other non-diagnostic noise. The
    /// same content the raw `vivado.log` receives.
    Debug,
    /// Show INFO+ diagnostics. NONE-block noise renders dimmed
    /// (vw run) or collapsed (repl). Default.
    Info,
    /// Show WARNING+ diagnostics. NONE + INFO are elided.
    Warning,
    /// Show CRITICAL WARNING+ diagnostics. NONE + INFO + WARNING
    /// are elided.
    Critical,
    /// Show only ERRORs. Everything else is elided.
    Error,
}

impl From<CliLogLevel> for vw_vivado::LogLevel {
    fn from(lvl: CliLogLevel) -> Self {
        match lvl {
            CliLogLevel::Debug => vw_vivado::LogLevel::Debug,
            CliLogLevel::Info => vw_vivado::LogLevel::Info,
            CliLogLevel::Warning => vw_vivado::LogLevel::Warning,
            CliLogLevel::Critical => vw_vivado::LogLevel::Critical,
            CliLogLevel::Error => vw_vivado::LogLevel::Error,
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
        #[arg(
            long,
            help = "Vivado part id, e.g. xcvp1202-vsva2785-2MP-e-S. \
                    If omitted and stdout is a tty, an interactive picker \
                    is launched against the current Vivado install."
        )]
        part: Option<String>,
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
    #[command(
        about = "Run VHDL/cosim testbenches with NVC — all in parallel, nextest-style"
    )]
    Bench {
        #[arg(
            help = "Filter to testbenches whose name contains this substring; omit to run all"
        )]
        testbench: Option<String>,
        #[arg(long, help = "VHDL standard", default_value_t = CliVhdlStandard::Vhdl2019)]
        std: CliVhdlStandard,
        #[arg(
            long,
            help = "List matching testbenches instead of running them"
        )]
        list: bool,
        #[arg(
            long,
            help = "Maximum number of testbenches to run concurrently (default: CPU count)"
        )]
        concurrency: Option<usize>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Ignore directories matching these names (comma-separated or use multiple times)"
        )]
        ignore: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Runtime flags to pass to NVC (comma-separated or use multiple times)"
        )]
        runtime_flags: Vec<String>,
        #[arg(long, help = "Build Rust library for testbench before running")]
        build_rust: bool,
        #[arg(
            long,
            help = "Generate/regenerate mixed-signal scaffolding from mist.toml",
            requires = "testbench"
        )]
        scaffold: bool,
        /// Internal: run exactly one testbench into this isolated nvc build
        /// dir. Used by the parallel runner to fan out per-bench; when set,
        /// `testbench` is an exact name (not a filter) and anodization is
        /// assumed already done.
        #[arg(long, hide = true, requires = "testbench")]
        build_dir: Option<String>,
    },
    #[command(about = "Run an htcl script against a Vivado worker. \
                     With no file, discovers `<workspace>/design.htcl`.")]
    Run {
        #[arg(help = "Path to an .htcl source file. Omit to run the \
                    workspace's `design.htcl`.")]
        file: Option<Utf8PathBuf>,
        #[arg(
            long,
            help = "Parse and print diagnostics only; don't launch Vivado"
        )]
        check: bool,
        #[arg(
            long,
            value_name = "ID",
            conflicts_with = "variant",
            help = "Select a non-default `[[target-parts]]` entry by full \
                    part ID or unique substring (e.g. `--part 3HP`). \
                    Mutually exclusive with `--variant`."
        )]
        part: Option<String>,
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with = "part",
            help = "Select a non-default `[[workspace.variants]]` entry by \
                    exact name. Variants own their parts inline."
        )]
        variant: Option<String>,
        #[arg(
            long = "log-level",
            value_enum,
            default_value_t = CliLogLevel::Info,
            help = "Minimum severity to show. `debug` shows everything \
                    (including Vivado's non-diagnostic noise); INFO+ \
                    filters and dims/collapses non-diagnostic output. \
                    The full raw stream always lands in \
                    `<workspace>/target/logs/vivado-*.log`."
        )]
        log_level: CliLogLevel,
        #[arg(
            long = "info-with-stack",
            help = "Attach the Tcl call stack to INFO messages too \
                    (WARNING / ERROR / CRITICAL always include the stack)"
        )]
        info_with_stack: bool,
        #[arg(
            long = "bunyan",
            help = "Emit newline-delimited bunyan JSON log records on \
                    stdout instead of the human-readable stream, for \
                    piping into `looker` in CI. Vivado INFO → bunyan \
                    info (30), WARNING → warn (40), CRITICAL WARNING and \
                    ERROR → error (50). Non-diagnostic noise is omitted \
                    unless `--log-level=debug`. vw's own status lines \
                    stay on stderr, so stdout is pure JSON."
        )]
        bunyan: bool,
    },
    #[command(about = "Launch the vw analyzer LSP server on stdio")]
    Analyzer,
    #[command(
        about = "Interactive htcl REPL backed by a long-lived Vivado worker"
    )]
    Repl {
        #[arg(
            long = "log-level",
            value_enum,
            default_value_t = CliLogLevel::Info,
            help = "Minimum severity shown in scrollback. `debug` shows \
                    every block raw; INFO+ collapses non-diagnostic \
                    output into a togglable placeholder."
        )]
        log_level: CliLogLevel,
        #[arg(
            long = "load",
            value_name = "FILE",
            conflicts_with_all = [
                "from_synth_checkpoint",
                "from_place_checkpoint",
                "from_route_checkpoint",
            ],
            help = "Source FILE into the session as soon as Vivado is up"
        )]
        initial_load: Option<Utf8PathBuf>,
        #[arg(
            long = "from-synth-checkpoint",
            conflicts_with_all = [
                "from_place_checkpoint",
                "from_route_checkpoint",
            ],
            help = "Skip design.htcl. On boot, open \
                    <ws>/target/synth/<top>.dcp instead. Errors if the \
                    checkpoint is missing."
        )]
        from_synth_checkpoint: bool,
        #[arg(
            long = "from-place-checkpoint",
            conflicts_with_all = ["from_route_checkpoint"],
            help = "Skip design.htcl. On boot, open \
                    <ws>/target/place/<top>.dcp instead. Errors if the \
                    checkpoint is missing."
        )]
        from_place_checkpoint: bool,
        #[arg(
            long = "from-route-checkpoint",
            help = "Skip design.htcl. On boot, open \
                    <ws>/target/route/<top>.dcp instead. Errors if the \
                    checkpoint is missing."
        )]
        from_route_checkpoint: bool,
        #[arg(
            long,
            value_name = "ID",
            conflicts_with = "variant",
            help = "Select a non-default `[[target-parts]]` entry by full \
                    part ID or unique substring. Mutually exclusive with \
                    `--variant`."
        )]
        part: Option<String>,
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with = "part",
            help = "Select a non-default `[[workspace.variants]]` entry by \
                    exact name."
        )]
        variant: Option<String>,
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
            conflicts_with_all = ["all_parts", "variant", "all_variants"],
            help = "Check against a specific `[[target-parts]]` entry by \
                    full part ID or unique substring. Default: the workspace's \
                    default part."
        )]
        part: Option<String>,
        #[arg(
            long = "all-parts",
            conflicts_with_all = ["part", "variant", "all_variants"],
            help = "Check against every declared `[[target-parts]]` entry \
                    instead of just the default"
        )]
        all_parts: bool,
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with_all = ["part", "all_parts", "all_variants"],
            help = "Check against a specific `[[workspace.variants]]` entry \
                    by exact name. Default: the workspace's default variant."
        )]
        variant: Option<String>,
        #[arg(
            long = "all-variants",
            conflicts_with_all = ["part", "all_parts", "variant"],
            help = "Check against every declared `[[workspace.variants]]` \
                    entry (each variant's `.part`) instead of just the \
                    default"
        )]
        all_variants: bool,
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
            conflicts_with = "variant",
            help = "Select the workspace-default `[[target-parts]]` entry \
                    for tests without their own `@test(target=…)`; matches \
                    by full ID or unique substring"
        )]
        part: Option<String>,
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with = "part",
            help = "Select a specific `[[workspace.variants]]` entry as the \
                    default for tests without their own \
                    `@test(variant=…)`"
        )]
        variant: Option<String>,
        #[arg(
            long = "log-level",
            value_enum,
            default_value_t = CliLogLevel::Info,
            help = "Minimum severity to show during test evals. See \
                    `vw run --help` for the full model."
        )]
        log_level: CliLogLevel,
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

// Netrc credential lookup moved to `vw_lib` so `vw-vivado`'s
// RPC auto-update path can reuse it. See
// `vw_lib::get_access_credentials_for_workspace`.

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
        Commands::Init { name, part } => {
            let target_part = resolve_init_target_part(part);
            if let Err(e) =
                init_workspace(&cwd, name.clone(), target_part.clone())
            {
                eprintln!("{} {e}", "error:".bright_red());
                process::exit(1);
            }
            println!(
                "{} Initialized workspace: {}",
                "✓".bright_green(),
                name.cyan()
            );
            match &target_part {
                Some(p) => println!("  target part: {}", p.cyan()),
                None => println!(
                    "  {} target-parts is empty; edit vw.toml or re-run `vw init` with --part when ready.",
                    "note:".yellow()
                ),
            }
        }
        Commands::Update => {
            let access_creds =
                get_access_credentials_for_workspace(&cwd, false);
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
            let access_creds = get_access_credentials_for_repo(&repo);
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
            concurrency,
            ignore,
            runtime_flags,
            build_rust,
            scaffold,
            build_dir,
        } => {
            // Self-heal a fresh checkout like `vw check`. Skip the
            // internal `--build-dir` subprocess (the parent runner
            // already fetched — re-fetching per bench would race) and
            // `--list` (pure metadata, no deps needed).
            if build_dir.is_none() && !list {
                ensure_workspace_deps(&cwd).await;
            }
            if let Some(bd) = build_dir {
                // Internal single-exec mode — the parallel runner fans out one
                // subprocess per bench into an isolated build dir. Run exactly
                // `testbench`; anodization was already done by the runner. This
                // process's stdout/stderr is captured by the runner and shown
                // only if the bench fails.
                let name =
                    testbench.expect("--build-dir requires a testbench name");
                println!("Running testbench: {}", name.cyan());
                if let Err(e) = run_testbench(
                    &cwd,
                    name,
                    std.into(),
                    true,
                    &runtime_flags,
                    build_rust,
                    scaffold,
                    &bd,
                )
                .await
                {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            } else if scaffold {
                // Scaffolding is a single-bench, no-simulation operation.
                let name =
                    testbench.expect("--scaffold requires a testbench name");
                match run_testbench(
                    &cwd,
                    name.clone(),
                    std.into(),
                    true,
                    &runtime_flags,
                    build_rust,
                    true,
                    vw_lib::BUILD_DIR,
                )
                .await
                {
                    Ok(()) => println!(
                        "{} Scaffolding generated for '{}'",
                        "✓".bright_green(),
                        name
                    ),
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                }
            } else {
                // Parallel nextest-style runner: no positional runs every
                // testbench; a positional filters by name substring.
                let conc = concurrency.unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                });
                if let Err(e) = bench_runner::run_benches(
                    &cwd,
                    testbench.as_deref(),
                    list,
                    conc,
                    std.into(),
                    &ignore,
                )
                .await
                {
                    eprintln!("{} {e}", "error:".bright_red());
                    process::exit(1);
                }
            }
        }
        Commands::Run {
            file,
            check,
            part,
            variant,
            log_level,
            info_with_stack,
            bunyan,
        } => {
            // Self-heal a fresh checkout: fetch missing deps the same
            // way `vw check` does before resolving `src @dep` imports.
            ensure_workspace_deps(&cwd).await;
            let resolved = match file {
                Some(f) => f,
                None => match discover_entry_file(&cwd) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                },
            };
            if let Err(e) = run_htcl(
                &resolved,
                check,
                part.as_deref(),
                variant.as_deref(),
                log_level.into(),
                info_with_stack,
                bunyan,
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
            log_level,
            initial_load,
            from_synth_checkpoint,
            from_place_checkpoint,
            from_route_checkpoint,
            part,
            variant,
            info_with_stack,
        } => {
            // `--from-*-checkpoint` short-circuits every other
            // load path: skip `design.htcl` auto-discovery, skip
            // `--load`, and instead resolve the requested DCP
            // Rust-side so we can fail with a clear error BEFORE
            // spawning Vivado if the checkpoint is missing.
            // Clap already gates mutual exclusivity, so at most
            // one of the three is true here.
            let from_stage = if from_synth_checkpoint {
                Some("synth")
            } else if from_place_checkpoint {
                Some("place")
            } else if from_route_checkpoint {
                Some("route")
            } else {
                None
            };
            let (resolved_load, initial_source) = if let Some(stage) =
                from_stage
            {
                match resolve_checkpoint_path(&cwd, stage, variant.as_deref()) {
                    Ok(path) => {
                        // `src @vivado-cmd` first so the namespaced
                        // wrapper is defined — a fresh REPL boot
                        // hasn't loaded any modules yet, and
                        // calling `vivado_cmd::open_checkpoint`
                        // without it fires an "undefined proc"
                        // error. Absolute path in braces so cwd
                        // shifts inside the worker can't reroute.
                        let snippet = format!(
                            "src @vivado-cmd\n\
                             vivado_cmd::open_checkpoint -file {{{path}}}\n",
                        );
                        (None, Some(snippet))
                    }
                    Err(e) => {
                        eprintln!("{} {e}", "error:".bright_red());
                        process::exit(1);
                    }
                }
            } else {
                // Same `design.htcl` auto-discovery as `vw run`:
                // if the user didn't pass `--load`, look for a
                // `design.htcl` in the enclosing workspace and
                // source it up front. Silent no-op when there's
                // no workspace or no `design.htcl` — the REPL
                // still boots and the user can `:load` something
                // explicitly.
                // Self-heal a fresh checkout: fetch missing deps
                // the same way `vw check` does before the REPL
                // resolves `src @dep`.
                ensure_workspace_deps(&cwd).await;
                let resolved = initial_load.or_else(|| {
                    vw_lib::find_workspace_dir(cwd.as_std_path())
                        .and_then(|ws| vw_lib::find_design_file(&ws))
                });
                (resolved, None)
            };
            if let Err(e) = vw_repl::run(vw_repl::ReplOptions {
                log_level: log_level.into(),
                initial_load: resolved_load,
                initial_source,
                part,
                variant,
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
            variant,
            all_variants,
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
                     directory with a `vw.toml` that has a `design.htcl`, \
                     `module.htcl`, or `test/*.htcl`",
                    "note:".bright_yellow(),
                );
                return;
            }
            // Transparently fetch missing dependencies before checking
            // — the way `cargo` fetches absent deps rather than erroring
            // on an unresolved import. Shared with `run`/`bench`/`repl`.
            ensure_workspace_deps(&cwd).await;
            let mut had_errors = false;
            // conflicts_with on the clap args guarantees at most
            // one non-Default source at a time. Map both flag
            // families to the same PartSelector; the resolver
            // decides which config block applies based on the
            // workspace shape.
            let part_selector = if let Some(v) = variant.as_deref() {
                PartSelector::Explicit(v.to_string())
            } else if all_variants {
                PartSelector::All
            } else {
                PartSelector::from_flags(part.as_deref(), all_parts)
            };
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
            // VHDL static analysis (vhdl_ls) over the workspace's own
            // HDL — the same checks the editor surfaces live, run in
            // batch alongside the htcl check. Skipped entirely for a
            // pure-htcl workspace (nothing rendered into a VHDL library).
            if let Some(ws) = vw_lib::find_workspace_dir(cwd.as_std_path()) {
                if vw_lib::workspace_has_vhdl(&ws, None) {
                    // Make sure the VHDL standard library is available —
                    // fetched into the dep cache on first use so a
                    // machine without a system rust_hdl install still
                    // analyzes. On failure, fall back to vhdl_lang's
                    // built-in search (`None`), which just skips VHDL if
                    // nothing is found.
                    let stdlib = vw_lib::ensure_vhdl_stdlib().await.ok();
                    let mut vhdl_result =
                        vw_lib::check_vhdl(&ws, None, stdlib.as_deref());
                    // A design that instantiates `entity ip.<x>` /
                    // `entity xil_defaultlib.<x>` needs Vivado-generated
                    // wrappers/stubs under `target/`. Whether the
                    // SPECIFIC referenced entity is present can't be
                    // judged from the library being non-empty — the BD
                    // wrappers populate `xil_defaultlib` while a
                    // standalone XCI IP's stub can still be missing — so
                    // we let the analyzer be the judge: if it can't
                    // resolve a unit within those libraries, generate the
                    // IP in-process (configure_ip + generate_ip_targets)
                    // and re-analyze. That pass is skip-gated and
                    // `generate_target`-only, so it's cheap; and we only
                    // reach it on a real resolution failure, never
                    // speculatively. A generation that doesn't complete
                    // cleanly isn't fatal — the re-analysis reports the
                    // true remaining state.
                    if matches!(&vhdl_result, Ok(diags)
                        if diagnostics_need_ip_generation(diags))
                    {
                        println!(
                            "{} generating IP for `ip`/`xil_defaultlib` \
                             (vw::configure_ip)…",
                            "note:".bright_yellow(),
                        );
                        if let Err(e) = ensure_ip_generated(
                            &ws,
                            None,
                            None,
                            // Terse: the check only cares about VHDL
                            // resolution, so suppress the Vivado
                            // firehose (NONE/INFO) and surface just
                            // Warning/CriticalWarning/Error from the
                            // IP-generation pass.
                            vw_vivado::LogLevel::Warning,
                        )
                        .await
                        {
                            eprintln!(
                                "{} IP generation did not complete \
                                 cleanly: {e}",
                                "warning:".yellow(),
                            );
                        }
                        vhdl_result =
                            vw_lib::check_vhdl(&ws, None, stdlib.as_deref());
                    }
                    match vhdl_result {
                        Ok(diags) if !diags.is_empty() => {
                            let cwd_owned = std::env::current_dir().ok();
                            let cwd_ref = cwd_owned.as_deref();
                            let (mut errs, mut warns) = (0usize, 0usize);
                            for d in &diags {
                                let label = match d.severity {
                                    vw_lib::VhdlSeverity::Error => {
                                        errs += 1;
                                        "error:".bright_red()
                                    }
                                    vw_lib::VhdlSeverity::Warning => {
                                        warns += 1;
                                        "warning:".yellow()
                                    }
                                    vw_lib::VhdlSeverity::Info => {
                                        "info:".cyan()
                                    }
                                    vw_lib::VhdlSeverity::Hint => {
                                        "hint:".cyan()
                                    }
                                };
                                let path = render_path(&d.file, cwd_ref);
                                eprintln!(
                                    "{label} {path}:{}:{}: {}",
                                    d.line, d.column, d.message,
                                );
                            }
                            eprintln!(
                                "VHDL: {errs} error(s), {warns} warning(s)"
                            );
                            if errs > 0 {
                                had_errors = true;
                            }
                        }
                        Ok(_) => {} // no VHDL, or no findings
                        Err(e) => {
                            had_errors = true;
                            eprintln!(
                                "{} vhdl check failed: {e}",
                                "error:".bright_red(),
                            );
                        }
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
            variant,
            log_level,
            info_with_stack,
        } => {
            if let Err(e) = htcl_test::run_htcl_tests(
                &cwd,
                filter,
                list,
                test_threads,
                part.as_deref(),
                variant.as_deref(),
                log_level.into(),
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

/// Decide what part to seed a new workspace's `target-parts` with.
///
/// Explicit `--part <p>` on the command line always wins. Otherwise
/// launch the interactive picker when stdout is a tty and a Vivado
/// install can be located; on non-tty, no-Vivado, or user cancel
/// (Esc), fall back to `None` and print a hint so `vw init` still
/// succeeds and the user can hand-edit `vw.toml` later.
fn resolve_init_target_part(explicit: Option<String>) -> Option<String> {
    if let Some(part) = explicit {
        return Some(part);
    }
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "{} `vw init` running non-interactively without `--part`; \
             workspace will have empty target-parts.",
            "note:".yellow()
        );
        return None;
    }
    let Some(install) = vw_lib::parts::find_vivado_install() else {
        eprintln!(
            "{} could not locate Vivado (`which vivado`, $XILINX_VIVADO); \
             workspace will have empty target-parts. Re-run with `--part` \
             once Vivado is on PATH.",
            "note:".yellow()
        );
        return None;
    };
    let parts = vw_lib::parts::enumerate_parts(&install);
    if parts.is_empty() {
        eprintln!(
            "{} Vivado install at {install} contains no plaintext parts \
             catalog; workspace will have empty target-parts.",
            "note:".yellow()
        );
        return None;
    }
    match part_picker::pick_part(&parts) {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            eprintln!(
                "{} no part picked; workspace will have empty target-parts.",
                "note:".yellow()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "{} part picker failed: {e}; workspace will have empty target-parts.",
                "warning:".yellow()
            );
            None
        }
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
/// Transparently fetch missing dependencies before a workspace op —
/// the way `cargo` fetches absent deps rather than erroring on an
/// unresolved import. Cheap, offline no-op when everything is already
/// cached; only reaches the network (via the `vw update` machinery)
/// when a declared git dep isn't materialized (fresh checkout,
/// `vw clear`, or a newly-added dep). Also surgically prunes a stale
/// git lock entry left by a `repo → path` switch. Exits the process on
/// a fetch failure. Shared by `vw check` / `run` / `bench` / `repl` so
/// they all self-heal a fresh checkout the same way.
/// Resolve `<ws>/target/<stage>/<top>.dcp` for a `--from-*-checkpoint`
/// invocation and confirm it exists on disk. Errors when there's no
/// workspace, no top-entity resolution, or no DCP at the derived path.
///
/// `stage` must be one of `"synth" | "place" | "route"` — matches
/// the on-disk convention in `~/src/htcl/vw/module.htcl` (see the
/// `checkpoint_path` writes in `vw::synth` / `vw::place` /
/// `vw::route`).
fn resolve_checkpoint_path(
    cwd: &Utf8Path,
    stage: &str,
    variant_query: Option<&str>,
) -> Result<Utf8PathBuf, String> {
    let ws =
        vw_lib::find_workspace_dir(cwd.as_std_path()).ok_or_else(|| {
            "`--from-*-checkpoint` requires a workspace (nearest `vw.toml`)"
                .to_string()
        })?;
    let cfg = vw_lib::load_workspace_config(&ws)
        .map_err(|e| format!("failed to load vw.toml: {e}"))?;
    // Resolve the active variant name the same way the auto-project
    // does: caller-supplied → workspace default → None. That keeps
    // `<top>` consistent with what `vw::synth` / `vw::place` /
    // `vw::route` used when they wrote the checkpoint.
    let active_variant = if !cfg.workspace.variants.is_empty() {
        cfg.workspace
            .select_variant(variant_query)
            .map_err(|e| e.to_string())?
            .map(|v| v.name.clone())
    } else {
        None
    };
    let top = cfg
        .workspace
        .resolve_top(active_variant.as_deref())
        .ok_or_else(|| {
            format!(
                "`--from-{stage}-checkpoint` needs a top-entity: set \
                 `top = \"...\"` at the workspace or variant level in \
                 `{ws}/vw.toml`",
            )
        })?;
    let path = ws.join("target").join(stage).join(format!("{top}.dcp"));
    if !path.exists() {
        return Err(format!(
            "checkpoint not found: {path}\n\
             hint: run `vw::{stage}` (or the full flow through it) to \
             produce the checkpoint before using `--from-{stage}-checkpoint`.",
        ));
    }
    Ok(path)
}

async fn ensure_workspace_deps(cwd: &Utf8Path) {
    let Some(ws) = vw_lib::find_workspace_dir(cwd.as_std_path()) else {
        return;
    };
    // A `repo → path` switch in `vw.toml` leaves the old git pin in
    // `vw.lock`, which would shadow the path dep at resolution time.
    // Drop just those stale entries — surgically, so no other git dep
    // gets re-resolved/bumped.
    match vw_lib::prune_stale_path_deps_from_lock(&ws) {
        Ok(true) => println!(
            "{} updated vw.lock (a dependency changed to a path dep)",
            "note:".bright_yellow(),
        ),
        Ok(false) => {}
        Err(e) => {
            eprintln!("{} could not update vw.lock: {e}", "warning:".yellow(),)
        }
    }
    if !vw_lib::dependencies_present(&ws) {
        println!("{} fetching missing dependencies…", "note:".bright_yellow());
        let creds = get_access_credentials_for_workspace(&ws, false);
        if let Err(e) = update_workspace_with_token(&ws, creds).await {
            eprintln!(
                "{} failed to fetch dependencies: {e}",
                "error:".bright_red()
            );
            process::exit(1);
        }
    }
}

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
/// workspace directory with no explicit file list. Picks up
/// `<ws>/design.htcl` (project entry) and `<ws>/module.htcl`
/// (library entry) — either or both may be present — plus every
/// `<ws>/test/**/*.htcl` (checked in test-mode).
///
/// Errors when we can't find the enclosing workspace at all —
/// otherwise returns an empty vec, letting the caller print
/// "nothing to check" without treating it as a hard failure.
/// Resolve the CLI's `--part` / `--variant` flags against the
/// workspace's declared parts / variants and return the
/// `(auto_project, active_variant)` pair the RPC and Vivado
/// spawn need. Applies mutual-exclusion / mode-mismatch rules:
///
/// - `--variant` on a part-mode workspace (no
///   `[[workspace.variants]]`) → error.
/// - `--part` on a variant-mode workspace → error (variants own
///   their parts).
/// - Neither flag → workspace default (either the default
///   target-part or the default variant's `.part`).
///
/// The `active_variant` slot flows into the RPC session so
/// `vw::vhdl_design_sources` filters by the CLI-picked variant
/// automatically, without design.htcl having to name it.
pub(crate) fn resolve_workspace_selection(
    ws: &camino::Utf8Path,
    part: Option<&str>,
    variant: Option<&str>,
) -> Result<(Option<vw_vivado::AutoProject>, Option<String>), String> {
    let Ok(cfg) = vw_lib::load_workspace_config(ws) else {
        return Ok((None, None));
    };
    let ws_info = &cfg.workspace;
    // Mode-mismatch guards.
    if variant.is_some() && ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} has no `[[workspace.variants]]` block; \
             remove `--variant` or add variants to vw.toml",
        ));
    }
    if part.is_some() && !ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} is variant-mode (has \
             `[[workspace.variants]]`); use `--variant <name>` instead of \
             `--part` — variants own their parts inline",
        ));
    }
    // Resolve to a specific variant (variant mode) or part (part mode).
    if !ws_info.variants.is_empty() {
        let selected =
            ws_info.select_variant(variant).map_err(|e| e.to_string())?;
        let Some(v) = selected else {
            return Ok((None, None));
        };
        let persist_dir = prepare_persist_dir(ws, &ws_info.name)?;
        Ok((
            Some(vw_vivado::AutoProject {
                name: ws_info.name.clone(),
                part: v.part.clone(),
                persist_dir,
            }),
            Some(v.name.clone()),
        ))
    } else {
        let selected = ws_info
            .select_target_part(part)
            .map_err(|e| e.to_string())?;
        let persist_dir = prepare_persist_dir(ws, &ws_info.name)?;
        Ok((
            selected.map(|p| vw_vivado::AutoProject {
                name: ws_info.name.clone(),
                part: p.to_string(),
                persist_dir: persist_dir.clone(),
            }),
            None,
        ))
    }
}

/// Shared bootstrap for the on-disk Vivado project dir used by
/// `vw run` and `vw repl`. Runs the one-shot legacy IP-cache
/// cleanup + staleness wipe, then returns
/// `Some(<ws>/target/vw-project)` on success.
///
/// A failure here is not fatal — we log the reason and fall back
/// to `persist_dir: None` (in-memory project), so the user still
/// gets a working session. That covers e.g. a read-only workspace
/// or the `target/` dir being held by another process.
fn prepare_persist_dir(
    ws: &camino::Utf8Path,
    name: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    match vw_lib::prepare_vw_project_dir(ws, name) {
        Ok(prep) => {
            if prep.legacy_cache_removed > 0 {
                eprintln!(
                    "{} removed {} legacy IP cache entr{y} under \
                     {ws}/target/ip — replaced by on-disk Vivado project",
                    "info:".cyan(),
                    prep.legacy_cache_removed,
                    y = if prep.legacy_cache_removed == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                );
            }
            if let Some(wiped) = &prep.wiped_project {
                eprintln!(
                    "{} wiped stale Vivado project at {wiped} \
                     (source fingerprint changed or manifest missing)",
                    "info:".cyan(),
                );
            }
            Ok(Some(prep.project_dir.into_std_path_buf()))
        }
        Err(e) => {
            eprintln!(
                "{} failed to prepare on-disk Vivado project dir under \
                 {ws}/target/vw-project ({e}); falling back to in-memory \
                 project (state won't persist across sessions)",
                "warning:".bright_yellow(),
            );
            Ok(None)
        }
    }
}

/// Locate the workspace's `design.htcl` for a bare `vw run` with
/// no file argument. Errors with a targeted message when the
/// workspace doesn't have one — the user's next move is either
/// `vw run <file>` or creating a `design.htcl`.
fn discover_entry_file(
    cwd: &Utf8Path,
) -> Result<Utf8PathBuf, Box<dyn std::error::Error>> {
    let ws = vw_lib::find_workspace_dir(cwd.as_std_path())
        .ok_or("not in a vw workspace (no vw.toml in the parent chain)")?;
    vw_lib::find_design_file(&ws).ok_or_else(|| {
        format!(
            "no `design.htcl` in {ws}; pass a file explicitly \
             (`vw run <file>`) or create a `design.htcl`",
        )
        .into()
    })
}

fn discover_check_targets(
    cwd: &Utf8Path,
) -> Result<Vec<CheckTarget>, Box<dyn std::error::Error>> {
    let ws = vw_lib::find_workspace_dir(cwd.as_std_path())
        .ok_or("not in a vw workspace (no vw.toml in the parent chain)")?;
    let mut targets = Vec::new();
    if let Some(design) = vw_lib::find_design_file(&ws) {
        targets.push(CheckTarget {
            path: design,
            include_test_deps: false,
        });
    }
    let module = ws.join("module.htcl");
    if module.is_file() {
        targets.push(CheckTarget {
            path: module,
            include_test_deps: false,
        });
    }
    // `ip/module.htcl` is sourced implicitly at runtime by
    // `vw::configure_ip` (which walks `<ws>/ip/**` via
    // `list_ip_htcl_files`), so static discovery didn't include
    // it and typos in ip-tree procs never surfaced via
    // `vw check`. Add it explicitly so bare `vw check` catches
    // the same class of errors the LSP does.
    let ip_module = ws.join("ip/module.htcl");
    if ip_module.is_file() {
        targets.push(CheckTarget {
            path: ip_module,
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
    /// Handles both the legacy `[[target-parts]]` shape and the
    /// new `[[workspace.variants]]` shape (variants own their
    /// parts inline; the compat check treats them the same way
    /// once the part is extracted). Returns an empty vec for a
    /// library workspace (neither block populated) — caller
    /// treats that as "no compat check."
    fn resolve<'a>(
        &self,
        ws_info: &'a vw_lib::WorkspaceInfo,
    ) -> std::result::Result<Vec<&'a str>, String> {
        if !ws_info.variants.is_empty() {
            return self.resolve_variant_mode(ws_info);
        }
        if ws_info.target_parts.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            Self::Default => Ok(ws_info
                .default_target_part()
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect()),
            Self::Explicit(q) => Ok(ws_info
                .select_target_part(Some(q))
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect()),
            Self::All => Ok(ws_info
                .target_parts
                .iter()
                .map(|p| p.part.as_str())
                .collect()),
        }
    }

    fn resolve_variant_mode<'a>(
        &self,
        ws_info: &'a vw_lib::WorkspaceInfo,
    ) -> std::result::Result<Vec<&'a str>, String> {
        match self {
            Self::Default => Ok(ws_info
                .default_variant()
                .map_err(|e| e.to_string())?
                .map(|v| v.part.as_str())
                .into_iter()
                .collect()),
            Self::Explicit(q) => Ok(ws_info
                .select_variant(Some(q))
                .map_err(|e| e.to_string())?
                .map(|v| v.part.as_str())
                .into_iter()
                .collect()),
            Self::All => {
                Ok(ws_info.variants.iter().map(|v| v.part.as_str()).collect())
            }
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

/// Encode a [`vw_vivado::Severity`] as a `u8` matching the ladder's
/// `PartialOrd`. Used for `AtomicU8::fetch_max` on the "worst seen
/// during this session" counter — atomic comparison over the raw
/// integer is thread-safe without needing a mutex, and the mapping
/// is a static one-liner rather than a lookup table.
fn severity_as_u8(s: vw_vivado::Severity) -> u8 {
    match s {
        vw_vivado::Severity::None => 0,
        vw_vivado::Severity::Info => 1,
        vw_vivado::Severity::Warning => 2,
        vw_vivado::Severity::CriticalWarning => 3,
        vw_vivado::Severity::Error => 4,
    }
}

/// Render one classified [`vw_vivado::Block`] to stdout, applying the
/// user-selected log level:
///
/// - `Block::Diagnostic { severity, .. }` — passes through
///   [`render_chunk`] with the block joined back into a single
///   chunk, provided the severity clears `log_level`.
/// - `Block::None { .. }` — non-diagnostic Vivado stdout (banners,
///   `VHDL Output written …`, block-design `Slave segment …` chatter,
///   a design's own `puts`/`putr`). At `LogLevel::Debug` it renders
///   raw via the Stdout path (every byte). At `Info` it dims each
///   line so users still see noise flowed through without it
///   dominating. At `Warning`+ it's elided entirely — the caller
///   explicitly asked for terse output (only Warning/Critical/Error),
///   e.g. `vw check`'s IP pre-pass, where this stdout is pure barf.
fn render_block(
    block: &vw_vivado::Block,
    log_level: vw_vivado::LogLevel,
    proc_table: &std::collections::HashMap<String, vw_repl::ProcLocation>,
    origin: Option<&vw_repl::Origin>,
    input_file: Option<&std::path::Path>,
) {
    use colored::Colorize;
    match block {
        vw_vivado::Block::Diagnostic { severity, lines } => {
            if !log_level.allows(*severity) {
                return;
            }
            let kind = vw_vivado::stream_kind_for(*severity);
            let joined = lines.join("\n");
            render_chunk(kind, &joined, proc_table, origin, input_file);
        }
        vw_vivado::Block::None { lines } => match log_level {
            vw_vivado::LogLevel::Debug => {
                // Raw pass-through — same style as if it hadn't been
                // block-grouped at all.
                let joined = lines.join("\n");
                render_chunk(
                    vw_vivado::StreamKind::Stdout,
                    &joined,
                    proc_table,
                    origin,
                    input_file,
                );
            }
            vw_vivado::LogLevel::Info => {
                // Dim so users still see noise flowed through.
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                for line in lines {
                    let _ = writeln!(out, "{}", line.dimmed());
                }
                let _ = out.flush();
            }
            // Warning/Critical/Error: terse — drop non-diagnostic
            // output entirely.
            _ => {}
        },
    }
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
    // Drop a single trailing newline, resolve stack frames to real
    // htcl source, and tag traceless warnings/errors with the current
    // origin. Shared with the bunyan emitter via this helper so both
    // surfaces show identical, source-resolved message text.
    let tagged =
        resolve_diagnostic_text(kind, chunk, proc_table, origin, input_file);
    if tagged.is_empty() {
        return;
    }
    let prefix = match kind {
        vw_vivado::StreamKind::Error
        | vw_vivado::StreamKind::CriticalWarning => "✗ ",
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
            vw_vivado::StreamKind::Error
            | vw_vivado::StreamKind::CriticalWarning => {
                leading.red().bold().to_string()
            }
            vw_vivado::StreamKind::Warning => {
                leading.truecolor(255, 140, 0).bold().to_string()
            }
            vw_vivado::StreamKind::Info => leading.bright_black().to_string(),
            vw_vivado::StreamKind::Stdout => leading.to_string(),
        };
        let styled_line: String = match kind {
            vw_vivado::StreamKind::Error
            | vw_vivado::StreamKind::CriticalWarning => line.red().to_string(),
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

/// Resolve a diagnostic chunk's `at <input>:N in ::proc` frames back to
/// real htcl source and tag traceless warnings/errors with the current
/// origin. Shared by the human renderer ([`render_chunk`]) and the
/// bunyan emitter ([`emit_bunyan_block`]) so both surfaces show
/// identical, source-resolved text. Trims a single trailing newline;
/// returns `""` for an otherwise-empty chunk.
fn resolve_diagnostic_text(
    kind: vw_vivado::StreamKind,
    chunk: &str,
    proc_table: &std::collections::HashMap<String, vw_repl::ProcLocation>,
    origin: Option<&vw_repl::Origin>,
    input_file: Option<&std::path::Path>,
) -> String {
    let trimmed = chunk.trim_end_matches('\n');
    if trimmed.is_empty() {
        return String::new();
    }
    let resolved = vw_repl::resolve_stack_frames_with(
        trimmed,
        |name| proc_table.get(name).cloned(),
        input_file,
    );
    // Tag traceless warnings/errors with the currently-executing
    // statement's origin — Vivado's C++ IP-Flow paths bypass
    // `::common::send_msg_id` and arrive without an `at …` frame.
    match kind {
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
    }
}

/// Emit one classified [`vw_vivado::Block`] as a bunyan JSON record on
/// stdout (the `--bunyan` path). Diagnostic blocks map their severity to
/// a bunyan level and honor the `--log-level` filter exactly like the
/// human renderer; non-diagnostic NONE blocks only enter the structured
/// stream at the `--log-level=debug` escape hatch (they always remain in
/// the raw `vivado-*.log`). Filtered-out blocks are a no-op.
fn emit_bunyan_block(
    block: &vw_vivado::Block,
    log_level: vw_vivado::LogLevel,
    proc_table: &std::collections::HashMap<String, vw_repl::ProcLocation>,
    origin: Option<&vw_repl::Origin>,
    input_file: Option<&std::path::Path>,
    hostname: &str,
    pid: u32,
) {
    match block {
        vw_vivado::Block::Diagnostic { severity, lines } => {
            if !log_level.allows(*severity) {
                return;
            }
            let kind = vw_vivado::stream_kind_for(*severity);
            let joined = lines.join("\n");
            let msg = resolve_diagnostic_text(
                kind, &joined, proc_table, origin, input_file,
            );
            if msg.is_empty() {
                return;
            }
            emit_bunyan_line(
                bunyan_level_for(*severity),
                "vivado",
                Some(bunyan_severity_label(*severity)),
                &msg,
                hostname,
                pid,
            );
        }
        vw_vivado::Block::None { lines } => {
            if !matches!(log_level, vw_vivado::LogLevel::Debug) {
                return;
            }
            let msg = lines.join("\n");
            if msg.trim().is_empty() {
                return;
            }
            emit_bunyan_line(
                bunyan_level_for(vw_vivado::Severity::None),
                "vivado",
                Some(bunyan_severity_label(vw_vivado::Severity::None)),
                &msg,
                hostname,
                pid,
            );
        }
    }
}

/// Write a single newline-delimited bunyan record to stdout. `serde_json`
/// handles all escaping, so an embedded quote or newline in `msg` can't
/// break the one-object-per-line protocol looker parses. Every required
/// bunyan field is present (`v`, `name`, `hostname`, `pid`, `level`,
/// `time`, `msg`); the original Vivado severity rides along in a
/// non-standard `severity` field so a viewer can still distinguish
/// CRITICAL WARNING from ERROR (both collapse to level 50).
fn emit_bunyan_line(
    level: u8,
    component: &str,
    severity: Option<&str>,
    msg: &str,
    hostname: &str,
    pid: u32,
) {
    use std::io::Write;
    let time =
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let record =
        bunyan_record(level, component, severity, msg, hostname, pid, &time);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{record}");
    let _ = out.flush();
}

/// Build the bunyan record value. Pure (timestamp injected) so the field
/// set and JSON escaping are unit-testable without touching stdout or the
/// clock. `emit_bunyan_line` wraps it with `chrono::Utc::now()`.
fn bunyan_record(
    level: u8,
    component: &str,
    severity: Option<&str>,
    msg: &str,
    hostname: &str,
    pid: u32,
    time: &str,
) -> serde_json::Value {
    let mut record = serde_json::json!({
        "v": 0,
        "name": "vw",
        "hostname": hostname,
        "pid": pid,
        "level": level,
        "component": component,
        "time": time,
        "msg": msg,
    });
    if let Some(sev) = severity {
        record["severity"] = serde_json::Value::from(sev);
    }
    record
}

/// Map a Vivado [`vw_vivado::Severity`] to a bunyan numeric level:
/// INFO → 30, WARNING → 40, CRITICAL WARNING and ERROR → 50, and
/// non-diagnostic NONE → 20 (debug).
fn bunyan_level_for(severity: vw_vivado::Severity) -> u8 {
    match severity {
        vw_vivado::Severity::None => 20,
        vw_vivado::Severity::Info => 30,
        vw_vivado::Severity::Warning => 40,
        vw_vivado::Severity::CriticalWarning | vw_vivado::Severity::Error => 50,
    }
}

/// The original Vivado severity as a stable lowercase label, preserved in
/// the bunyan `severity` field since CRITICAL WARNING and ERROR share
/// level 50.
fn bunyan_severity_label(severity: vw_vivado::Severity) -> &'static str {
    match severity {
        vw_vivado::Severity::None => "none",
        vw_vivado::Severity::Info => "info",
        vw_vivado::Severity::Warning => "warning",
        vw_vivado::Severity::CriticalWarning => "critical-warning",
        vw_vivado::Severity::Error => "error",
    }
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

/// Thin loader wrapper: resolve the entry's `src` imports into one
/// flattened program, then hand it to [`run_loaded_program`].
async fn run_htcl(
    file: &camino::Utf8Path,
    check_only: bool,
    part: Option<&str>,
    variant: Option<&str>,
    log_level: vw_vivado::LogLevel,
    info_with_stack: bool,
    bunyan: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let program = load_htcl_program(file).await?;
    run_loaded_program(
        file,
        &program,
        check_only,
        part,
        variant,
        log_level,
        info_with_stack,
        bunyan,
    )
    .await
}

/// Execute an already-loaded htcl program end to end: parse/validate
/// gates, an optional check-only short-circuit, then spawn Vivado and
/// run every command — RPC handler, prelude shipping, per-command
/// lowering + eval, and severity-driven exit code.
///
/// Split out from [`run_htcl`] so `vw check`'s IP pre-pass can run an
/// in-memory `src @vw` + `vw::configure_ip` program through the exact
/// same Vivado machinery, without writing a file or shelling out to
/// `vw run`.
///
/// `file` is the entry path — used for workspace discovery, the raw
/// log location, and proc-trace labels; for the IP pre-pass it's a
/// synthetic path inside the workspace whose on-disk source is never
/// read (the program is already loaded/flattened).
#[allow(clippy::too_many_arguments)]
async fn run_loaded_program(
    file: &camino::Utf8Path,
    program: &vw_htcl::LoadedProgram,
    check_only: bool,
    part: Option<&str>,
    variant: Option<&str>,
    log_level: vw_vivado::LogLevel,
    info_with_stack: bool,
    bunyan: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Under the hood the block renderer decides what to show; the
    // backend's `verbose` toggle still gates the unclassified PTY
    // firehose (banner, source echo, idle chatter). At log_level =
    // Debug the user wants everything, so verbose stays on; at
    // higher levels it stays off, and the classifier's NONE-block
    // path is the only source of non-diagnostic content — which the
    // renderer then dims or collapses.
    let verbose = matches!(log_level, vw_vivado::LogLevel::Debug);
    // Borrow `source` from the (caller-owned) program instead of
    // moving; the stack-frame rewriting needs the LoadedProgram for
    // body-span resolution.
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
    // `[[target-parts]]` or `[[workspace.variants]]`, ship a
    // `create_project -in_memory` down the wire before user code
    // runs. `--variant <name>` picks a variant (variant-mode
    // workspaces); `--part <id>` picks a target-parts entry
    // (part-mode workspaces). Otherwise the workspace default
    // wins. Eliminates the "no open project" failure mode for
    // `ip::check` / `get_ipdefs` / etc., and gives the whole
    // session a stable part context for downstream
    // implementation/timing steps.
    let ws_utf8 = rpc_workspace_root
        .as_deref()
        .and_then(camino::Utf8Path::from_path)
        .map(|p| p.to_path_buf());
    let (auto_project, active_variant) = match ws_utf8.as_deref() {
        Some(ws) => resolve_workspace_selection(ws, part, variant)?,
        None => (None, None),
    };
    // Seed the RPC handler's preload map with everything the
    // entry-file load pulled in. `vw run` is single-shot — no
    // batches commit after this — so the initial population IS
    // the final state. `compile_htcl_module` (called from
    // `vw::configure_ip` if the user's htcl invokes it) then
    // skips re-shipping files whose procs are already installed.
    let preload: vw_vivado::SharedPreload = {
        let mut m = std::collections::HashMap::new();
        for f in &program.files {
            if let Some(t) = f.mtime {
                m.insert(f.path.clone(), t);
            }
        }
        std::sync::Arc::new(std::sync::RwLock::new(m))
    };
    // Shared CRITICAL WARNING counter. Bumped by the stream sink
    // below on each `Severity::CriticalWarning` chunk; read via
    // the `critical_warning_count` RPC by `vw::synth` / `vw::place`
    // to gate checkpoint writes on a CW-clean phase. Cloned into
    // the handler and the sink so both sides see the same atomic.
    let cw_count: vw_vivado::SharedCriticalWarningCount =
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rpc_handler = vw_vivado::make_handler_full(
        rpc_workspace_root.clone(),
        active_variant,
        preload,
        cw_count.clone(),
    );
    // Raw byte-log: `<workspace>/target/logs/vivado-<ts>.log`.
    // Failure to create the directory demotes to no-log rather than
    // aborting the run — the log is a diagnostic aid, and a
    // misconfigured target/ dir shouldn't block a synth flow.
    let raw_log = rpc_workspace_root.as_deref().and_then(|ws| {
        match vw_vivado::raw_log_path_for_workspace(ws) {
            Ok(p) => {
                eprintln!(
                    "{} {}",
                    "raw vivado log:".bright_black(),
                    p.display().to_string().bright_black(),
                );
                Some(p)
            }
            Err(e) => {
                eprintln!("{} raw log unavailable: {e}", "warning:".yellow());
                None
            }
        }
    });
    let mut backend =
        vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
            verbose,
            info_with_stack,
            rpc_handler: Some(rpc_handler),
            auto_project,
            raw_log,
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
        program,
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
    // Segmenter state — shared between the sink (which pushes into
    // it per chunk) and the outer scope (which flushes any trailing
    // NONE after the last eval returns so the tail doesn't get
    // silently dropped when Vivado's last write is non-diagnostic).
    let block_acc = std::sync::Arc::new(std::sync::Mutex::new(
        vw_vivado::BlockAccumulator::new(),
    ));
    // Track the highest severity classified during the session. We
    // check `kind` — not the rendered block — so `--log-level=error`
    // can't silence a CRITICAL WARNING into a false-pass exit code.
    // Encoded as u8 (0..=4) matching the Severity ladder so a plain
    // atomic fetch_max gets us thread-safe "worst so far" semantics
    // without a mutex.
    let worst_severity = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
        severity_as_u8(vw_vivado::Severity::None),
    ));
    // Static bunyan record fields (`hostname`, `pid`), resolved once and
    // shared with the stdout sink and the trailing-flush below. Only
    // consulted under `--bunyan`; cheap enough to compute either way.
    let bunyan_host = gethostname::gethostname().to_string_lossy().into_owned();
    let bunyan_pid = std::process::id();
    {
        let procs = std::sync::Arc::clone(&proc_table);
        let input_file = std::sync::Arc::clone(&input_file_for_stack);
        let origin = std::sync::Arc::clone(&current_origin);
        let acc = std::sync::Arc::clone(&block_acc);
        let worst = std::sync::Arc::clone(&worst_severity);
        let cw = std::sync::Arc::clone(&cw_count);
        let host = bunyan_host.clone();
        backend.set_stdout_sink(move |kind, chunk: &str| {
            let cur_origin = origin.lock().ok().and_then(|g| g.clone());
            worst.fetch_max(
                severity_as_u8(vw_vivado::severity_of(kind)),
                std::sync::atomic::Ordering::Relaxed,
            );
            // Bump the CW counter exposed via the
            // `critical_warning_count` RPC. Only count exact
            // CriticalWarning (not Error) — htcl checkpoint gates
            // want CWs specifically; errors already abort the
            // eval before the checkpoint-write branch is reached.
            if matches!(kind, vw_vivado::StreamKind::CriticalWarning) {
                cw.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let blocks = acc
                .lock()
                .map(|mut a| a.push(kind, chunk))
                .unwrap_or_default();
            for block in blocks {
                if bunyan {
                    emit_bunyan_block(
                        &block,
                        log_level,
                        &procs,
                        cur_origin.as_ref(),
                        Some(input_file.as_path()),
                        &host,
                        bunyan_pid,
                    );
                } else {
                    render_block(
                        &block,
                        log_level,
                        &procs,
                        cur_origin.as_ref(),
                        Some(input_file.as_path()),
                    );
                }
            }
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
                    if bunyan {
                        // Eval return values are results, not
                        // diagnostics — surface them at bunyan info so
                        // stdout stays a pure JSON stream for looker.
                        emit_bunyan_line(
                            30,
                            "result",
                            None,
                            &out.value,
                            &bunyan_host,
                            bunyan_pid,
                        );
                    } else if matches!(
                        log_level,
                        vw_vivado::LogLevel::Debug | vw_vivado::LogLevel::Info
                    ) {
                        // Echo a command's return value only at Info/
                        // Debug. At Warning+ the caller asked for terse
                        // output — e.g. `vw check`'s IP pre-pass, where
                        // `vw::configure_ip`'s `null` return is pure
                        // noise.
                        println!("{}", out.value);
                    }
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
    // Flush any trailing NONE block the last chunk left in the
    // accumulator. Vivado's final write is often non-diagnostic
    // (banner, resource summary, "synth_design completed" table);
    // without this the very end of the session would sit unemitted
    // when the caller's eval returns and the sink stops being
    // called. render_block honors the log-level here too, so
    // trailing noise still gets dimmed at Info+ and shown raw at
    // Debug.
    let trailing = block_acc.lock().map(|mut a| a.flush()).unwrap_or_default();
    let cur_origin = current_origin.lock().ok().and_then(|g| g.clone());
    for block in trailing {
        if bunyan {
            emit_bunyan_block(
                &block,
                log_level,
                &proc_table,
                cur_origin.as_ref(),
                Some(input_file_for_stack.as_path()),
                &bunyan_host,
                bunyan_pid,
            );
        } else {
            render_block(
                &block,
                log_level,
                &proc_table,
                cur_origin.as_ref(),
                Some(input_file_for_stack.as_path()),
            );
        }
    }
    // Severity-driven exit code. If ANY chunk classified as
    // CRITICAL WARNING or ERROR during the session, propagate a
    // non-zero exit so CI wrappers / scripts can gate on it. We
    // read the classification counter — NOT the render decision —
    // so a user passing `--log-level=error` can't hide a
    // CRITICAL WARNING into a false-pass. The diagnostic itself
    // has already streamed through the log-level filter (or was
    // suppressed at the caller's request); this is just about the
    // process return code.
    let worst = worst_severity.load(std::sync::atomic::Ordering::Relaxed);
    if worst >= severity_as_u8(vw_vivado::Severity::CriticalWarning) {
        let label = if worst >= severity_as_u8(vw_vivado::Severity::Error) {
            "error"
        } else {
            "critical warning"
        };
        return Err(format!("session emitted at least one {label}").into());
    }
    Ok(())
}

/// True when a VHDL diagnostic set contains an error a Vivado IP pass
/// could fix: an unresolved unit or missing library within the
/// tool-generated `ip` / `xil_defaultlib` libraries. Keys off the
/// library name in the analyzer's message — which only appears on a
/// resolution failure (`No such library 'ip'`, `No primary unit
/// 'primary_clock' within library 'xil_defaultlib'`) — rather than the
/// emptiness of the rendered library: the BD wrappers populate
/// `xil_defaultlib` even while a standalone XCI IP's stub is still
/// absent, so emptiness is not a reliable signal.
fn diagnostics_need_ip_generation(diags: &[vw_lib::VhdlDiagnostic]) -> bool {
    diags.iter().any(|d| {
        matches!(d.severity, vw_lib::VhdlSeverity::Error)
            && (d.message.contains("library 'ip'")
                || d.message.contains("library 'xil_defaultlib'"))
    })
}

/// Run `src @vw` + `vw::configure_ip` in-process (spawning Vivado) so a
/// subsequent VHDL check can resolve the design's generated `ip` /
/// `xil_defaultlib` libraries.
///
/// Called by `vw check` only when
/// [`vw_lib::workspace_needs_ip_generation`] reports the design
/// references those libraries and their wrappers aren't already in
/// `target/` — so a workspace whose IP is already generated never
/// pays for a (multi-minute) Vivado run. The program is built and run
/// entirely in-library: no temp file, no `vw run` sub-process.
async fn ensure_ip_generated(
    ws: &camino::Utf8Path,
    part: Option<&str>,
    variant: Option<&str>,
    log_level: vw_vivado::LogLevel,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build the resolver exactly as `load_htcl_program` does so `@vw`
    // — and `@vw`'s own transitive `src @vivado-cmd/…` imports —
    // resolve. `configure_ip` is a normal (non-test) run, so
    // test-dependencies stay out.
    let mut resolver = vw_htcl::Resolver::new();
    if let Ok(paths) = vw_lib::transitive_dep_cache_paths_with_test(ws, false) {
        for (name, path) in paths {
            resolver = resolver.with_dep(name, path);
        }
    }
    // Cargo-parity self-reference so a workspace can `src @<self>/…`.
    if let Ok(cfg) = vw_lib::load_workspace_config(ws) {
        resolver = resolver.with_dep_if_absent(
            cfg.workspace.name,
            ws.as_std_path().to_path_buf(),
        );
    }
    // Synthetic entry inside the workspace — never written to disk.
    // Its parent (the workspace) is what `run_loaded_program` uses to
    // discover the RPC workspace root; `@vw` resolves through the dep
    // map above, not this path.
    let entry = ws.join(".vw-configure-ip.htcl");
    // `configure_ip -generate_targets false` makes block-design IPs
    // check-ready (their `<ip>_wrapper` entities land in library `ip`
    // via `make_wrapper`) WITHOUT the minutes-long
    // `generate_target -name all` per BD — that generation only feeds
    // synthesis, and the check resolves the wrapper's inner BD as an
    // unbound `component`. `generate_ip_stubs` then writes the
    // instantiation-template stubs for the standalone XCI IPs so
    // `entity xil_defaultlib.<ip>` (e.g. a bare `clk_wizard` like
    // `primary_clock`) also resolves. Neither step synthesizes.
    let program = vw_htcl::load_program_source(
        "src @vw\n\
         vw::configure_ip -generate_targets false\n\
         vw::generate_ip_stubs\n",
        entry.as_std_path(),
        &resolver,
    )?;
    let run_result = run_loaded_program(
        &entry, &program, /*check_only=*/ false, part, variant, log_level,
        /*info_with_stack=*/ false, /*bunyan=*/ false,
    )
    .await;
    // Rewrite the just-generated `.vho` instantiation templates into
    // black-box VHDL stubs. Done here (not in htcl) — a mechanical
    // component→entity splice is far simpler in Rust than in Tcl. Run
    // it even if the pre-pass reported a non-fatal error, since the
    // templates may already be on disk.
    if let Ok(n) = vw_lib::write_ip_stubs_from_templates(ws) {
        if n > 0 {
            println!(
                "{} wrote {n} IP stub(s) for the VHDL check",
                "note:".bright_yellow(),
            );
        }
    }
    run_result
}

#[cfg(test)]
mod bunyan_tests {
    use super::*;

    /// The severity→level mapping is the crux of `--bunyan`: INFO→30,
    /// WARNING→40, CRITICAL WARNING and ERROR both →50 (error), and
    /// non-diagnostic noise →20 (debug).
    #[test]
    fn severity_maps_to_requested_bunyan_levels() {
        assert_eq!(bunyan_level_for(vw_vivado::Severity::Info), 30);
        assert_eq!(bunyan_level_for(vw_vivado::Severity::Warning), 40);
        assert_eq!(bunyan_level_for(vw_vivado::Severity::CriticalWarning), 50);
        assert_eq!(bunyan_level_for(vw_vivado::Severity::Error), 50);
        assert_eq!(bunyan_level_for(vw_vivado::Severity::None), 20);
    }

    /// Every required bunyan field (per looker's `BunyanEntry`) must be
    /// present with the right type, the record must be a single line, and
    /// quotes/newlines in the message must survive JSON escaping.
    #[test]
    fn record_is_single_line_json_with_all_required_fields() {
        let rec = bunyan_record(
            50,
            "vivado",
            Some("critical-warning"),
            "CRITICAL WARNING: [Vivado 12-4739] a \"quoted\" bit\n  at x.htcl:3",
            "host1",
            1234,
            "2026-07-19T00:00:00.000Z",
        );
        let line = rec.to_string();
        assert!(!line.contains('\n'), "a record must serialize to one line");

        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        // Required fields looker rejects a record for missing.
        assert_eq!(v["v"], 0);
        assert_eq!(v["name"], "vw");
        assert_eq!(v["hostname"], "host1");
        assert_eq!(v["pid"], 1234);
        assert_eq!(v["level"], 50);
        assert_eq!(v["time"], "2026-07-19T00:00:00.000Z");
        assert!(v["msg"].is_string());
        // Original severity preserved so CW is distinguishable from ERROR.
        assert_eq!(v["severity"], "critical-warning");
        assert_eq!(v["component"], "vivado");
        // Escaping round-trips the embedded quote and newline.
        let msg = v["msg"].as_str().unwrap();
        assert!(msg.contains('"') && msg.contains('\n'));
    }

    /// An info record with no `severity` (the eval-result echo path) still
    /// carries every required field and omits the optional `severity`.
    #[test]
    fn result_record_omits_optional_severity() {
        let rec = bunyan_record(
            30,
            "result",
            None,
            "ok",
            "h",
            7,
            "2026-07-19T00:00:00.000Z",
        );
        assert!(rec.get("severity").is_none());
        assert_eq!(rec["level"], 30);
        assert_eq!(rec["component"], "result");
        assert_eq!(rec["v"], 0);
    }
}
