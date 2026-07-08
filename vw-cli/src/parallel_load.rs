// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Parallel htcl loader with per-dep progress rows.
//!
//! Wraps the sync `vw_htcl` load primitives with an async coordinator
//! that:
//!
//! - Recursively discovers and parses files, with `tokio::task::
//!   spawn_blocking` for each file's read+parse so N cores work in
//!   parallel.
//! - Stitches the results back into the SAME `LoadedProgram` shape
//!   the serial loader produces (flat `source` + `regions`), in the
//!   same deterministic DFS-from-entry order — so every downstream
//!   consumer (validator, putr, lower, LSP) works unchanged.
//! - Fires observer events (`on_source`, `on_parsed`, `on_dep_
//!   completed`) as parses complete, so the CLI's
//!   [`MultiProgress`] UI can render one live row per top-level
//!   dep and commit each row when its subtree is done.
//!
//! The loader is dependency-graph aware via `petgraph`. Cycles are
//! detected and rejected with `LoadError::Cycle`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use futures::future::BoxFuture;
use futures::FutureExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::task::JoinError;
use vw_htcl::{
    parse, CommandKind, Document, ImportEdge, LoadError, LoadedFile,
    LoadedProgram, Resolver, SourceRegion, Span, Stmt,
};

/// Extended observer trait for the parallel loader. Adds
/// [`on_dep_completed`] so the UI can commit a top-level dep's
/// progress row when its whole subtree has finished parsing.
///
/// The base `LoadObserver` methods still fire (once per file, from
/// blocking-thread contexts so implementations must be Send + Sync).
pub trait ParallelObserver: Send + Sync {
    fn on_source(&self, _raw: &str, _resolved: &Path) {}
    fn on_parsed(&self, _file: &Path, _raw: Option<&str>) {}
    /// A top-level dep name (`@vivado-cmd`, `@cpm5`, ...) has
    /// finished parsing its whole subtree. Used by the CLI to
    /// commit the dep's progress row to `Checking @<name>`.
    fn on_dep_completed(&self, _dep_name: &str) {}
}

/// The result of parsing one file. Shared across all consumers via
/// `Arc` so multiple parts of the stitch phase can hold references
/// without cloning the source text.
struct ParsedFile {
    path: PathBuf,
    source: String,
    document: Document,
    mtime: Option<SystemTime>,
    /// Import descriptors in source order, each resolved to a
    /// canonical path. Preserving order matters for the stitch
    /// phase's chunking output.
    imports: Vec<ImportInfo>,
}

#[derive(Clone)]
struct ImportInfo {
    raw: String,
    resolved: PathBuf,
    /// Span of the `src` command in this file's local source
    /// (the source the parse was against, not the flat source).
    src_span: Span,
}

/// Shared parse-cache. One slot per canonical path; the slot's
/// `Notify` fires when the parse completes, so a second discovery
/// of the same path awaits the first task's result instead of
/// re-parsing.
type ParseCache = Arc<Mutex<HashMap<PathBuf, ParseSlot>>>;

#[derive(Clone)]
enum ParseSlot {
    /// In-progress. Awaiters watch the notify.
    Pending(Arc<tokio::sync::Notify>),
    /// Completed successfully.
    Done(Arc<ParsedFile>),
    /// Failed. Error is stringified because `LoadError` isn't `Clone`.
    Failed(String),
    /// Preloaded and unchanged (mtime match). Skip; downstream
    /// treats this file as if it wasn't there (it's already in a
    /// prior batch's LoadedProgram).
    Skipped,
}

/// Public entry point for the parallel loader. Same contract as
/// `vw_htcl::load_program`: reads the entry file, recursively
/// resolves every `src` statement, and returns a `LoadedProgram`
/// whose flat source is DFS-ordered exactly as the serial loader
/// would produce.
///
/// `preloaded` mirrors the sync loader's cross-batch cache — files
/// listed here whose current mtime matches the stored one are
/// skipped as if already sourced.
pub async fn load_parallel(
    entry: &Path,
    resolver: Arc<Resolver>,
    observer: Arc<dyn ParallelObserver>,
    preloaded: HashMap<PathBuf, SystemTime>,
) -> Result<LoadedProgram, LoadError> {
    let entry = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let cache: ParseCache = Arc::new(Mutex::new(HashMap::new()));
    let preloaded = Arc::new(preloaded);
    let deps_in_flight: Arc<Mutex<HashMap<String, usize>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Recursive parallel discovery+parse. The entry file has no
    // dep context; every reachable file inherits from its
    // importer's dep bucket (see `bucket_for_path` used by the
    // observer wiring).
    schedule_parse(
        entry.clone(),
        None,
        resolver.clone(),
        observer.clone(),
        cache.clone(),
        preloaded.clone(),
        deps_in_flight.clone(),
    )
    .await?;

    // Serially stitch the flat source in the same DFS order the
    // sync loader produces. This is what makes the output
    // interchangeable with `vw_htcl::load_program` for downstream
    // consumers.
    stitch(&entry, &cache)
}

/// Await the parse of `path` (kicking it off first if no other
/// task has). Recurses into imports concurrently, so multiple
/// file trees load in parallel.
fn schedule_parse(
    path: PathBuf,
    imported_via_raw: Option<String>,
    resolver: Arc<Resolver>,
    observer: Arc<dyn ParallelObserver>,
    cache: ParseCache,
    preloaded: Arc<HashMap<PathBuf, SystemTime>>,
    deps_in_flight: Arc<Mutex<HashMap<String, usize>>>,
) -> BoxFuture<'static, Result<(), LoadError>> {
    async move {
        // Fast path: skip preloaded files whose mtime hasn't
        // changed. Same cross-batch semantics as the sync loader.
        if let Some(stored_mtime) = preloaded.get(&path) {
            let current_mtime = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            if current_mtime == Some(*stored_mtime) {
                let mut guard = cache.lock().unwrap();
                guard.entry(path.clone()).or_insert(ParseSlot::Skipped);
                return Ok(());
            }
        }

        // Claim the slot. If someone else already claimed, await
        // their notify; if already done, return.
        enum Claim {
            Done,
            Failed(String),
            AwaitPending(Arc<tokio::sync::Notify>),
            Owned(Arc<tokio::sync::Notify>),
        }
        let claim = {
            let mut guard = cache.lock().unwrap();
            match guard.get(&path).cloned() {
                Some(ParseSlot::Done(_)) | Some(ParseSlot::Skipped) => {
                    Claim::Done
                }
                Some(ParseSlot::Failed(msg)) => Claim::Failed(msg),
                Some(ParseSlot::Pending(n)) => Claim::AwaitPending(n),
                None => {
                    let n = Arc::new(tokio::sync::Notify::new());
                    guard.insert(path.clone(), ParseSlot::Pending(n.clone()));
                    Claim::Owned(n)
                }
            }
        };
        let notify = match claim {
            Claim::Done => return Ok(()),
            Claim::Failed(msg) => {
                return Err(LoadError::Io {
                    path: path.clone(),
                    source: std::io::Error::other(msg),
                });
            }
            Claim::AwaitPending(n) => {
                n.notified().await;
                return Ok(());
            }
            Claim::Owned(n) => n,
        };

        // Do read+parse on a blocking thread. Reading is I/O
        // bound; parsing is CPU bound. spawn_blocking is the
        // right primitive.
        let parsed_path = path.clone();
        let parse_task =
            tokio::task::spawn_blocking(move || read_and_parse(&parsed_path));

        let parsed_result = parse_task.await.map_err(join_err_to_load_err)?;
        let parsed = match parsed_result {
            Ok(p) => Arc::new(p),
            Err(e) => {
                let msg = format!("{e}");
                let mut guard = cache.lock().unwrap();
                guard.insert(path.clone(), ParseSlot::Failed(msg));
                notify.notify_waiters();
                return Err(e);
            }
        };

        // Extract src operands from the parsed doc, resolve
        // each, and schedule parallel parses for the targets.
        // The `imports` field on ParsedFile carries the ordered
        // (raw, resolved) pairs; we resolve here so we can
        // emit the on_source event with the resolved path.
        let parent_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let mut imports: Vec<ImportInfo> = Vec::new();
        for stmt in &parsed.document.stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            let CommandKind::Src(import) = &cmd.kind else {
                continue;
            };
            let Some(raw) = import.path.as_deref() else {
                let line = line_of(&parsed.source, cmd.span.start) + 1;
                let err = LoadError::DynamicPath {
                    importer: path.clone(),
                    line,
                };
                let mut guard = cache.lock().unwrap();
                guard.insert(path.clone(), ParseSlot::Failed(format!("{err}")));
                notify.notify_waiters();
                return Err(err);
            };
            let resolved =
                resolver.resolve(&parent_dir, raw).map_err(|source| {
                    LoadError::Resolve {
                        importer: path.clone(),
                        raw: raw.to_string(),
                        source,
                    }
                })?;
            imports.push(ImportInfo {
                raw: raw.to_string(),
                resolved,
                src_span: cmd.span,
            });
        }

        // Attach imports to the parsed file so the stitch phase
        // can reproduce the byte order. Use the `Arc` trick:
        // wrap in a new Arc after mutating.
        let parsed_with_imports = Arc::new(ParsedFile {
            path: parsed.path.clone(),
            source: parsed.source.clone(),
            document: parsed.document.clone(),
            mtime: parsed.mtime,
            imports: imports.clone(),
        });

        // Emit on_source for each not-yet-seen target BEFORE
        // recursing. The observer sees Sourcing events in the
        // same conceptual order the serial loader would.
        for imp in &imports {
            let already_seen = cache
                .lock()
                .unwrap()
                .get(&imp.resolved)
                .map(|s| !matches!(s, ParseSlot::Failed(_)))
                .unwrap_or(false);
            if !already_seen {
                observer.on_source(&imp.raw, &imp.resolved);
                // Track top-level dep in-flight count for
                // on_dep_completed firing.
                if let Some(dep) = extract_dep_from_raw(&imp.raw) {
                    let mut deps = deps_in_flight.lock().unwrap();
                    *deps.entry(dep).or_insert(0) += 1;
                }
            }
        }

        // Spawn recursive parses concurrently.
        let mut recurse_tasks = Vec::new();
        for imp in imports.iter() {
            let fut = schedule_parse(
                imp.resolved.clone(),
                Some(imp.raw.clone()),
                resolver.clone(),
                observer.clone(),
                cache.clone(),
                preloaded.clone(),
                deps_in_flight.clone(),
            );
            recurse_tasks.push(fut);
        }
        for result in futures::future::join_all(recurse_tasks).await {
            result?;
        }

        // All this file's imports are done. Publish the parse
        // result and notify awaiters.
        {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                path.clone(),
                ParseSlot::Done(parsed_with_imports.clone()),
            );
        }
        notify.notify_waiters();

        observer.on_parsed(&path, imported_via_raw.as_deref());

        // If this file is the last outstanding file of a
        // top-level dep bucket, fire on_dep_completed.
        if let Some(via_raw) = imported_via_raw.as_deref() {
            if let Some(dep) = extract_dep_from_raw(via_raw) {
                let mut deps = deps_in_flight.lock().unwrap();
                if let Some(count) = deps.get_mut(&dep) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        deps.remove(&dep);
                        drop(deps);
                        observer.on_dep_completed(&dep);
                    }
                }
            }
        }

        Ok(())
    }
    .boxed()
}

/// Sync read+parse for one file. Runs on a `spawn_blocking`
/// thread. Failures propagate as `LoadError`.
fn read_and_parse(path: &Path) -> Result<ParsedFile, LoadError> {
    let source = std::fs::read_to_string(path).map_err(|e| LoadError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let parsed = parse(&source);
    if !parsed.errors.is_empty() {
        return Err(LoadError::Parse {
            path: path.to_path_buf(),
            errors: parsed.errors,
        });
    }
    Ok(ParsedFile {
        path: path.to_path_buf(),
        source,
        document: parsed.document,
        mtime,
        imports: Vec::new(),
    })
}

/// Extract the top-level `@name` prefix from a src operand, so
/// the observer can bucket files by top-level dep. `./relative`
/// operands (dep-internal imports) return None — the CLI-side
/// observer resolves them via the loader-observer path.
fn extract_dep_from_raw(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('@')?;
    let name = match rest.split_once('/') {
        Some((n, _)) => n,
        None => rest,
    };
    Some(format!("@{name}"))
}

/// Serially stitch the flat `LoadedProgram` from the parsed
/// cache. Walks in the same DFS-from-entry order the sync
/// loader uses so byte offsets line up with what downstream
/// consumers expect.
fn stitch(
    entry: &Path,
    cache: &ParseCache,
) -> Result<LoadedProgram, LoadError> {
    let mut program = LoadedProgram::default();
    let mut loaded_files: HashMap<PathBuf, usize> = HashMap::new();
    let mut in_progress: HashSet<PathBuf> = HashSet::new();
    stitch_file(
        entry,
        None,
        cache,
        &mut program,
        &mut loaded_files,
        &mut in_progress,
    )?;
    if !program.source.ends_with('\n') {
        program.source.push('\n');
    }
    Ok(program)
}

/// Recursive DFS stitch. Same shape as the sync loader's
/// `load_file` but reads pre-parsed content from the cache.
fn stitch_file(
    path: &Path,
    imported_via: Option<ImportEdge>,
    cache: &ParseCache,
    program: &mut LoadedProgram,
    loaded_files: &mut HashMap<PathBuf, usize>,
    in_progress: &mut HashSet<PathBuf>,
) -> Result<(), LoadError> {
    if loaded_files.contains_key(path) || in_progress.contains(path) {
        return Ok(());
    }
    let parsed = {
        let guard = cache.lock().unwrap();
        match guard.get(path).cloned() {
            Some(ParseSlot::Done(p)) => p,
            Some(ParseSlot::Skipped) => return Ok(()),
            Some(ParseSlot::Failed(msg)) => {
                return Err(LoadError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other(msg),
                });
            }
            Some(ParseSlot::Pending(_)) | None => {
                return Err(LoadError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "parallel loader lost parse for this path",
                    ),
                });
            }
        }
    };
    in_progress.insert(path.to_path_buf());

    let file_index = program.files.len() as u32;
    program.files.push(LoadedFile {
        path: path.to_path_buf(),
        source: parsed.source.clone(),
        imported_via,
        mtime: parsed.mtime,
    });
    loaded_files.insert(path.to_path_buf(), file_index as usize);

    // Emit chunks + regions in span order, recursing into each
    // src target between chunks.
    let mut cursor = 0usize;
    for imp in &parsed.imports {
        // Find this src stmt's span in the doc so we chunk
        // around it. `imp.src_span` was recorded at parse time.
        let cmd_start = imp.src_span.start as usize;
        let cmd_end = imp.src_span.end as usize;
        emit_chunk(program, &parsed.source, cursor, cmd_start, file_index);
        cursor = cmd_end;
        if parsed.source.as_bytes().get(cursor) == Some(&b'\n') {
            cursor += 1;
        }
        stitch_file(
            &imp.resolved,
            Some(ImportEdge {
                importer_file: file_index as usize,
                src_span: imp.src_span,
            }),
            cache,
            program,
            loaded_files,
            in_progress,
        )?;
    }
    emit_chunk(
        program,
        &parsed.source,
        cursor,
        parsed.source.len(),
        file_index,
    );
    if !program.source.ends_with('\n') {
        program.source.push('\n');
    }
    in_progress.remove(path);
    Ok(())
}

fn emit_chunk(
    program: &mut LoadedProgram,
    source: &str,
    start: usize,
    end: usize,
    file_index: u32,
) {
    if start >= end {
        return;
    }
    let flat_start = program.source.len() as u32;
    program.source.push_str(&source[start..end]);
    let flat_end = program.source.len() as u32;
    program.regions.push(SourceRegion {
        flat_start,
        flat_end,
        file_index,
        file_offset: start as u32,
    });
}

fn line_of(source: &str, byte: u32) -> u32 {
    source[..(byte as usize).min(source.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
}

fn join_err_to_load_err(e: JoinError) -> LoadError {
    LoadError::Io {
        path: PathBuf::new(),
        source: std::io::Error::other(e.to_string()),
    }
}

/// Observer that drives an `indicatif::MultiProgress` panel — one
/// live [`ProgressBar`] per top-level dep, plus one for local /
/// workspace files.
///
/// On a TTY, each bar's message updates as its inner files fly by.
/// When [`on_dep_completed`] fires, the bar commits with
/// `Checking @<dep>` and stays in scrollback. Non-TTY stdout is
/// handled natively by `indicatif` — bars degrade to no-op and
/// we fall back to the `println` path for each event.
///
/// The observer holds bars behind an internal `Mutex` because
/// [`ProgressBar`] is `Send + Sync` but our bookkeeping around
/// bar creation (first-sight-per-dep) needs synchronized
/// access. Bar updates otherwise happen from many blocking-
/// thread contexts concurrently.
pub struct MultiProgressObserver {
    multi: MultiProgress,
    /// `(name, ProgressBar)` — one per top-level dep. `name` is
    /// the `@dep` prefix or "workspace" for local files. Insertion
    /// order preserved so scrollback matches source order.
    bars: Mutex<Vec<(String, ProgressBar)>>,
    /// `(depname, cache-directory-abs-path)` pairs used to rewrite
    /// resolved paths back into `@dep/relative` labels — same as
    /// the previous CliObserver.
    dep_paths: Vec<(String, PathBuf)>,
    /// True when stdout is a real terminal. Bars only render
    /// when true; non-TTY falls back to plain `println!`.
    stdout_is_tty: bool,
    /// Human-friendly label for the local workspace's bar. Picked
    /// up from `vw.toml`'s `name = "…"` field when available, so
    /// `Checking workspace` becomes `Checking metroid` for the
    /// metroid workspace. Falls back to the literal `workspace`
    /// when no name is configured.
    workspace_label: String,
}

impl MultiProgressObserver {
    pub fn new(
        dep_paths: Vec<(String, PathBuf)>,
        workspace_label: String,
    ) -> Self {
        use std::io::IsTerminal;
        let multi = MultiProgress::new();
        Self {
            multi,
            bars: Mutex::new(Vec::new()),
            dep_paths,
            stdout_is_tty: std::io::stdout().is_terminal(),
            workspace_label,
        }
    }

    /// Format a `src` label the same way the previous
    /// `CliObserver::friendly_import` did: resolve to
    /// `@<depname>/<relative>` when the path is inside a known
    /// dep-cache directory; otherwise strip `@` sigil and
    /// `.htcl` suffix.
    fn friendly_label(&self, raw: &str, resolved: Option<&Path>) -> String {
        if let Some(resolved) = resolved {
            let canonical = resolved
                .canonicalize()
                .unwrap_or_else(|_| resolved.to_path_buf());
            for (name, dep_path) in &self.dep_paths {
                let dep_canonical = dep_path
                    .canonicalize()
                    .unwrap_or_else(|_| dep_path.clone());
                if let Ok(rel) = canonical.strip_prefix(&dep_canonical) {
                    let rel_str = rel.display().to_string();
                    let rel_str = rel_str.trim_end_matches(".htcl");
                    return if rel_str.is_empty() {
                        format!("@{name}")
                    } else {
                        format!("@{name}/{rel_str}")
                    };
                }
            }
        }
        if !raw.is_empty() {
            return raw
                .trim_start_matches('@')
                .trim_end_matches(".htcl")
                .to_string();
        }
        resolved
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }

    /// Bucket a `label` into a top-level dep name for bar routing.
    /// `@foo/bar` → `@foo`; `foo/bar` or `foo` → the workspace
    /// label (typically the `vw.toml` name).
    fn bucket_of(&self, label: &str) -> String {
        if let Some(rest) = label.strip_prefix('@') {
            let name = match rest.split_once('/') {
                Some((n, _)) => n,
                None => rest,
            };
            format!("@{name}")
        } else {
            self.workspace_label.clone()
        }
    }

    /// Return the bar for `bucket`, creating it on first sight.
    /// Access is serialized on the bars mutex so parallel task
    /// contexts don't race.
    fn bar_for(&self, bucket: &str) -> ProgressBar {
        let mut guard = self.bars.lock().unwrap();
        if let Some((_, bar)) = guard.iter().find(|(n, _)| n == bucket) {
            return bar.clone();
        }
        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(
            ProgressStyle::with_template("{prefix:>12.bold.green} {msg}")
                .expect("static template compiles"),
        );
        bar.set_prefix("Sourcing");
        bar.set_message(bucket.to_string());
        // Steady-tick so spinner-shaped bars animate without needing
        // explicit ticks between updates.
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        guard.push((bucket.to_string(), bar.clone()));
        bar
    }
}

impl ParallelObserver for MultiProgressObserver {
    fn on_source(&self, raw: &str, resolved: &Path) {
        let label = self.friendly_label(raw, Some(resolved));
        let bucket = self.bucket_of(&label);
        if !self.stdout_is_tty {
            println!("{:>12} {label}", "Sourcing");
            return;
        }
        let bar = self.bar_for(&bucket);
        bar.set_prefix("Sourcing");
        bar.set_message(label);
    }

    fn on_parsed(&self, file: &Path, raw: Option<&str>) {
        let label = self.friendly_label(raw.unwrap_or(""), Some(file));
        let bucket = self.bucket_of(&label);
        if !self.stdout_is_tty {
            println!("{:>12} {label}", "Checking");
            return;
        }
        let bar = self.bar_for(&bucket);
        bar.set_prefix("Checking");
        bar.set_message(label);
    }

    fn on_dep_completed(&self, dep_name: &str) {
        if !self.stdout_is_tty {
            return;
        }
        let guard = self.bars.lock().unwrap();
        if let Some((_, bar)) = guard.iter().find(|(n, _)| n == dep_name) {
            bar.set_prefix("Checking");
            bar.finish_with_message(dep_name.to_string());
        }
    }
}

impl MultiProgressObserver {
    /// Finalize any bar that wasn't committed by an
    /// [`on_dep_completed`] — happens for the "workspace" bar
    /// covering local files and for the entry file's own bar.
    /// Call this after `load_parallel` returns.
    pub fn finish(&self) {
        if !self.stdout_is_tty {
            return;
        }
        {
            let guard = self.bars.lock().unwrap();
            for (name, bar) in guard.iter() {
                if !bar.is_finished() {
                    bar.set_prefix("Checking");
                    bar.finish_with_message(name.clone());
                }
            }
        }
        // Force a fresh row for any subsequent stdout output. Without
        // this, `indicatif` leaves the cursor at the end of the last
        // rendered bar row and downstream stdout writes (e.g. the
        // vw-run Vivado stream's first `puts` result) land on the
        // same line as `Checking @<last-dep>`.
        println!();
    }
}
