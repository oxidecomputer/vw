// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw test` — htcl-level test runner.
//!
//! Discovers `@test`-annotated procs under `<workspace>/test/**/
//! *.htcl`, drives them against a Vivado session, and reports
//! results in a nextest-inspired format.
//!
//! Design shape:
//! - Each test file's non-`@test` top-level statements are treated
//!   as setup (proc decls, `src` imports). They're shipped to
//!   Vivado once per file per session.
//! - Tests without a specific attribute value run in a SHARED
//!   Vivado session; `@test(dedicated-eda)` marks tests that need
//!   their own Vivado process (spawned per test, capped by
//!   `--test-threads`).
//! - Assertion failures throw Tcl errors, which the runner catches
//!   as `BackendError::Tcl` and marks the enclosing test FAILED.

use std::path::PathBuf;
use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};
use colored::*;

use vw_eda::EdaBackend;

use crate::load_htcl_program_for_test;

/// Entry point wired from `main.rs::Commands::Test`.
#[allow(clippy::too_many_arguments)]
pub async fn run_htcl_tests(
    cwd: &Utf8Path,
    filter: Option<String>,
    list: bool,
    test_threads: usize,
    part: Option<&str>,
    variant: Option<&str>,
    verbose: bool,
    info_with_stack: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws = vw_lib::find_workspace_dir(cwd.as_std_path())
        .ok_or("not in a vw workspace (no vw.toml in the parent chain)")?;
    let test_files = vw_lib::list_htcl_tests(&ws)?;
    if test_files.is_empty() {
        eprintln!("no tests found in {}", ws.join("test").as_str().dimmed());
        return Ok(());
    }

    // Discover phase — load each test file's program and enumerate
    // its `@test` procs.
    let mut all_tests: Vec<TestCase> = Vec::new();
    for path in &test_files {
        let path_utf8 = Utf8PathBuf::from_path_buf(path.clone())
            .map_err(|p| format!("non-UTF8 path: {p:?}"))?;
        let discovered =
            discover_tests_in_file(&path_utf8, &ws, filter.as_deref()).await?;
        all_tests.extend(discovered);
    }

    if list {
        for t in &all_tests {
            let tag = if t.dedicated { " [dedicated-eda]" } else { "" };
            println!("{}::{}{}", t.display_path, t.name.cyan(), tag.dimmed());
        }
        return Ok(());
    }

    if all_tests.is_empty() {
        eprintln!(
            "no tests matched the filter {}",
            filter.as_deref().unwrap_or("").dimmed()
        );
        return Ok(());
    }

    let overall_start = Instant::now();
    let mut summary = RunSummary::default();

    // Shared bucket — one Vivado for all `@test` (without
    // `dedicated-eda`) tests.
    let (shared, dedicated): (Vec<_>, Vec<_>) =
        all_tests.into_iter().partition(|t| !t.dedicated);

    println!(
        "\nrunning {} tests",
        (shared.len() + dedicated.len()).to_string().bold()
    );

    if !shared.is_empty() {
        run_shared_bucket(
            &ws,
            shared,
            &mut summary,
            part,
            variant,
            verbose,
            info_with_stack,
        )
        .await?;
    }
    if !dedicated.is_empty() {
        run_dedicated_bucket(
            &ws,
            dedicated,
            test_threads,
            &mut summary,
            part,
            variant,
            verbose,
            info_with_stack,
        )
        .await?;
    }

    print_summary(&summary, overall_start.elapsed());
    if summary.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// One `@test`-annotated proc discovered inside a test file. The
/// `program` and `setup_tcl_lines` fields carry everything the
/// runner needs to ship setup to Vivado before invoking the test.
struct TestCase {
    /// Test file path, workspace-relative.
    display_path: String,
    /// Absolute path — used to key "which file's setup has been
    /// shipped in this Vivado session already."
    file_path: PathBuf,
    /// Proc name as it appears at Tcl call level.
    name: String,
    /// True if `@test(dedicated-eda)` — this test wants its own
    /// Vivado process.
    dedicated: bool,
    /// `@target(part=<id>)` override — swaps the auto-project's
    /// `-part` argument for this specific test. Meaningful only
    /// for `dedicated-eda` tests (shared-bucket tests all reuse
    /// the workspace-default project).
    target_part_override: Option<String>,
    /// `@test(dedicated-eda variant=<name>)` override — selects a
    /// specific `[[workspace.variants]]` entry for this test.
    /// The variant's `part` becomes the auto-project part; its
    /// name flows through to `vw::vhdl_design_sources` so
    /// variant-exclusive files filter correctly. Same
    /// dedicated-eda restriction as `target_part_override`.
    /// Meaningful only in variant-mode workspaces.
    variant_override: Option<String>,
    /// Concrete Tcl lines to ship as setup before invoking the
    /// test proc. Includes proc declarations, `src`-imported code,
    /// and any top-level setup statements.
    setup_tcl: Vec<String>,
}

async fn discover_tests_in_file(
    file: &Utf8Path,
    ws: &Utf8Path,
    filter: Option<&str>,
) -> Result<Vec<TestCase>, Box<dyn std::error::Error>> {
    let program = load_htcl_program_for_test(file).await?;
    let source = program.source.clone();
    let parsed = vw_htcl::parse(&source);
    // Only reject on parse errors — validator warnings shouldn't
    // block tests, matching cargo test's behavior.
    if !parsed.errors.is_empty() {
        let mut msg = format!("parse errors in {}:\n", file);
        for e in &parsed.errors {
            msg.push_str(&format!("  {}: {}\n", e.span.start, e.message));
        }
        return Err(msg.into());
    }

    let putr_map = vw_htcl::putr::rewrite(&source, &parsed.document);
    let signature_table = vw_htcl::signature_table(&parsed.document);
    let line_index = vw_htcl::LineIndex::new(&source);

    // Pre-ship: primitive prelude + enum preludes + overload
    // dispatchers. Same shape `run_htcl` uses (main.rs:1631-1667)
    // and required for wrapped user code to install its procs
    // correctly under the shim's `install_proc_body_wrap` machinery.
    let mut _ignored: Vec<vw_htcl::ValidatorDiagnostic> = Vec::new();
    let enum_decl_table =
        vw_htcl::build_enum_decl_table(&parsed.document, &mut _ignored);
    let type_decl_table =
        vw_htcl::build_type_decl_table(&parsed.document, &mut _ignored);
    let type_decl_names: std::collections::HashSet<String> =
        type_decl_table.keys().cloned().collect();
    let (_full_sigs, overload_table) =
        vw_htcl::build_signature_table_with_overloads(
            &parsed.document,
            &type_decl_names,
            &mut _ignored,
        );
    let mut setup_tcl: Vec<String> = Vec::new();
    for p in vw_htcl::emit_primitive_prelude() {
        setup_tcl.push(p);
    }
    for ed in enum_decl_table.values() {
        let prelude = vw_htcl::emit_enum_prelude(ed);
        if !prelude.trim().is_empty() {
            setup_tcl.push(prelude);
        }
    }
    for info in overload_table.values() {
        let dispatcher = vw_htcl::emit_dispatcher(info);
        if !dispatcher.trim().is_empty() {
            setup_tcl.push(dispatcher);
        }
    }

    // Partition statements: `@test`-proc decls are collected as
    // tests; everything else (including proc decls WITHOUT
    // `@test`, `src` imports, and top-level setup) becomes setup.
    let mut test_procs: Vec<(String, bool, Option<String>, Option<String>)> =
        Vec::new();
    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        if let vw_htcl::CommandKind::Proc(proc) = &cmd.kind {
            if let Some(test_attr) = proc.attribute("test") {
                let Some(name) = proc.name.clone() else {
                    continue;
                };
                let dedicated = test_attr.values.iter().any(|v| {
                    matches!(
                        v,
                        vw_htcl::AttributeValue::Ident { value, .. }
                            if value == "dedicated-eda"
                    )
                });
                // Per-test target override, e.g.
                // `@test(dedicated-eda target="xcvp1202-vsva2785-3HP-e-S")`.
                // Swaps the auto-project's `-part` for this test's
                // Vivado session. Only meaningful for dedicated-eda
                // tests (the validator warns otherwise); shared tests
                // silently fall back to the workspace default.
                let target_part_override =
                    extract_target_from_test_attr(test_attr);
                let variant_override =
                    extract_variant_from_test_attr(test_attr);
                test_procs.push((
                    name,
                    dedicated,
                    target_part_override,
                    variant_override,
                ));
                // Test proc decls MUST also be shipped as setup —
                // Vivado has to know about the proc before we call
                // it. Fall through to the setup-emit below.
            }
        }
        let lowered = vw_htcl::lower_command_with_putr_and_index(
            cmd,
            &source,
            &signature_table,
            &putr_map,
            &line_index,
        );
        // `rewrite_externs` — same as `vw run`.
        let stripped = vw_htcl::rewrite_externs(&lowered).text;
        if !stripped.trim().is_empty() {
            setup_tcl.push(stripped);
        }
    }

    let display_path = file
        .strip_prefix(ws)
        .map(|p| p.to_string())
        .unwrap_or_else(|_| file.to_string());

    let mut out = Vec::new();
    for (name, dedicated, target_part_override, variant_override) in test_procs
    {
        if let Some(filt) = filter {
            if !name.contains(filt) {
                continue;
            }
        }
        out.push(TestCase {
            display_path: display_path.clone(),
            file_path: file.as_std_path().to_path_buf(),
            name,
            dedicated,
            target_part_override,
            variant_override,
            setup_tcl: setup_tcl.clone(),
        });
    }
    Ok(out)
}

/// Pull the `variant=<name>` keyed value out of a `@test(...)`
/// attribute. Same shape as [`extract_target_from_test_attr`]
/// but for the variant-mode workspace path.
fn extract_variant_from_test_attr(attr: &vw_htcl::Attribute) -> Option<String> {
    for v in &attr.values {
        let vw_htcl::AttributeValue::Keyed { key, value, .. } = v else {
            continue;
        };
        if key != "variant" {
            continue;
        }
        return match value.as_ref() {
            vw_htcl::AttributeValue::String { value, .. } => {
                Some(value.clone())
            }
            vw_htcl::AttributeValue::Ident { value, .. } => Some(value.clone()),
            _ => None,
        };
    }
    None
}

/// Pull the `target=<part>` keyed value out of a `@test(...)`
/// attribute. Returns `None` if the attribute has no `target`
/// key or the value isn't a string/ident.
fn extract_target_from_test_attr(attr: &vw_htcl::Attribute) -> Option<String> {
    for v in &attr.values {
        let vw_htcl::AttributeValue::Keyed { key, value, .. } = v else {
            continue;
        };
        if key != "target" {
            continue;
        }
        return match value.as_ref() {
            vw_htcl::AttributeValue::String { value, .. } => {
                Some(value.clone())
            }
            vw_htcl::AttributeValue::Ident { value, .. } => Some(value.clone()),
            _ => None,
        };
    }
    None
}

/// Run all shared-bucket tests in ONE Vivado session. First-file
/// setup lands once, then each proc-invocation is a separate eval.
async fn run_shared_bucket(
    ws: &Utf8Path,
    tests: Vec<TestCase>,
    summary: &mut RunSummary,
    part: Option<&str>,
    variant: Option<&str>,
    verbose: bool,
    info_with_stack: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Shared bucket uses one Vivado process for every test in it,
    // so per-test `@target(part=…)` / `@target(variant=…)` cannot
    // apply — those overrides only make sense in dedicated-eda
    // tests where each run gets a fresh session. The validator
    // flags this at check time; here we silently fall back to the
    // workspace default (or the CLI's `--part` / `--variant`
    // selector if the user passed one).
    let auto_project = workspace_auto_project(ws, part, variant)?;
    // Show the first test's name up front with a "starting vivado"
    // status so the run doesn't look hung during the ~10-30s
    // Vivado boot. `run_one_test` overwrites this line as soon as
    // it fires its own "running" line.
    if let Some(first) = tests.first() {
        let label = format!("{}::{}", first.display_path, first.name);
        print_status_line("starting vivado", &label);
    }
    let active_variant = resolve_active_variant_name(ws, variant)?;
    let mut backend = spawn_backend(
        ws,
        verbose,
        info_with_stack,
        auto_project,
        active_variant,
    )
    .await?;
    // Track which file's setup we've already shipped.
    let mut shipped: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    for test in tests {
        run_one_test(
            &mut backend,
            &test,
            summary,
            &mut shipped,
            /*display_shipped=*/ true,
        )
        .await;
    }
    // Detach shutdown — see `run_dedicated_bucket` for the
    // rationale; here it saves the summary block from a 10s
    // Vivado-exit wait tacked onto the tail of `vw test`.
    tokio::spawn(async move {
        let _ = backend.shutdown().await;
    });
    Ok(())
}

/// Run `@test(dedicated-eda)` tests, each in its own Vivado
/// process. `test_threads` caps parallelism.
#[allow(clippy::too_many_arguments)]
async fn run_dedicated_bucket(
    ws: &Utf8Path,
    tests: Vec<TestCase>,
    test_threads: usize,
    summary: &mut RunSummary,
    part: Option<&str>,
    variant: Option<&str>,
    verbose: bool,
    info_with_stack: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let threads = test_threads.max(1);
    // Sequential for MVP — a proper semaphore-based parallel run
    // would use tokio::task::JoinSet. Vivado processes each hold
    // ~1-2 GB of RAM so caution around parallelism is warranted;
    // parallel exec is a follow-up.
    let _ = threads;
    let ws_default = workspace_auto_project(ws, part, variant)?;
    // Precompute variant → part for cheap per-test lookup below.
    // `None` when the workspace has no variants block; a missing
    // variant name gets a targeted error rather than a silent
    // fallback so a typo in `@test(dedicated-eda variant=vpk20)`
    // fails loud.
    let ws_cfg = vw_lib::load_workspace_config(ws).ok();
    for test in tests {
        // Resolution order per-test:
        //   1. `variant=<name>`   → variant.part, override name  from ws_default
        //   2. `target=<part>`    → the literal part
        //   3. no override        → workspace default (whatever `part` selected)
        let auto_project = if let Some(vname) = &test.variant_override {
            let Some(cfg) = &ws_cfg else {
                return Err(format!(
                    "test `{}` requested variant `{}` but the workspace \
                     has no `vw.toml` — declare a `[[workspace.variants]]` \
                     block first",
                    test.name, vname,
                )
                .into());
            };
            let variant = cfg
                .workspace
                .select_variant(Some(vname))
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "test `{}` requested variant `{}` but the workspace \
                         has no `[[workspace.variants]]` block",
                        test.name, vname,
                    )
                })?;
            Some(vw_vivado::AutoProject {
                name: ws_default
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "vw_test".to_string()),
                part: variant.part.clone(),
            })
        } else if let Some(p) = &test.target_part_override {
            Some(vw_vivado::AutoProject {
                name: ws_default
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "vw_test".to_string()),
                part: p.clone(),
            })
        } else {
            ws_default.clone()
        };
        // Show the test up front so the runner announces what's
        // pending during Vivado boot (~10-30s per dedicated-eda
        // test). Overwritten by run_one_test's "running" once
        // spawn returns.
        let label = format!("{}::{}", test.display_path, test.name);
        print_status_line("starting vivado", &label);
        // Per-test variant precedence: `@test(dedicated-eda variant=…)`
        // wins over the CLI `--variant`, which in turn wins over the
        // workspace default. Only meaningful when the workspace is
        // variant-mode; part-mode workspaces get `None` and the
        // session-scoped filter stays inert.
        let active_variant = if test.variant_override.is_some() {
            test.variant_override.clone()
        } else {
            resolve_active_variant_name(ws, variant)?
        };
        let mut backend = spawn_backend(
            ws,
            verbose,
            info_with_stack,
            auto_project,
            active_variant,
        )
        .await?;
        let mut shipped: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        run_one_test(
            &mut backend,
            &test,
            summary,
            &mut shipped,
            /*display_shipped=*/ false,
        )
        .await;
        // Detach shutdown. Vivado's tear-down is a 10s-bounded
        // graceful-exit dance (see VivadoBackend::shutdown), and
        // for an in-memory test session nothing about that wait is
        // load-bearing — the child process exit / kill happens
        // either way. Fire-and-forget so the runner reaches the
        // summary block immediately after the last test, and so
        // one test's shutdown overlaps the next test's Vivado
        // boot instead of serializing. If the process exits before
        // the shutdown task finishes, VivadoBackend's Drop still
        // SIGKILLs the child — no orphan.
        tokio::spawn(async move {
            let _ = backend.shutdown().await;
        });
    }
    Ok(())
}

async fn run_one_test(
    backend: &mut vw_vivado::VivadoBackend,
    test: &TestCase,
    summary: &mut RunSummary,
    shipped: &mut std::collections::HashSet<PathBuf>,
    _display_shipped: bool,
) {
    let started = Instant::now();
    let label = format!("{}::{}", test.display_path, test.name);
    // Live "RUN" line — on a TTY we print with `\r` (no newline)
    // and overwrite in place when the test finishes; on non-TTY
    // we suppress it entirely so log files stay linear. Same
    // convention nextest uses. Vivado boot takes ~10-30s per
    // shared bucket, so without this the runner looks hung until
    // the first PASS/FAIL prints.
    print_running_line(&label);
    // Ship setup once per file per session. Any error during setup
    // is a hard-fail — subsequent tests in the same file would
    // observe a broken state, so we bail on the whole file.
    if shipped.insert(test.file_path.clone()) {
        for line in &test.setup_tcl {
            if let Err(e) = backend.eval(line).await {
                let elapsed = format_secs(started.elapsed().as_secs_f64());
                clear_running_line();
                println!(
                    " {} [{}] {} — setup error",
                    "FAIL".red().bold(),
                    elapsed.red(),
                    label,
                );
                summary.failed += 1;
                summary.failures.push(TestFailure {
                    display: label,
                    err: e,
                });
                return;
            }
        }
    }
    // Actually invoke the test proc.
    match backend.eval(&test.name).await {
        Ok(_) => {
            let elapsed = format_secs(started.elapsed().as_secs_f64());
            clear_running_line();
            println!(
                " {} [{}] {}",
                "PASS".green().bold(),
                elapsed.green(),
                label,
            );
            summary.passed += 1;
        }
        Err(e) => {
            let elapsed = format_secs(started.elapsed().as_secs_f64());
            clear_running_line();
            println!(" {} [{}] {}", "FAIL".red().bold(), elapsed.red(), label,);
            summary.failed += 1;
            summary.failures.push(TestFailure {
                display: label,
                err: e,
            });
        }
    }
}

/// Print the "currently running" line for `label`, overwriting
/// itself on the next `clear_running_line`. No-op on non-TTY —
/// the label would just fill the log with cursor-return sequences
/// that get rendered as literal `\r`.
fn print_running_line(label: &str) {
    print_status_line("running", label);
}

/// Same shape as [`print_running_line`] but with a caller-chosen
/// status word. Used to show `starting vivado` up front while the
/// backend boots (~10-30s) so the runner doesn't look hung on
/// dedicated-eda tests before their first eval.
fn print_status_line(status: &str, label: &str) {
    use std::io::{IsTerminal, Write};
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return;
    }
    // Pad status to a consistent width so successive re-renders
    // ("starting vivado" → "running") overwrite cleanly without
    // trailing chars leaking through.
    let _ = write!(
        out,
        "\r {} [ {:16} ] {}",
        "RUN ".cyan().bold(),
        status.cyan(),
        label,
    );
    let _ = out.flush();
}

fn clear_running_line() {
    use std::io::{IsTerminal, Write};
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return;
    }
    // Overwrite the RUN line with spaces then \r back to column 0,
    // so the PASS/FAIL println that follows lands at the start of
    // the line. 120 cols is enough for any reasonable test name +
    // decoration; longer names truncate the RUN line in place,
    // which is fine because it's transient.
    let _ = write!(out, "\r{}\r", " ".repeat(120));
    let _ = out.flush();
}

async fn spawn_backend(
    ws: &Utf8Path,
    verbose: bool,
    info_with_stack: bool,
    auto_project: Option<vw_vivado::AutoProject>,
    active_variant: Option<String>,
) -> Result<vw_vivado::VivadoBackend, Box<dyn std::error::Error>> {
    let rpc_handler = vw_vivado::make_handler_with_variant(
        Some(ws.as_std_path().to_path_buf()),
        active_variant,
    );
    let backend = vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
        verbose,
        info_with_stack,
        rpc_handler: Some(rpc_handler),
        auto_project,
        ..Default::default()
    })
    .await
    .map_err(|e| format!("failed to start Vivado worker: {e}"))?;
    Ok(backend)
}

/// Resolve the active variant name for a workspace given the
/// CLI's `--variant` selector. Returns `None` for workspaces with
/// no `[[workspace.variants]]` block (variant-mode is inactive).
/// Errors mirror `workspace_auto_project`'s selector-mismatch
/// checks — a workspace-mode/selector mismatch is caller error.
fn resolve_active_variant_name(
    ws: &Utf8Path,
    variant: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Ok(cfg) = vw_lib::load_workspace_config(ws) else {
        return Ok(None);
    };
    if cfg.workspace.variants.is_empty() {
        return Ok(None);
    }
    let selected = cfg
        .workspace
        .select_variant(variant)
        .map_err(|e| e.to_string())?;
    Ok(selected.map(|v| v.name.clone()))
}

/// Derive the auto-project from a workspace's `[[target-parts]]`
/// or `[[workspace.variants]]` block — same rule `vw run` uses.
/// - `part` is the CLI's `--part` selector; ignored when the
///   workspace is variant-mode.
/// - `variant` is the CLI's `--variant` selector; ignored when the
///   workspace is part-mode.
///
/// Returns:
/// - `Ok(None)` for library workspaces (neither block declared)
/// - `Ok(Some(_))` when a part / variant resolves cleanly
/// - `Err(_)` when the CLI selector is bogus, the multi-entry list
///   has no default marked, or the user passed a selector that
///   doesn't match the workspace's declared mode.
fn workspace_auto_project(
    ws: &Utf8Path,
    part: Option<&str>,
    variant: Option<&str>,
) -> Result<Option<vw_vivado::AutoProject>, Box<dyn std::error::Error>> {
    let Ok(cfg) = vw_lib::load_workspace_config(ws) else {
        return Ok(None);
    };
    if !cfg.workspace.variants.is_empty() {
        if part.is_some() {
            return Err(format!(
                "workspace `{}` declares `[[workspace.variants]]` — use \
                 `--variant <name>` instead of `--part`",
                cfg.workspace.name,
            )
            .into());
        }
        let selected = cfg
            .workspace
            .select_variant(variant)
            .map_err(|e| e.to_string())?;
        return Ok(selected.map(|v| vw_vivado::AutoProject {
            name: cfg.workspace.name.clone(),
            part: v.part.clone(),
        }));
    }
    if variant.is_some() {
        return Err(format!(
            "workspace `{}` does not declare `[[workspace.variants]]` — \
             remove `--variant` or add a variants block",
            cfg.workspace.name,
        )
        .into());
    }
    let selected = cfg
        .workspace
        .select_target_part(part)
        .map_err(|e| e.to_string())?;
    Ok(selected.map(|p| vw_vivado::AutoProject {
        name: cfg.workspace.name.clone(),
        part: p.to_string(),
    }))
}

#[derive(Default)]
struct RunSummary {
    passed: usize,
    failed: usize,
    failures: Vec<TestFailure>,
}

struct TestFailure {
    display: String,
    /// The raw backend error kept intact so `print_summary` can
    /// render it with the same colored/split treatment `vw run`
    /// and the REPL apply — message in bright red, stdout in
    /// default, stack in dimmed gray.
    err: vw_eda::BackendError,
}

fn print_summary(summary: &RunSummary, elapsed: std::time::Duration) {
    if !summary.failures.is_empty() {
        println!("\n{}\n", "failures:".red().bold());
        for f in &summary.failures {
            print_failure_block(f);
        }
    }
    let outcome = if summary.failed == 0 {
        "ok".green().bold().to_string()
    } else {
        "FAILED".red().bold().to_string()
    };
    println!(
        "\ntest result: {outcome}. {} passed; {} failed; finished in {}",
        summary.passed.to_string().green(),
        summary.failed.to_string().red(),
        format_secs(elapsed.as_secs_f64()),
    );
}

/// Print one failure block. Layout mirrors nextest's:
/// separator + STDOUT capture + STDERR/error + stack. Colors
/// match `vw run` / REPL — bright red for the failing message,
/// dimmed for the stack, default for captured stdout.
fn print_failure_block(f: &TestFailure) {
    let bar = "─".repeat(64);
    println!("{}", bar.red());
    println!(" {} {}", "✗".red().bold(), f.display.bold());
    match &f.err {
        vw_eda::BackendError::Tcl {
            message,
            info,
            stdout,
            ..
        } => {
            if !stdout.trim().is_empty() {
                println!("\n{}", "STDOUT:".bright_black().bold());
                println!("{}", stdout.trim_end());
            }
            println!("\n{}", "ERROR:".red().bold());
            for line in message.lines() {
                println!("  {}", line.red());
            }
            if let Some(info) = info {
                // Tcl's `errorInfo` embeds the error message at its
                // top and appends the "while executing…" frames
                // after it. Rendering the whole thing after the
                // ERROR block duplicates every line of the message
                // (which for `assert_file_eq` is the entire diff).
                // Strip the message prefix off `info` and show
                // only the frame tail; skip the section entirely
                // if nothing but the message was present.
                let frames = strip_message_prefix(info, message);
                if !frames.trim().is_empty() {
                    println!("\n{}", "stack:".bright_black().bold());
                    for line in frames.lines() {
                        println!("  {}", line.bright_black());
                    }
                }
            }
        }
        other => {
            println!("  {}", other.to_string().red());
        }
    }
    println!("{}\n", bar.red());
}

/// Trim Tcl's echo of the error message from the start of
/// `errorInfo`. Tcl formats `errorInfo` as
/// `<message>\n    while executing "..."\n    ...`, so the frame
/// tail always starts after `message` (whose newlines and
/// whitespace we match verbatim). When the message doesn't appear
/// at the top for some reason, we fall back to returning the raw
/// info unchanged rather than silently swallowing the whole stack.
fn strip_message_prefix<'a>(info: &'a str, message: &str) -> &'a str {
    let trimmed = message.trim_end();
    if let Some(rest) = info.strip_prefix(trimmed) {
        // Skip any leading whitespace / newlines between the
        // message and the first `while executing "..."` frame.
        rest.trim_start_matches(['\n', '\r', ' '])
    } else {
        info
    }
}

fn format_secs(secs: f64) -> String {
    format!("{secs:>6.3}s")
}

#[cfg(test)]
mod tests {
    use super::strip_message_prefix;

    #[test]
    fn strip_removes_message_echo_and_leading_whitespace() {
        let message =
            "assert_file_eq: files differ\n  actual: a\n  expected: b";
        let info = "assert_file_eq: files differ\n  actual: a\n  expected: b\n    while executing\n\"error \\\"foo\\\"\"";
        let out = strip_message_prefix(info, message);
        assert!(out.starts_with("while executing"), "got {out:?}");
    }

    #[test]
    fn strip_falls_back_when_prefix_missing() {
        let info = "totally different\n    while executing\n\"foo\"";
        let out = strip_message_prefix(info, "assert_file_eq: files differ");
        assert_eq!(out, info);
    }
}
