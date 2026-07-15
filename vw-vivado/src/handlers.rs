// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Shared RPC handlers wired into every Vivado spawn (`vw run`,
//! `vw repl`, `vw test`). Each method here answers a `vw::` htcl
//! proc whose answer lives on the tool side rather than in Vivado.
//!
//! Methods:
//! - `workspace_root` — the discovered `vw.toml` parent dir.
//! - `diff_files` — read two files and return a unified diff of
//!   their contents. Used by `test::assert_file_eq` to render a
//!   readable failure message instead of a two-line "files differ"
//!   note.
//! - `vhdl_dependency_sources` — every VHDL file shipped by any
//!   transitive dep (regular `[dependencies]` only), grouped by
//!   target library. Consumed by `design.htcl` to feed
//!   `read_vhdl -library <lib> …`.
//! - `vhdl_dependency_sources_with_test` — same, but the entry
//!   workspace's `[test-dependencies]` are also included.
//!   Consumed by `test/*.htcl` where test-deps are in scope.
//! - `vhdl_design_sources` — every VHDL file under
//!   `<workspace>/hdl/`. Consumed alongside dependency sources
//!   to compile the workspace's own design.
//! - `vhdl_ip_sources` — every generated IP wrapper under
//!   `<workspace>/target/ip/**/*.vhd`. Populated by
//!   `vw::make_wrapper`. Kept separate from design sources
//!   because wrappers have their own regen lifecycle and
//!   typically compile into a distinct library (`ip`).
//! - `design_constraints` — every Vivado constraint file under
//!   `<workspace>/constraints/**/*.{xdc,sdc}`. Fed to `read_xdc`
//!   during synth prep.
//! - `design_synth_constraints` / `design_place_constraints` /
//!   `design_route_constraints` — phase-scoped variants that
//!   walk `constraints/synth/`, `constraints/place/`,
//!   `constraints/route/` respectively. Used to attach USED_IN
//!   flags to `read_xdc` so route-only constraints don't apply
//!   during synthesis (and vice versa).
//! - `synth_needs_update` — content-hash comparison between the
//!   checkpoint's sidecar manifest and the current tracked source
//!   set (design VHDL, IP wrappers, synth XDC, workspace `.htcl`,
//!   vw.toml, vw.lock). Returns `true` when the checkpoint OR
//!   manifest is missing OR the fingerprints disagree. Backs
//!   the `vw::synth` cache path.
//! - `synth_mark_checkpoint` — writes the sidecar manifest for a
//!   freshly-produced checkpoint. Called after
//!   `vivado_cmd::write_checkpoint` so the next invocation can
//!   compare fingerprints and skip resynth on unchanged sources.
//! - `ip_needs_update` / `ip_mark_checkpoint` — same shape as
//!   the synth pair, but the fingerprint covers only
//!   `<ws>/ip/**/*.htcl`. Backs `vw::configure_ip` so the
//!   `ip::configure` proc (typically a batch of expensive
//!   `create_ip` / `make_wrapper` calls) is skipped when the
//!   IP tree hasn't changed since the last checkpoint.
//! - `compile_htcl_module` — parses + lowers an htcl module (any
//!   `src`-shaped path resolved against the workspace root) and
//!   returns the concatenated Tcl. `vw::configure_ip` uses this
//!   to auto-load `<ws>/ip/module.htcl` when `::ip::configure`
//!   isn't already defined, so a `src ip` in `design.htcl` isn't
//!   a hidden requirement.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use serde_json::Value;

use crate::rpc::{FnHandler, RpcHandler};

/// Shared "already loaded in this Vivado session" map. Populated
/// by the CLI / REPL after each successful htcl load with the
/// paths + mtimes of every file that was shipped to Vivado.
/// Consulted by [`compile_htcl_module`] so on-demand
/// module compilation skips re-shipping files whose procs are
/// already installed — which is both a big perf win and, more
/// importantly, KEEPS the RPC path from re-loading dep files that
/// design.htcl never touched. Concretely: `design.htcl`'s
/// `src @vw` pulls @vw + @vivado-cmd; a workspace `ip/cips.htcl`
/// also does `src @cpm5` / `src @cips` / `src @clk-wizard`; those
/// three are NOT in the initial session because design.htcl
/// never reached them, and the auto-load path must actually
/// compile + ship them the first time. See the safety note on
/// [`SharedPreload`] for how the map's invariants are enforced.
pub type PreloadedPaths = HashMap<PathBuf, SystemTime>;

/// Shared handle callers hold to update the "already-loaded"
/// map after each session commit. The RPC handler holds the same
/// Arc so reads see the most recent update without any explicit
/// message passing.
///
/// **Invariant**: only entries added to this map after Vivado
/// has finished evaling the corresponding Tcl are safe to skip.
/// Adding a path prematurely would cause `compile_htcl_module`
/// to omit content Vivado hasn't seen yet, leading to
/// `invalid command name` errors at runtime. The REPL updates
/// after every batch's `EvalDone`; the CLI updates once at the
/// end of the initial load.
pub type SharedPreload = Arc<RwLock<PreloadedPaths>>;

/// Build the FnHandler used by every Vivado spawn. `workspace_root`
/// is the workspace-root path to serve for `vw::workspace_root`;
/// pass `None` when the caller couldn't discover one (the RPC
/// method then returns an error instead of a bogus path).
pub fn make_handler(workspace_root: Option<PathBuf>) -> Arc<dyn RpcHandler> {
    make_handler_with_variant(workspace_root, None)
}

/// Like [`make_handler`] but also carries a session-scoped active
/// variant name — the value the CLI's `--variant <name>` flag
/// picked. When present, RPC methods that filter by variant
/// (currently `vhdl_design_sources`) fall back to this instead
/// of the workspace default. Explicit per-call `variant` kwargs
/// still take precedence.
pub fn make_handler_with_variant(
    workspace_root: Option<PathBuf>,
    active_variant: Option<String>,
) -> Arc<dyn RpcHandler> {
    let preloaded: SharedPreload = Arc::new(RwLock::new(HashMap::new()));
    make_handler_with_preloaded(workspace_root, active_variant, preloaded)
}

/// Full-detail constructor: carries the shared preload map that
/// [`compile_htcl_module`] consults. Callers who want on-demand
/// htcl loading to skip files already shipped to this Vivado
/// session should clone the returned map's Arc, hold onto it,
/// and update it after every successful load.
pub fn make_handler_with_preloaded(
    workspace_root: Option<PathBuf>,
    active_variant: Option<String>,
    preloaded: SharedPreload,
) -> Arc<dyn RpcHandler> {
    let workspace_root = workspace_root.map(Arc::new);
    let active_variant = active_variant.map(Arc::new);
    FnHandler::new(move |method: String, args: Value| {
        let ws = workspace_root.clone();
        let av = active_variant.clone();
        let pl = preloaded.clone();
        async move {
            dispatch(
                &method,
                args,
                ws.as_deref().map(|p| p.as_ref()),
                av.as_deref().map(|s| s.as_str()),
                &pl,
            )
            .await
        }
    })
}

async fn dispatch(
    method: &str,
    args: Value,
    workspace_root: Option<&std::path::Path>,
    active_variant: Option<&str>,
    preloaded: &SharedPreload,
) -> Result<Value, String> {
    match method {
        "workspace_root" => workspace_root
            .map(|p| Value::String(p.to_string_lossy().into_owned()))
            .ok_or_else(|| {
                "no workspace root: entry file has no `vw.toml` in its \
                 parent chain"
                    .to_string()
            }),
        "active_variant" => {
            Ok(active_variant_value(workspace_root, active_variant))
        }
        "project_name" => project_name_value(workspace_root),
        "diff_files" => diff_files(args),
        "vhdl_dependency_sources" => {
            vhdl_dependency_sources(
                workspace_root,
                /*include_test=*/ false,
                extract_exclude_sim_only(&args),
            )
            .await
        }
        "vhdl_dependency_sources_with_test" => {
            vhdl_dependency_sources(
                workspace_root,
                /*include_test=*/ true,
                extract_exclude_sim_only(&args),
            )
            .await
        }
        "vhdl_design_sources" => vhdl_design_sources(
            workspace_root,
            extract_variant(&args).or_else(|| active_variant.map(String::from)),
        ),
        "vhdl_ip_sources" => vhdl_ip_sources(workspace_root),
        "design_constraints" => design_constraints(workspace_root),
        "design_synth_constraints" => {
            design_phase_constraints(workspace_root, ConstraintPhase::Synth)
        }
        "design_place_constraints" => {
            design_phase_constraints(workspace_root, ConstraintPhase::Place)
        }
        "design_route_constraints" => {
            design_phase_constraints(workspace_root, ConstraintPhase::Route)
        }
        "synth_needs_update" => {
            synth_needs_update(workspace_root, active_variant, args)
        }
        "synth_mark_checkpoint" => {
            synth_mark_checkpoint(workspace_root, active_variant, args)
        }
        "ip_needs_update" => ip_needs_update(workspace_root, args),
        "ip_mark_checkpoint" => ip_mark_checkpoint(workspace_root, args),
        "compile_htcl_module" => {
            compile_htcl_module(workspace_root, args, preloaded).await
        }
        other => Err(format!("unknown RPC method: {other}")),
    }
}

/// `compile_htcl_module` — inputs `{path: "<import>"}` where
/// `<import>` follows the same resolution rules as an htcl `src`
/// statement (relative path, absolute path, `@dep/…`, or
/// directory-as-module). Loads the entry file + every transitive
/// import, lowers each command to Tcl, and returns the
/// concatenated Tcl string.
///
/// Callers `eval` the returned string to install everything the
/// module exports into the current interpreter. Used by
/// `vw::configure_ip` to auto-load `<ws>/ip/module.htcl` when
/// `::ip::configure` isn't already defined — so the user doesn't
/// have to remember to `src ip` in their `design.htcl` before
/// calling the wrapper.
///
/// The pipeline mirrors what `vw run` / `vw repl` do internally
/// (parse → sig-table → per-command lowering → extern rewrite)
/// but skips overload dispatchers, putr rewrites, and origin
/// markers. Those matter for user-facing tracebacks and
/// interactive `putr` — neither is needed for on-demand loading
/// of a well-formed module.
async fn compile_htcl_module(
    workspace_root: Option<&std::path::Path>,
    args: Value,
    preloaded: &SharedPreload,
) -> Result<Value, String> {
    // Extract owned inputs so the closure passed to
    // `spawn_blocking` doesn't borrow anything from this async
    // frame. `path` becomes a String, the preload map is
    // snapshotted here (short critical section on the RwLock —
    // NOT held across the compile).
    let ws = workspace_root_or_error(workspace_root)?;
    let obj = args.as_object().ok_or_else(|| {
        "compile_htcl_module: args must be an object with a `path` \
         string field"
            .to_string()
    })?;
    let path: String = obj
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            "compile_htcl_module: missing string `path`".to_string()
        })?;
    let preloaded_snapshot: std::collections::HashMap<
        std::path::PathBuf,
        std::time::SystemTime,
    > = preloaded.read().map(|g| g.clone()).unwrap_or_default();
    let ws_owned = ws.clone();

    // Offload the heavy work — file I/O, htcl parse, per-command
    // lowering — to the blocking thread pool. Without this, the
    // async frame runs to completion on the current runtime
    // thread and starves every other task (in particular the
    // REPL's 250ms redraw tick, which is why the input timer
    // appeared to freeze during long compiles). Only mouse
    // movement was un-freezing it because a mouse event would
    // hit the `crossterm_events` branch of the select! and
    // trigger a draw as a side effect of handling the event.
    tokio::task::spawn_blocking(move || {
        compile_htcl_module_blocking(ws_owned, path, preloaded_snapshot)
    })
    .await
    .map_err(|e| format!("compile_htcl_module: join error: {e}"))?
}

/// Sync core of `compile_htcl_module` — runs on the tokio
/// blocking thread pool so the async runtime stays responsive.
/// All heavy work (loader recursion, parse, lower, extern
/// rewrite, disk writes) lives here.
///
/// Disk cache: after a successful compile, writes a manifest at
/// `<ws>/target/.vw-compile-<sanitized-path>.tcl.manifest`
/// listing every loaded file's path + mtime. On subsequent
/// invocations, if the manifest exists AND every file's current
/// mtime matches, returns the cached `.tcl` verbatim in
/// milliseconds instead of re-running the loader.
///
/// The cache is invalidated by any file's mtime changing OR the
/// preload set changing (folded into the manifest fingerprint).
/// Not invalidated by ADDING new files (a new `.htcl` under `ip/`
/// wouldn't be in the manifest); users adding sources should
/// `rm target/.vw-compile-*` to force recompile.
fn compile_htcl_module_blocking(
    ws: camino::Utf8PathBuf,
    path: String,
    preloaded_snapshot: std::collections::HashMap<
        std::path::PathBuf,
        std::time::SystemTime,
    >,
) -> Result<Value, String> {
    // Compute cache paths early so we can short-circuit on hit.
    let dbg_name = format!(
        ".vw-compile-{}.tcl",
        path.replace(['/', '\\'], "_").replace('@', "at_"),
    );
    let target_dir = ws.join("target");
    let cache_tcl = target_dir.join(&dbg_name);
    let cache_manifest_name = format!("{dbg_name}.manifest");
    let cache_manifest = target_dir.join(&cache_manifest_name);

    // Cache hit path: manifest exists AND every listed file's
    // current mtime matches. The manifest also embeds a hash of
    // the preload snapshot's paths (sorted) — if the caller now
    // has a different set of files preloaded, the compile output
    // would differ, so we invalidate on that too.
    if let Some(cached) = try_load_cached_compile(
        cache_manifest.as_std_path(),
        cache_tcl.as_std_path(),
        &preloaded_snapshot,
    ) {
        return Ok(Value::String(cached));
    }
    // Build the resolver with the workspace's transitive deps plus
    // the Cargo-parity self-injection. Mirrors vw-cli's
    // `load_htcl_program_with_mode` at a minimum — we don't need
    // test deps here (the auto-load path never fires from inside
    // a test).
    let mut resolver = vw_htcl::Resolver::new();
    if let Ok(paths) = vw_lib::transitive_dep_cache_paths(&ws) {
        for (name, cache_path) in paths {
            resolver = resolver.with_dep(name, cache_path);
        }
    }
    if let Ok(cfg) = vw_lib::load_workspace_config(&ws) {
        resolver = resolver.with_dep_if_absent(
            cfg.workspace.name.clone(),
            ws.as_std_path().to_path_buf(),
        );
    }

    // Resolve the import path against the workspace root (that's
    // how `design.htcl` would resolve `src ip`). Directory-as-
    // module + `.htcl` extension logic is inside `resolve()`.
    let entry = resolver
        .resolve(ws.as_std_path(), &path)
        .map_err(|e| format!("compile_htcl_module: {e}"))?;

    // Preload: skip files the caller has already shipped to this
    // Vivado session.
    //
    // Correctness rule: only files that are actually installed
    // in the Vivado interp belong in this map. An earlier version
    // of this code preloaded every `.htcl` under every dep root
    // the resolver knew about — that was wrong because a
    // workspace can declare deps (e.g. `@cpm5` / `@cips` /
    // `@clk-wizard` in metroid) that `design.htcl`'s entry
    // graph never actually pulls in. Those procs weren't
    // installed at startup, and preloading their files caused
    // `invalid command name "cpm5::cpm_pcie0"` at runtime.
    // Trusting the caller-populated map avoids that class of
    // bug entirely.
    let mut noop = NoopLoadObserver;
    let program = vw_htcl::loader::load_with_preloaded(
        &entry,
        &resolver,
        &mut noop,
        &preloaded_snapshot,
    )
    .map_err(|e| format!("compile_htcl_module: loading {path}: {e}"))?;

    // Parse the flattened source once, then build the same set
    // of auxiliary tables vw-cli's runner uses. Enum-prelude and
    // overload-dispatcher emission both consult these — skipping
    // them means user procs referencing an enum namespace (e.g.
    // `proc Property::as_nested`) or a monomorphized generic
    // repr (e.g. `dict_string_Property::repr`) reach Tcl before
    // the namespace exists and error with `unknown namespace`.
    let parsed = vw_htcl::parser::parse(&program.source);
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
    let table = vw_htcl::signature_table(&parsed.document);

    let mut out = String::new();
    // Preludes first — namespace/proc definitions the lowered
    // commands below depend on. Order matches vw-cli's runner:
    // primitives create root type namespaces, enum preludes
    // create per-enum namespaces + variant constructors, and
    // overload dispatchers create the switch-arm procs.
    //
    // All three emissions are idempotent (Tcl `namespace eval X
    // {}` on an existing X is a no-op, `proc` redefinition
    // replaces). Safe to re-ship even when design.htcl already
    // installed the same preludes at startup.
    for p in vw_htcl::emit_primitive_prelude() {
        out.push_str(&p);
        out.push('\n');
    }
    for ed in enum_decl_table.values() {
        let prelude = vw_htcl::emit_enum_prelude(ed);
        if !prelude.trim().is_empty() {
            out.push_str(&prelude);
            out.push('\n');
        }
    }
    for info in overload_table.values() {
        let dispatcher = vw_htcl::emit_dispatcher(info);
        if !dispatcher.trim().is_empty() {
            out.push_str(&dispatcher);
            out.push('\n');
        }
    }

    // Now the lowered user commands. Same per-statement lowering
    // vw-cli does (minus putr rewrites and origin markers — this
    // path never fires from an interactive `putr` and the caller
    // already has an origin frame from `vw::configure_ip`).
    //
    // Critically: proc declarations for overload specializations
    // get lowered under a MANGLED internal name (e.g.
    // `Property::as_nested::v_Property::Scalar`) so the
    // dispatchers emitted above can route to the right variant.
    // Without this, both overloads of `Property::as_nested`
    // land at the same unmangled name and the second definition
    // silently shadows the first via Tcl proc redefinition —
    // which produces `called on Scalar value` errors when the
    // dispatcher expects the mangled variants to exist.
    //
    // Perf: use the `_with_putr_and_index` / `_with_name_and_index`
    // variants and pass a PRE-BUILT `LineIndex`. The non-`_index`
    // variants rebuild a LineIndex per call — an O(source_size)
    // newline scan. For a 16 MB / 13k-statement compile that was
    // O(stmts × source_size) quadratic and dominated wall-clock
    // at ~100s. With a shared index the loop is O(stmts × avg
    // command body) which finishes in a couple of seconds.
    let line_index = vw_htcl::LineIndex::new(&program.source);
    let empty_putr: std::collections::HashMap<vw_htcl::span::Span, String> =
        std::collections::HashMap::new();
    for stmt in &parsed.document.stmts {
        let vw_htcl::ast::Stmt::Command(cmd) = stmt else {
            continue;
        };
        let lowered = match overload_specialization_mangle(cmd, &overload_table)
        {
            Some(mangled) => {
                let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
                    unreachable!(
                        "overload_specialization_mangle already \
                             validated this is a Proc"
                    )
                };
                vw_htcl::lower_proc_decl_with_name_and_index(
                    proc,
                    &program.source,
                    &table,
                    Some(&mangled),
                    &empty_putr,
                    &line_index,
                )
            }
            None => vw_htcl::lower_command_with_putr_and_index(
                cmd,
                &program.source,
                &table,
                &empty_putr,
                &line_index,
            ),
        };
        // `extern::name` → `::name` so wrapper bodies that forward
        // via `extern::` reach Vivado as bare native names — same
        // rewrite vw-cli applies before shipping to the backend.
        let tcl = vw_htcl::rewrite_externs(&lowered).text;
        if !tcl.trim().is_empty() {
            out.push_str(&tcl);
            out.push('\n');
        }
    }

    // Persist the cache: compiled Tcl + a manifest listing every
    // loaded file's path + mtime + a hash of the preload set.
    // Next invocation short-circuits if all mtimes match AND the
    // preload hash matches (same set of files already-loaded).
    // Silent on write errors — cache misses on next run are
    // annoying but not incorrect.
    let _ = std::fs::create_dir_all(target_dir.as_std_path());
    let _ = std::fs::write(cache_tcl.as_std_path(), &out);
    let _ = write_compile_manifest(
        cache_manifest.as_std_path(),
        &program,
        &preloaded_snapshot,
    );

    Ok(Value::String(out))
}

/// Manifest file format (plain text):
/// ```text
/// preload-hash <u64>
/// <mtime-ns> <path>
/// <mtime-ns> <path>
/// ...
/// ```
/// One line per loaded file. `preload-hash` is FNV-1a over the
/// sorted list of preload paths so a caller-side change to
/// what's already-loaded invalidates the cache too.
fn write_compile_manifest(
    manifest_path: &std::path::Path,
    program: &vw_htcl::LoadedProgram,
    preloaded: &std::collections::HashMap<
        std::path::PathBuf,
        std::time::SystemTime,
    >,
) -> std::io::Result<()> {
    use std::fmt::Write;
    let mut body = String::new();
    let ph = preload_fingerprint(preloaded);
    writeln!(body, "preload-hash {ph}").ok();
    for f in &program.files {
        let Some(mtime) = f.mtime else { continue };
        let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        writeln!(body, "{} {}", dur.as_nanos(), f.path.display()).ok();
    }
    std::fs::write(manifest_path, body)
}

/// Try to serve a compile from the cache. Returns `Some(tcl)` iff
/// - the manifest exists,
/// - every listed file's current mtime matches the recorded one,
/// - AND the recorded preload-hash matches the current one.
///
/// Any mismatch (or missing file, or unreadable cache) → `None`,
/// and the caller falls through to a fresh compile.
fn try_load_cached_compile(
    manifest_path: &std::path::Path,
    cache_path: &std::path::Path,
    preloaded: &std::collections::HashMap<
        std::path::PathBuf,
        std::time::SystemTime,
    >,
) -> Option<String> {
    let manifest = std::fs::read_to_string(manifest_path).ok()?;
    let mut lines = manifest.lines();
    let ph_line = lines.next()?;
    let ph_str = ph_line.strip_prefix("preload-hash ")?;
    let stored_ph: u64 = ph_str.parse().ok()?;
    if stored_ph != preload_fingerprint(preloaded) {
        return None;
    }
    for line in lines {
        let mut parts = line.splitn(2, ' ');
        let stored_ns: u128 = parts.next()?.parse().ok()?;
        let path = std::path::Path::new(parts.next()?);
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta.modified().ok()?;
        let current_ns =
            mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        if current_ns != stored_ns {
            return None;
        }
    }
    // All checks pass — serve the cached Tcl.
    std::fs::read_to_string(cache_path).ok()
}

/// FNV-1a over the sorted preload path set. Deliberately ignores
/// mtimes — the file-mtime check above already covers content
/// changes for preloaded files; this fingerprint only detects
/// changes to WHICH files are preloaded (a different caller state).
fn preload_fingerprint(
    preloaded: &std::collections::HashMap<
        std::path::PathBuf,
        std::time::SystemTime,
    >,
) -> u64 {
    let mut paths: Vec<&std::path::Path> =
        preloaded.keys().map(|p| p.as_path()).collect();
    paths.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in paths {
        for b in p.to_string_lossy().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Separator so `/a/b` + `/c` doesn't collide with `/a` + `/b/c`.
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `ip_needs_update` — inputs `{checkpoint: <abs path>}`; output
/// is a JSON bool. Compares the checkpoint's sidecar manifest
/// against the current fingerprint of every `.htcl` file under
/// `<ws>/ip/`. `true` when the checkpoint or manifest is missing
/// or the fingerprints disagree.
///
/// Not variant-scoped: the ip/ tree defines its own inputs
/// (BD parameters, IP configs) which don't currently vary by
/// variant. If that changes, thread `active_variant` through
/// here and into `vw_lib::ip_source_fingerprint`.
fn ip_needs_update(
    workspace_root: Option<&std::path::Path>,
    args: Value,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let checkpoint = extract_checkpoint_arg(&args, "ip_needs_update")?;
    let needs = vw_lib::ip_needs_update(&ws, std::path::Path::new(&checkpoint))
        .map_err(|e| format!("checking IP checkpoint freshness: {e}"))?;
    Ok(Value::Bool(needs))
}

/// `ip_mark_checkpoint` — inputs `{checkpoint: <abs path>}`;
/// output is a JSON null. Writes the sidecar manifest recording
/// the current `<ws>/ip/**/*.htcl` fingerprint next to the
/// checkpoint. Called by `vw::configure_ip` immediately after
/// `vivado_cmd::write_checkpoint`.
fn ip_mark_checkpoint(
    workspace_root: Option<&std::path::Path>,
    args: Value,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let checkpoint = extract_checkpoint_arg(&args, "ip_mark_checkpoint")?;
    vw_lib::write_ip_checkpoint_manifest(
        &ws,
        std::path::Path::new(&checkpoint),
    )
    .map_err(|e| format!("writing IP checkpoint manifest: {e}"))?;
    Ok(Value::Null)
}

/// Silent [`vw_htcl::loader::LoadObserver`] — the RPC path has no
/// progress bar or CLI channel to talk to. Every callback stays
/// at the default no-op impl.
struct NoopLoadObserver;
impl vw_htcl::loader::LoadObserver for NoopLoadObserver {}

/// Mirror of `vw-cli`'s `overload_specialization_mangle` and
/// `vw-repl/src/lower.rs`'s equivalent. If `cmd` is a top-level
/// `proc` whose name is an overload public name AND whose first
/// arg annotation is a qualified enum variant, return the
/// mangled internal name to emit it under. The dispatcher
/// (produced by `emit_dispatcher`) routes calls to these mangled
/// names by argument type. Skipping this in `compile_htcl_module`
/// caused both overloads of a proc to collapse onto the same
/// unmangled name — the second one silently shadowed the first
/// via Tcl proc redefinition, and the dispatcher's runtime
/// switch never found either specialization.
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

/// Shared arg extractor for the checkpoint-scoped RPC methods.
/// Every one takes `{checkpoint: <abs path>}` and errors
/// identically on missing/wrong-typed input — factoring keeps
/// the message shape consistent across
/// `synth_*` / `ip_*` handlers.
fn extract_checkpoint_arg(
    args: &Value,
    method: &str,
) -> Result<String, String> {
    let obj = args.as_object().ok_or_else(|| {
        format!(
            "{method}: args must be an object with a `checkpoint` \
             string field"
        )
    })?;
    obj.get("checkpoint")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{method}: missing string `checkpoint`"))
}

/// `synth_mark_checkpoint` — inputs `{checkpoint: <abs path>}`;
/// output is a JSON null. Writes the sidecar manifest recording
/// the tracked source set's current fingerprint next to the
/// checkpoint. Called by `vw::synth` immediately after
/// `vivado_cmd::write_checkpoint` succeeds so the next invocation
/// can compare fingerprints and skip resynthesis when the sources
/// are unchanged.
fn synth_mark_checkpoint(
    workspace_root: Option<&std::path::Path>,
    active_variant: Option<&str>,
    args: Value,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let obj = args.as_object().ok_or_else(|| {
        "synth_mark_checkpoint: args must be an object with a \
         `checkpoint` string field"
            .to_string()
    })?;
    let checkpoint =
        obj.get("checkpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "synth_mark_checkpoint: missing string `checkpoint`".to_string()
            })?;
    vw_lib::write_synth_checkpoint_manifest(
        &ws,
        std::path::Path::new(checkpoint),
        active_variant,
    )
    .map_err(|e| format!("writing checkpoint manifest: {e}"))?;
    Ok(Value::Null)
}

/// `synth_needs_update` — inputs `{checkpoint: <abs path>}`; output
/// is a JSON bool. Delegates to [`vw_lib::synth_needs_update`],
/// which stats the checkpoint against the tracked source set
/// (design VHDL, IP wrappers, synth XDC, workspace htcl,
/// vw.toml, vw.lock). Missing checkpoint → `true`; any source
/// strictly newer than the checkpoint → `true`; otherwise `false`.
///
/// The active variant is threaded through so a variant-specific
/// design surface (which vw::synth already respects via
/// `vhdl_design_sources`) is used for the mtime scan too —
/// otherwise a variant-owned file change might invalidate a
/// checkpoint that doesn't actually include it.
fn synth_needs_update(
    workspace_root: Option<&std::path::Path>,
    active_variant: Option<&str>,
    args: Value,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let obj = args.as_object().ok_or_else(|| {
        "synth_needs_update: args must be an object with a `checkpoint` \
         string field"
            .to_string()
    })?;
    let checkpoint =
        obj.get("checkpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "synth_needs_update: missing string `checkpoint`".to_string()
            })?;
    let needs = vw_lib::synth_needs_update(
        &ws,
        std::path::Path::new(checkpoint),
        active_variant,
    )
    .map_err(|e| format!("checking checkpoint freshness: {e}"))?;
    Ok(Value::Bool(needs))
}

/// `vhdl_dependency_sources` — return every transitive-dep VHDL
/// file, grouped by target library, as a JSON object of the shape
/// `{"library_name": ["/abs/path/a.vhd", "/abs/path/b.vhd"], …}`.
/// Grouped-by-library because the primary consumer is a Vivado
/// `read_vhdl -library <lib> $files` loop — the tuple-per-file
/// shape would force the caller to bucket, which we can do here.
async fn vhdl_dependency_sources(
    workspace_root: Option<&std::path::Path>,
    include_test: bool,
    exclude_sim_only: bool,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    // Auto-fetch missing git deps. `workspace_has_unlocked_git_deps`
    // is a cheap disk-only check — no network — so the common
    // "already up-to-date" path stays cheap. When it returns
    // true we invoke the same fetch machinery `vw update` uses;
    // downstream enumeration then sees a fully-populated
    // `vw.lock` and `~/.vw/deps` layout.
    let unlocked = vw_lib::workspace_has_unlocked_git_deps(&ws, include_test)
        .map_err(|e| format!("checking lockfile: {e}"))?;
    if unlocked {
        tracing::info!(
            workspace = %ws,
            "auto-updating workspace: git deps missing from vw.lock",
        );
        // Look up netrc credentials the same way `vw update` does
        // so a workspace with private git deps (e.g. an
        // organization's internal GitHub repo, gitea, …) doesn't
        // 401 when auto-update kicks in. `None` is fine when
        // every git URL is public — libgit2 falls back to
        // unauthenticated clone.
        let creds =
            vw_lib::get_access_credentials_for_workspace(&ws, include_test);
        vw_lib::update_workspace_with_token(&ws, creds)
            .await
            .map_err(|e| format!("auto-updating workspace: {e}"))?;
    }
    let sources = vw_lib::vhdl_dependency_sources_ext(
        &ws,
        include_test,
        exclude_sim_only,
    )
    .map_err(|e| format!("enumerating VHDL dep sources: {e}"))?;
    let mut by_library: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for src in sources {
        by_library
            .entry(src.library)
            .or_default()
            .push(Value::String(src.path.to_string_lossy().into_owned()));
    }
    let obj: serde_json::Map<String, Value> = by_library
        .into_iter()
        .map(|(lib, files)| (lib, Value::Array(files)))
        .collect();
    Ok(Value::Object(obj))
}

/// `vhdl_design_sources` — return every VHDL file under
/// `<workspace>/hdl/` as a JSON array of absolute-path strings.
/// Empty array when the workspace has no `hdl/` dir yet.
fn vhdl_design_sources(
    workspace_root: Option<&std::path::Path>,
    variant: Option<String>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    // When no variant was passed but the workspace declares
    // variants, fall back to the workspace default so the flow
    // "just works" from `design.htcl` without an explicit
    // selector. The design-sources filter uses the resolved
    // name to keep only shared + active-variant files.
    let resolved = match variant {
        Some(name) => Some(name),
        None => workspace_default_variant_name(&ws),
    };
    let paths =
        vw_lib::vhdl_design_sources_for_variant(&ws, resolved.as_deref())
            .map_err(|e| format!("enumerating VHDL design sources: {e}"))?;
    Ok(paths_to_json_array(paths))
}

/// Look up the workspace's default variant name. Returns `None`
/// when the workspace has no variants OR the variants block is
/// malformed (no default flag on a multi-entry list). Errors
/// are swallowed here — the caller has already produced a
/// diagnostic through the check machinery; we don't want the
/// RPC path to surface the same problem twice.
fn workspace_default_variant_name(ws: &camino::Utf8Path) -> Option<String> {
    let cfg = vw_lib::load_workspace_config(ws).ok()?;
    cfg.workspace
        .default_variant()
        .ok()
        .flatten()
        .map(|v| v.name.clone())
}

/// `project_name` — return the `[workspace] name` field of the
/// entry workspace's `vw.toml`, as a JSON string. Errors when no
/// workspace can be discovered; the shim propagates that as a
/// normal Tcl error so htcl callers can branch on it.
///
/// Used by `vw::synth` and other library procs that want to tag
/// log messages with the project name (`log::info -id [vw::project_name]
/// -msg "…"`) without hard-coding it in every entry file.
fn project_name_value(
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let cfg = vw_lib::load_workspace_config(&ws)
        .map_err(|e| format!("failed to load workspace config at {ws}: {e}"))?;
    Ok(Value::String(cfg.workspace.name.clone()))
}

/// `active_variant` — return the name of the variant driving this
/// Vivado session, as a JSON string. Session-scoped precedence:
/// the CLI's `--variant <name>` selector wins; when unset, the
/// workspace default is used; when the workspace declares no
/// variants at all, the empty string is returned so htcl callers
/// can branch on `[vw::active_variant]` without try/catch.
fn active_variant_value(
    workspace_root: Option<&std::path::Path>,
    active_variant: Option<&str>,
) -> Value {
    if let Some(name) = active_variant {
        return Value::String(name.to_string());
    }
    let ws = workspace_root
        .and_then(|p| camino::Utf8PathBuf::from_path_buf(p.to_path_buf()).ok());
    let name = ws
        .as_deref()
        .and_then(workspace_default_variant_name)
        .unwrap_or_default();
    Value::String(name)
}

/// `vhdl_ip_sources` — return every generated IP wrapper under
/// `<workspace>/target/ip/**/*.vhd` as a JSON array of
/// absolute-path strings. Empty array when nothing has been
/// wrapped yet.
fn vhdl_ip_sources(
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let paths = vw_lib::vhdl_ip_sources(&ws)
        .map_err(|e| format!("enumerating VHDL IP sources: {e}"))?;
    Ok(paths_to_json_array(paths))
}

/// `design_constraints` — return every constraint file under
/// `<workspace>/constraints/**/*.{xdc,sdc}` as a JSON array of
/// absolute-path strings. Empty array when the workspace has no
/// `constraints/` dir.
fn design_constraints(
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let paths = vw_lib::design_constraints(&ws)
        .map_err(|e| format!("enumerating constraint files: {e}"))?;
    Ok(paths_to_json_array(paths))
}

/// Which phase-scoped constraints dir to enumerate. Mirrors the
/// phase-specific accessors in `vw_lib`; kept as a small internal
/// enum so the dispatch match up top can name each variant without
/// duplicating the workspace-root plumbing three times.
enum ConstraintPhase {
    Synth,
    Place,
    Route,
}

fn design_phase_constraints(
    workspace_root: Option<&std::path::Path>,
    phase: ConstraintPhase,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let paths = match phase {
        ConstraintPhase::Synth => vw_lib::design_synth_constraints(&ws),
        ConstraintPhase::Place => vw_lib::design_place_constraints(&ws),
        ConstraintPhase::Route => vw_lib::design_route_constraints(&ws),
    }
    .map_err(|e| format!("enumerating phase-scoped constraint files: {e}"))?;
    Ok(paths_to_json_array(paths))
}

/// Pull the `variant` string out of an RPC args object. Missing /
/// null / non-string values return `None` — the handler then
/// falls back to the workspace's default variant.
fn extract_variant(args: &Value) -> Option<String> {
    args.as_object()
        .and_then(|o| o.get("variant"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// Pull the `exclude_sim_only` boolean out of an RPC args object.
/// Missing / null / non-boolean values default to `false` so the
/// legacy call shape (`vw::vhdl_dependency_sources` with no args)
/// stays byte-for-byte compatible.
fn extract_exclude_sim_only(args: &Value) -> bool {
    args.as_object()
        .and_then(|o| o.get("exclude_sim_only"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn paths_to_json_array(paths: Vec<std::path::PathBuf>) -> Value {
    Value::Array(
        paths
            .into_iter()
            .map(|p| Value::String(p.to_string_lossy().into_owned()))
            .collect(),
    )
}

/// Small helper: workspace-root paths served through the RPC are
/// std `Path`, but `vw_lib` takes `Utf8Path`. Do the conversion in
/// one place and normalize the error to the RPC message shape.
fn workspace_root_or_error(
    workspace_root: Option<&std::path::Path>,
) -> Result<camino::Utf8PathBuf, String> {
    let raw = workspace_root.ok_or_else(|| {
        "no workspace root: entry file has no `vw.toml` in its parent chain"
            .to_string()
    })?;
    camino::Utf8PathBuf::from_path_buf(raw.to_path_buf()).map_err(|p| {
        format!("workspace root is not valid UTF-8: {}", p.display())
    })
}

/// `diff_files` — inputs `{actual: <path>, expected: <path>}`;
/// output is a JSON string carrying the unified diff, or an empty
/// string when the files are byte-equal. Paths are used verbatim
/// (the htcl side already resolves them relative to workspace
/// root before dispatching).
fn diff_files(args: Value) -> Result<Value, String> {
    let obj = args.as_object().ok_or_else(|| {
        "diff_files: args must be an object with `actual` and \
         `expected` string fields"
            .to_string()
    })?;
    let actual = obj
        .get("actual")
        .and_then(Value::as_str)
        .ok_or_else(|| "diff_files: missing string `actual`".to_string())?;
    let expected = obj
        .get("expected")
        .and_then(Value::as_str)
        .ok_or_else(|| "diff_files: missing string `expected`".to_string())?;
    let a_bytes = std::fs::read(actual)
        .map_err(|e| format!("reading actual `{actual}`: {e}"))?;
    let b_bytes = std::fs::read(expected)
        .map_err(|e| format!("reading expected `{expected}`: {e}"))?;
    if a_bytes == b_bytes {
        return Ok(Value::String(String::new()));
    }
    // Prefer text mode when both files are valid UTF-8 — the
    // typical case for VHDL/htcl/SDC/XDC. When either side is
    // binary, fall through to a short "binary files differ"
    // marker so we don't spew hex.
    let (Ok(a_text), Ok(b_text)) =
        (std::str::from_utf8(&a_bytes), std::str::from_utf8(&b_bytes))
    else {
        return Ok(Value::String(format!(
            "binary files differ ({} bytes actual, {} bytes expected)",
            a_bytes.len(),
            b_bytes.len(),
        )));
    };
    let diff = render_unified_diff(a_text, b_text, expected, actual);
    Ok(Value::String(diff))
}

/// Render a colored unified diff between `expected` and `actual`.
/// The header lines put `expected` on `-` (removed) and `actual`
/// on `+` (added), which matches the "you wanted X but got Y"
/// narrative that assertion messages want.
fn render_unified_diff(
    actual: &str,
    expected: &str,
    expected_path: &str,
    actual_path: &str,
) -> String {
    use colored::Colorize;
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(expected, actual);
    let mut out = String::new();
    out.push_str(&format!("--- {}\n", expected_path).red().to_string());
    out.push_str(&format!("+++ {}\n", actual_path).green().to_string());
    for group in diff.grouped_ops(3) {
        // Each group is a run of consecutive ops sharing context —
        // similar's own recommended unit for rendering per-hunk
        // headers.
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                let mut line = String::new();
                line.push_str(sign);
                for (emphasized, value) in change.iter_strings_lossy() {
                    if emphasized {
                        // Inline-changed subrun — bold within the
                        // colored line to draw the eye to the exact
                        // byte-run that differs.
                        match change.tag() {
                            ChangeTag::Delete => {
                                line.push_str(
                                    &value.on_red().white().bold().to_string(),
                                );
                            }
                            ChangeTag::Insert => {
                                line.push_str(
                                    &value
                                        .on_green()
                                        .white()
                                        .bold()
                                        .to_string(),
                                );
                            }
                            ChangeTag::Equal => line.push_str(&value),
                        }
                    } else {
                        line.push_str(&value);
                    }
                }
                // `iter_inline_changes` already emits the trailing
                // newline for line-based diffs; only add one if
                // it's missing so the last line of a hunk renders
                // cleanly.
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                let colored_line = match change.tag() {
                    ChangeTag::Delete => line.red().to_string(),
                    ChangeTag::Insert => line.green().to_string(),
                    ChangeTag::Equal => line.dimmed().to_string(),
                };
                out.push_str(&colored_line);
            }
        }
    }
    out
}
