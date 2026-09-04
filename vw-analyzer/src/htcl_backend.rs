// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! htcl [`LanguageBackend`] — native, in-process, using `vw-htcl`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{watch, RwLock};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, Documentation, Hover, HoverContents, InsertTextFormat,
    Location, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel,
    Position, Range, SignatureHelp, SignatureInformation, SymbolInformation,
    SymbolKind, TextEdit, Url, WorkspaceEdit,
};
use tracing::debug;
use vw_htcl::{
    definition_at, find_references_in, hover_at, identify_at, parse,
    signature_help_at, validate_with_all_extras_and_vars, Attribute,
    AttributeValue, CommandKind, Completion, CompletionKind, HoverTarget,
    LineCol, LineIndex, ParseOutput, ProcArg, ProcSignature, ReferenceTarget,
    Severity, Span, Stmt,
};

use crate::backend::LanguageBackend;

#[derive(Default)]
pub struct HtclBackend {
    docs: Arc<RwLock<HashMap<Url, DocState>>>,
    /// Editor-supplied workspace roots (LSP `rootUri` /
    /// `workspaceFolders`). Consulted as a fallback when the file
    /// currently being analyzed sits outside the enclosing
    /// `vw.toml` — e.g. after a goto-def has taken the user into a
    /// dep-cache dir. Without this, dep names declared only in the
    /// editor-root workspace fail to resolve and every `@name/…`
    /// import in the visited file goes dead.
    workspace_roots: Arc<RwLock<Vec<std::path::PathBuf>>>,
}

/// Cached analysis for one open document. Populated by the
/// background indexer spawned from `set_text`, read by every
/// request handler. Parses + workspace-view + diagnostics all
/// bundled together — the source strings live in
/// `view.view_source` and `local_text`, and every span in
/// `parsed_view`/`parsed_local` indexes into them, so passing the
/// whole `Arc<DocAnalysis>` around keeps span interpretation safe.
pub(crate) struct DocAnalysis {
    /// Document text at index time — sources for every span in
    /// `parsed_local`, and for line/col translation via
    /// `local_line_index`.
    pub local_text: String,
    /// Concatenated workspace view (local text + every
    /// transitively `src`d file). Source for spans in
    /// `parsed_view` and for `line_index`.
    pub view: crate::workspace::WorkspaceView,
    pub parsed_local: ParseOutput,
    pub parsed_view: ParseOutput,
    pub local_line_index: LineIndex,
    /// Diagnostics computed at index time — parse errors from the
    /// local doc plus validator errors from the workspace view
    /// filtered to local-file spans. Served verbatim by
    /// `LanguageBackend::diagnostics`.
    pub diagnostics: Vec<Diagnostic>,
    /// Same validator errors, but for spans that land in
    /// TRANSITIVELY-imported files. Each entry is
    /// `(origin_file_uri, diagnostic_with_file_local_range)`.
    /// The `workspace/diagnostic` handler routes these back to the
    /// files that actually contain the error, giving the editor a
    /// workspace-wide picker (`space-D` in Helix) even for files
    /// the user hasn't opened.
    pub cross_file_diagnostics: Vec<(Url, Diagnostic)>,
}

struct DocState {
    text: String,
    /// Monotonic per-URI counter bumped on every `set_text`. The
    /// spawned indexer captures the generation it was created for
    /// and only pushes its result to the watch channel if the
    /// counter still matches — newer set_text having bumped it
    /// means the newer indexer will supersede us.
    generation: u64,
    /// Watch channel carrying the latest completed analysis.
    /// `None` while indexing is in flight; `Some(Arc<..>)` once
    /// the indexer commits. `set_text` sends `None` to invalidate.
    /// Request handlers subscribe and `.changed().await` until
    /// `borrow()` returns `Some`.
    tx: watch::Sender<Option<Arc<DocAnalysis>>>,
    /// Handle to the in-flight indexer. `set_text` aborts the
    /// previous handle before spawning a new one — keeps only ONE
    /// index running at a time so rapid typing doesn't back up
    /// (each keystroke's stale index runs to completion under the
    /// generation guard, but the ABORT drops it at the next .await
    /// point which shortens the wall-clock for the freshest text).
    index_task: Option<tokio::task::JoinHandle<()>>,
}

impl HtclBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only convenience: update text and wait for the indexer
    /// to commit. Production `set_text` is deliberately fire-and-
    /// forget so keystrokes never block; tests want synchronous
    /// behavior so their assertions run against a committed
    /// analysis.
    #[cfg(test)]
    pub(crate) async fn set_text_sync(&self, uri: Url, text: String) {
        use crate::backend::LanguageBackend as _;
        self.set_text(uri.clone(), text).await;
        self.wait_for_reindex(&uri).await;
    }

    /// Snapshot of the editor-supplied workspace roots. Callers
    /// pass this into [`crate::workspace::build_view`] etc. as
    /// fallback dep-lookup roots — see `workspace_roots`
    /// on the struct for the rationale. Cloned so the lock isn't
    /// held across the (potentially I/O-heavy) view build.
    async fn workspace_roots_snapshot(&self) -> Vec<std::path::PathBuf> {
        self.workspace_roots.read().await.clone()
    }

    /// Preload analyses for every entry point `vw check` would
    /// discover under each workspace root. These files land in
    /// the docs map as "virtual-open" entries — the editor never
    /// sent `did_open` for them, but their committed analysis
    /// participates in `workspace_diagnostics` the same way an
    /// actually-open file does. That's what lets space-D show
    /// warnings in files the user hasn't visited yet.
    ///
    /// Entry-point set mirrors `vw-cli`'s `discover_check_targets`:
    /// `<ws>/design.htcl`, `<ws>/module.htcl`, `<ws>/ip/module.htcl`,
    /// and every discovered `bench/**/*.htcl` test. If an entry is
    /// missing on disk (empty workspace) or the read fails,
    /// silently skip that entry — a preload failure must never
    /// block LSP startup or hide the on-open editor experience for
    /// files that DO exist.
    ///
    /// Skips entries already in the docs map so this can be called
    /// repeatedly (e.g. `did_change_workspace_folders`) without
    /// clobbering the live buffer of a file the user is editing.
    async fn preload_workspace_targets(&self, roots: &[std::path::PathBuf]) {
        use crate::backend::LanguageBackend as _;
        for root in roots {
            let root_utf8 =
                match camino::Utf8PathBuf::from_path_buf(root.clone()) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
            let ws = match vw_lib::find_workspace_dir(root_utf8.as_std_path()) {
                Some(ws) => ws,
                None => continue,
            };
            let mut targets: Vec<std::path::PathBuf> = Vec::new();
            if let Some(design) = vw_lib::find_design_file(&ws) {
                targets.push(design.into_std_path_buf());
            }
            let module = ws.join("module.htcl");
            if module.is_file() {
                targets.push(module.into_std_path_buf());
            }
            let ip_module = ws.join("ip/module.htcl");
            if ip_module.is_file() {
                targets.push(ip_module.into_std_path_buf());
            }
            if let Ok(tests) = vw_lib::list_htcl_tests(&ws) {
                targets.extend(tests);
            }
            for target in targets {
                let Ok(uri) = Url::from_file_path(&target) else {
                    continue;
                };
                {
                    let docs = self.docs.read().await;
                    if docs.contains_key(&uri) {
                        continue;
                    }
                }
                let Ok(text) = std::fs::read_to_string(&target) else {
                    continue;
                };
                debug!(%uri, "preloading workspace target");
                self.set_text(uri, text).await;
            }
        }
    }

    /// Snapshot the current in-memory text for `uri`. Used by
    /// completion / hover / signature-help handlers that need the
    /// CURRENT text (what the user just typed) rather than the
    /// analysis snapshot's stale copy. Cheap: one hashmap read + a
    /// String clone.
    pub(crate) async fn current_text(&self, uri: &Url) -> Option<String> {
        let docs = self.docs.read().await;
        docs.get(uri).map(|s| s.text.clone())
    }

    /// Resolve a `src` import path from `entry_file`'s directory,
    /// honoring the editor-supplied root fallback.
    async fn resolve_import(
        &self,
        entry_file: &std::path::Path,
        raw: &str,
    ) -> Option<std::path::PathBuf> {
        let roots = self.workspace_roots_snapshot().await;
        crate::workspace::resolve_import(entry_file, raw, &roots)
    }

    /// Return the cached analysis for `uri`, awaiting the in-flight
    /// indexer if one is currently running. Returns `None` when the
    /// document isn't tracked (never `set_text`'d or already
    /// `close`d).
    ///
    /// Contract with `set_text`: every time a new set_text fires
    /// on this URI, the watch channel receives `Some(...)` only
    /// after the indexer for THAT set_text's text commits under
    /// the generation guard. So `analysis_for` naturally waits for
    /// the LATEST index. Older in-flight indexers that finish
    /// under a stale generation are silently discarded — waiters
    /// don't get bogus results.
    pub(crate) async fn analysis_for(
        &self,
        uri: &Url,
    ) -> Option<Arc<DocAnalysis>> {
        // Non-blocking snapshot: return whatever's currently in the
        // watch channel — `Some(previous_analysis)` while a rebuild
        // is in flight (serve-stale), `None` before the very first
        // indexer has committed. Callers that CAN work with a stale
        // snapshot (completion, hover, goto-def, references) use
        // this directly; callers that need a fresh commit
        // (diagnostics publish + progress indicator) use
        // `wait_for_reindex` explicitly.
        //
        // We intentionally do NOT `await` a `changed()` here: rapid
        // typing plus a 7s workspace-validate means every keystroke
        // aborts the in-flight indexer, and if we haven't yet had a
        // first commit, waiting would block indefinitely. Returning
        // `None` immediately lets handlers degrade gracefully to a
        // local-only analysis or empty results.
        let docs = self.docs.read().await;
        let state = docs.get(uri)?;
        let out = state.tx.borrow().clone();
        out
    }

    /// Build a fresh `DocAnalysis` for `text` synchronously
    /// (parses + workspace view + validate). Called from the
    /// spawned indexer task. Extracted so it can also be invoked
    /// synchronously from tests without needing the async task
    /// plumbing.
    pub(crate) fn build_analysis(
        uri: &Url,
        text: String,
        workspace_roots: &[std::path::PathBuf],
    ) -> Arc<DocAnalysis> {
        let view = crate::workspace::build_view(uri, &text, workspace_roots);
        let parsed_local = parse(&text);
        let local_line_index = LineIndex::new(&text);
        let parsed_view = parse(&view.view_source);
        let line_index = LineIndex::new(&view.view_source);

        let mut diagnostics = Vec::new();
        // Target-compatibility check. For LSP diagnostics we anchor
        // the message on the `src @<dep>` statement in the local
        // file — much more informative than a whole-file gutter
        // marker. Only fires when the entry workspace declares a
        // target-part; libraries (no target) are no-op.
        if let Ok(file_path) = uri.to_file_path() {
            if let Some(ws) =
                file_path.parent().and_then(vw_lib::find_workspace_dir)
            {
                if let Ok(cfg) = vw_lib::load_workspace_config(&ws) {
                    // LSP checks the DEFAULT part only — cheap and
                    // reflects what `vw run` would boot with. In
                    // variant-mode workspaces the default variant's
                    // part is what runs by default; in part-mode
                    // workspaces we fall back to the default target
                    // part. The CLI's `vw check --all-parts` /
                    // `--all-variants` covers the wider matrix on
                    // demand.
                    let resolved_part: Option<String> =
                        if !cfg.workspace.variants.is_empty() {
                            cfg.workspace
                                .default_variant()
                                .ok()
                                .flatten()
                                .map(|v| v.part.clone())
                        } else {
                            cfg.workspace
                                .default_target_part()
                                .ok()
                                .flatten()
                                .map(|p| p.to_string())
                        };
                    if let Some(target_part) = resolved_part {
                        let target_part = target_part.as_str();
                        let dep_targets = vw_lib::collect_dep_targets(&ws);
                        let mismatches = vw_lib::check_target_compatibility(
                            Some(target_part),
                            &dep_targets,
                        );
                        // Anchor each mismatch on the specific
                        // `src @<dep>` line in the LOCAL file.
                        for m in &mismatches {
                            for stmt in &parsed_local.document.stmts {
                                let vw_htcl::Stmt::Command(cmd) = stmt else {
                                    continue;
                                };
                                let vw_htcl::CommandKind::Src(src) = &cmd.kind
                                else {
                                    continue;
                                };
                                let Some(raw) = &src.path else { continue };
                                // Match `@<dep>` or `@<dep>/...`.
                                let dep_name = raw
                                    .strip_prefix('@')
                                    .and_then(|rest| {
                                        rest.split_once('/')
                                            .map(|(n, _)| n)
                                            .or(Some(rest))
                                    })
                                    .unwrap_or("");
                                if dep_name != m.dep {
                                    continue;
                                }
                                let (start, end) =
                                    local_line_index.range(src.path_span);
                                let hint = target_mismatch_families_hint(m);
                                let (severity, message) = match m.kind {
                                    vw_lib::TargetMismatchKind::NotSupported => (
                                        DiagnosticSeverity::ERROR,
                                        format!(
                                            "target-part `{}` matches dep \
                                             `{}`'s `not-supported` list \
                                             — Xilinx has attested the IP \
                                             is not usable on this part \
                                             ({})",
                                            m.target_part, m.dep, hint,
                                        ),
                                    ),
                                    vw_lib::TargetMismatchKind::Unblessed => (
                                        DiagnosticSeverity::WARNING,
                                        format!(
                                            "target-part `{}` isn't blessed \
                                             by dep `{}` ({}); the IP may \
                                             still work but Xilinx hasn't \
                                             blessed the combination",
                                            m.target_part, m.dep, hint,
                                        ),
                                    ),
                                };
                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: lc_to_pos(start),
                                        end: lc_to_pos(end),
                                    },
                                    severity: Some(severity),
                                    source: Some("vw-htcl".into()),
                                    message,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }
        let mut cross_file_diagnostics: Vec<(Url, Diagnostic)> = Vec::new();
        // Local parse errors.
        for err in &parsed_local.errors {
            let (start, end) = local_line_index.range(err.span);
            diagnostics.push(Diagnostic {
                range: Range {
                    start: lc_to_pos(start),
                    end: lc_to_pos(end),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("vw-htcl".into()),
                message: err.message.clone(),
                ..Default::default()
            });
        }
        // Precompute a per-imported-file `LineIndex` on demand.
        // Kept keyed by region index so multiple diagnostics
        // landing in the same file only build the index once.
        let mut import_line_indexes: HashMap<usize, LineIndex> = HashMap::new();
        // Workspace-view validator. Diagnostics whose span sits
        // in the local prefix land in `diagnostics`; those that
        // land in an imported region get retranslated into that
        // file's own line/col and stashed for the workspace-
        // diagnostic path.
        for d in validate_with_all_extras_and_vars(
            &parsed_view.document,
            &view.view_source,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &view.dep_names,
        ) {
            let severity = match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
            };
            if d.span.start < view.local_len {
                let (start, end) = line_index.range(d.span);
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: lc_to_pos(start),
                        end: lc_to_pos(end),
                    },
                    severity: Some(severity),
                    source: Some("vw-htcl".into()),
                    message: d.message,
                    ..Default::default()
                });
                continue;
            }
            // Locate which imported file the span belongs to and
            // translate its offset into that file's coordinates.
            // Skip diagnostics that don't land in any tracked
            // region — shouldn't happen with the current view
            // builder, but the model allows for view-source
            // regions unmapped to files (empty for now).
            let Some((region_idx, region, file_offset_start)) =
                view.imports.iter().enumerate().find_map(|(i, r)| {
                    (d.span.start >= r.start && d.span.start < r.end)
                        .then(|| (i, r, d.span.start - r.start))
                })
            else {
                continue;
            };
            let file_offset_end = d.span.end.saturating_sub(region.start);
            let file_text_range =
                &view.view_source[region.start as usize..region.end as usize];
            let li = import_line_indexes
                .entry(region_idx)
                .or_insert_with(|| LineIndex::new(file_text_range));
            let (start, end) = li.range(vw_htcl::Span {
                start: file_offset_start,
                end: file_offset_end.min(region.end - region.start),
            });
            cross_file_diagnostics.push((
                region.file_uri.clone(),
                Diagnostic {
                    range: Range {
                        start: lc_to_pos(start),
                        end: lc_to_pos(end),
                    },
                    severity: Some(severity),
                    source: Some("vw-htcl".into()),
                    message: d.message,
                    ..Default::default()
                },
            ));
        }

        Arc::new(DocAnalysis {
            local_text: text,
            view,
            parsed_local,
            parsed_view,
            local_line_index,
            diagnostics,
            cross_file_diagnostics,
        })
    }

    /// Spawn a fresh indexer task for `uri` + `text`. Returns the
    /// `JoinHandle` so the caller can install it into `DocState`
    /// (and abort it on a subsequent update). `debounce` gates how
    /// long the task waits before starting the ~7s build — used
    /// by `set_text` (250ms, to coalesce rapid typing) and by
    /// `save` (0ms, so `Ctrl-s` re-checks immediately).
    ///
    /// The `spawn_blocking` inner run to completion even if the
    /// outer task is aborted; the generation guard at commit time
    /// discards any superseded result.
    fn spawn_indexer(
        &self,
        uri: Url,
        text: String,
        generation: u64,
        debounce: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        spawn_indexer_task(
            self.docs.clone(),
            self.workspace_roots.clone(),
            uri,
            text,
            generation,
            debounce,
        )
    }
}

/// Standalone version of `HtclBackend::spawn_indexer`. Split out so
/// the fan-out (`reindex_importers_of`, called from the indexer's
/// own commit path) can spawn follow-up indexers without needing a
/// live reference to the surrounding `HtclBackend` — the tokio task
/// only ever holds the two `Arc`s the indexer itself needs.
fn spawn_indexer_task(
    docs: Arc<RwLock<HashMap<Url, DocState>>>,
    workspace_roots: Arc<RwLock<Vec<std::path::PathBuf>>>,
    uri: Url,
    text: String,
    generation: u64,
    debounce: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !debounce.is_zero() {
            tokio::time::sleep(debounce).await;
        }
        let roots = workspace_roots.read().await.clone();
        let uri_inner = uri.clone();
        let analysis = match tokio::task::spawn_blocking(move || {
            HtclBackend::build_analysis(&uri_inner, text, &roots)
        })
        .await
        {
            Ok(a) => a,
            Err(_) => return,
        };
        // Commit under the generation guard. Superseded results
        // are discarded — the `set_text` handler that bumped past
        // us installed a fresher indexer that WILL commit.
        let committed = {
            let docs_r = docs.read().await;
            let Some(state) = docs_r.get(&uri) else {
                return;
            };
            if state.generation != generation {
                debug!(
                    uri = %uri,
                    generation,
                    current = state.generation,
                    "index superseded, discarded",
                );
                return;
            }
            debug!(uri = %uri, generation, "index committed");
            state.tx.send_replace(Some(analysis.clone()));
            true
        };
        // Fan out to open documents whose CURRENT analysis
        // imports `uri`. Reason: an import's disk / open-buffer
        // content changed (that's what just committed here), so
        // any doc that transitively `src`s it now has stale
        // symbol/hover data for that region. Without this fan-out,
        // editing `ip/clock.htcl` leaves `ip/module.htcl`'s hover
        // pointing at the pre-edit analysis until the user
        // touches module.htcl itself. Fan-out uses the same
        // 0ms-debounce path `save()` takes so the ripple lands
        // fast; cascading (A imports B imports C, C edits) still
        // converges because each level's fan-out only fires ONCE
        // per commit (there's no re-entry: A's rebuild doesn't
        // re-fire B's, since B's imports of A haven't changed).
        if committed {
            reindex_importers_of(&docs, &workspace_roots, &uri).await;
        }
    })
}

/// Enqueue a fresh indexer for every open doc whose currently-
/// committed analysis lists `changed` in `view.imports`. Called from
/// the indexer commit path so downstream files pick up upstream
/// edits (doc comments, signatures, new procs) without waiting for
/// the downstream file to be edited itself.
///
/// Skips docs that don't have a committed analysis yet (they'll
/// pick up the change on their first commit anyway) and skips the
/// changed URI itself (it just committed).
async fn reindex_importers_of(
    docs: &Arc<RwLock<HashMap<Url, DocState>>>,
    workspace_roots: &Arc<RwLock<Vec<std::path::PathBuf>>>,
    changed: &Url,
) {
    // Collect the (uri, text, new_generation, prev_task) tuples under
    // the write lock, then release the lock BEFORE awaiting aborts
    // and spawning new indexers — other handlers can proceed
    // concurrently. Matches the pattern in `set_text`.
    let mut to_spawn: Vec<(
        Url,
        String,
        u64,
        Option<tokio::task::JoinHandle<()>>,
    )> = Vec::new();
    {
        let mut docs_w = docs.write().await;
        // Snapshot the URIs first — we can't hold two live borrows
        // (one to iterate, one to `get_mut`) at once.
        let candidates: Vec<Url> = docs_w
            .iter()
            .filter(|(u, _)| *u != changed)
            .filter_map(|(u, state)| {
                let analysis = state.tx.borrow();
                analysis
                    .as_ref()
                    .filter(|a| {
                        a.view
                            .imports
                            .iter()
                            .any(|i| same_file(&i.file_uri, changed))
                    })
                    .map(|_| u.clone())
            })
            .collect();
        for u in candidates {
            let Some(state) = docs_w.get_mut(&u) else {
                continue;
            };
            state.generation += 1;
            let prev = state.index_task.take();
            to_spawn.push((u, state.text.clone(), state.generation, prev));
        }
    }
    for (u, text, gen, prev) in to_spawn {
        if let Some(h) = prev {
            h.abort();
        }
        let handle = spawn_indexer_task(
            docs.clone(),
            workspace_roots.clone(),
            u.clone(),
            text,
            gen,
            std::time::Duration::ZERO,
        );
        let mut docs_w = docs.write().await;
        if let Some(state) = docs_w.get_mut(&u) {
            if state.generation == gen {
                state.index_task = Some(handle);
            } else {
                handle.abort();
            }
        } else {
            handle.abort();
        }
    }
}

#[async_trait]
impl LanguageBackend for HtclBackend {
    fn language_id(&self) -> &str {
        "htcl"
    }

    fn handles(&self, uri: &Url) -> bool {
        uri.path().ends_with(".htcl")
    }

    async fn set_text(&self, uri: Url, text: String) {
        debug!(%uri, bytes = text.len(), "set_text");
        // Capture the previous indexer task (if any) so we can abort
        // it AFTER releasing the write lock — abort() itself is
        // cheap but keeping the lock held for it stalls other
        // handlers wanting to read the docs map.
        let (tx, generation, prev_task) = {
            let mut docs = self.docs.write().await;
            match docs.get_mut(&uri) {
                Some(state) => {
                    state.generation += 1;
                    state.text = text.clone();
                    // Serve-stale-while-rebuild: do NOT clear the
                    // previous analysis. Reads (`analysis_for`,
                    // via completion/hover/goto-def/references)
                    // will see the pre-keystroke snapshot
                    // immediately instead of waiting the ~7s a
                    // fresh `build_analysis` takes on a large
                    // workspace (the metroid tree hits ~7s in
                    // the validator alone). Diagnostics update
                    // one commit behind — an acceptable tradeoff
                    // for interactive latency. When the freshly
                    // spawned indexer commits, `send_replace(Some
                    // (new))` swaps the snapshot in-place with
                    // no observable gap.
                    let prev = state.index_task.take();
                    (state.tx.clone(), state.generation, prev)
                }
                None => {
                    let (tx, _rx) = watch::channel(None);
                    docs.insert(
                        uri.clone(),
                        DocState {
                            text: text.clone(),
                            generation: 1,
                            tx: tx.clone(),
                            index_task: None,
                        },
                    );
                    (tx, 1, None)
                }
            }
        };
        if let Some(handle) = prev_task {
            handle.abort();
        }

        let handle = self.spawn_indexer(
            uri.clone(),
            text,
            generation,
            std::time::Duration::from_millis(250),
        );

        // Store the handle so a subsequent set_text can abort us.
        let mut docs = self.docs.write().await;
        if let Some(state) = docs.get_mut(&uri) {
            if state.generation == generation {
                state.index_task = Some(handle);
            } else {
                // A newer set_text landed between our two lock
                // acquisitions. Kill our task, the newer set_text
                // has already installed its own.
                handle.abort();
            }
        } else {
            // Doc was closed while we were spawning. Kill our task.
            handle.abort();
        }
        let _ = tx; // silence unused warning when watching sends aren't used further
    }

    async fn set_workspace_roots(&self, roots: Vec<std::path::PathBuf>) {
        *self.workspace_roots.write().await = roots.clone();
        // Preload the same set of files `vw check` scans so
        // workspace-wide diagnostics cover the whole tree — not
        // just the import graph reachable from the docs the
        // user has explicitly opened. Without this the space-D
        // picker misses warnings in leaf files (e.g. `ip/gtm.htcl`,
        // `ip/dcmac.htcl`) until the user opens each one; with
        // it, opening ANY file in the workspace surfaces the
        // full-workspace picture on first analysis.
        //
        // Preload runs BEHIND `set_text`'s 250ms debounce (it
        // just enqueues indexers) so this call returns fast; the
        // per-file builds run on the indexer's spawn_blocking
        // thread pool in the background.
        self.preload_workspace_targets(&roots).await;
    }

    async fn save(&self, uri: &Url) {
        // Save is the user's explicit "I'm done for now" signal —
        // skip the debounce entirely so their edit is checked
        // immediately. Bump the generation so any in-flight
        // debounced indexer from the last `set_text` gets
        // superseded when this one commits. Text is whatever's
        // currently stored; Helix sends `did_change` before
        // `did_save` on save operations so the buffer's already
        // in sync.
        let (text, generation, prev_task) = {
            let mut docs = self.docs.write().await;
            let Some(state) = docs.get_mut(uri) else {
                return;
            };
            state.generation += 1;
            let prev = state.index_task.take();
            (state.text.clone(), state.generation, prev)
        };
        if let Some(handle) = prev_task {
            handle.abort();
        }
        let handle = self.spawn_indexer(
            uri.clone(),
            text,
            generation,
            std::time::Duration::ZERO,
        );
        let mut docs = self.docs.write().await;
        if let Some(state) = docs.get_mut(uri) {
            if state.generation == generation {
                state.index_task = Some(handle);
            } else {
                handle.abort();
            }
        } else {
            handle.abort();
        }
    }

    async fn wait_for_reindex(&self, uri: &Url) {
        // Subscribe to the doc's analysis-watch channel, then wait
        // for the NEXT commit — i.e. the indexer task's
        // `send_replace(Some(new))` at the end of `set_text`'s
        // spawned future. Used by the server to wrap the wait in
        // an LSP `workDoneProgress` notification so the editor's
        // "indexing…" spinner reflects the actual rebuild
        // duration, not just a fire-and-forget millisecond.
        //
        // `mark_unchanged` is the key call: `subscribe()` seeds the
        // receiver at the current sender value (which may be the
        // stale-serve analysis we've KEPT alive across `set_text`
        // — see the serve-stale comment there). Without
        // `mark_unchanged` the immediate `changed().await` would
        // return instantly on that already-seen value and the
        // spinner would flash for a millisecond instead of
        // spanning the whole rebuild.
        //
        // If the sender is dropped (doc closed), or the current
        // watch has never held a value (uri unknown), we return
        // immediately — no rebuild to wait on.
        let mut rx = {
            let docs = self.docs.read().await;
            match docs.get(uri) {
                Some(state) => state.tx.subscribe(),
                None => return,
            }
        };
        rx.mark_unchanged();
        let _ = rx.changed().await;
    }

    async fn close(&self, uri: &Url) {
        let prev_task = {
            let mut docs = self.docs.write().await;
            docs.remove(uri).and_then(|s| s.index_task)
        };
        if let Some(handle) = prev_task {
            handle.abort();
        }
    }

    async fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        // All diagnostics precomputed at index time. No work here.
        let Some(analysis) = self.analysis_for(uri).await else {
            return Vec::new();
        };
        analysis.diagnostics.clone()
    }

    async fn workspace_diagnostics(&self) -> Vec<(Url, Vec<Diagnostic>)> {
        // Walk every open document's committed analysis. Each
        // carries its own local diagnostics AND the retranslated
        // diagnostics from every file it transitively `src`s.
        //
        // Snapshot the URI list first so we can drop the docs
        // read lock before calling `analysis_for` on each (which
        // takes its own read).
        let uris: Vec<Url> = self.docs.read().await.keys().cloned().collect();
        // First-call gate: preloaded entry points can still be
        // building the FIRST time this runs (initialize → preload
        // spawns indexers, and the user opens a file before those
        // indexers commit). Without this wait, the fan-out
        // published from the user-open reindex reads a partial
        // `workspace_diagnostics` snapshot — files whose preload
        // hasn't committed yet contribute NOTHING, so the picker
        // shows an incomplete picture until the user happens to
        // open another file (which triggers a fresh fan-out
        // AFTER preloads have finished).
        //
        // The bounded wait is per-URI: subscribe to that doc's
        // analysis-watch channel and await the first non-`None`
        // value. Already-committed docs return instantly.
        // Preloads that fail to commit (indexer panic, disk read
        // error) never publish a `Some` — 5 s is a generous
        // ceiling that keeps the picker responsive even in that
        // pathological case (the failing URI is simply omitted).
        for uri in &uris {
            let rx = {
                let docs = self.docs.read().await;
                docs.get(uri).map(|s| s.tx.subscribe())
            };
            let Some(mut rx) = rx else { continue };
            if rx.borrow().is_some() {
                continue;
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                rx.wait_for(|v| v.is_some()),
            )
            .await;
        }
        let roots = self.workspace_roots_snapshot().await;
        // Open-doc set — used below to decide who owns a URI's
        // diagnostics. When a file is open, its own analysis was
        // built from the live buffer and is authoritative;
        // cross-file diagnostics from OTHER open files targeting
        // the same URI reflect whatever those files last saw on
        // disk, which is stale as soon as this file gets edited.
        // Ignoring them fixes the stuck-diagnostic bug where
        // fixing `cips.htcl` doesn't clear its markers until every
        // file that `src`s it also reindexes.
        let open_uris: std::collections::HashSet<Url> =
            uris.iter().cloned().collect();
        // Group by origin URI so a file `src`d by multiple open
        // docs doesn't get its diagnostics duplicated; when the
        // same file surfaces via more than one analysis, we keep
        // just the first non-empty set. In practice the analyses
        // agree because the validator is deterministic per input.
        let mut by_uri: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
        for uri in &uris {
            let Some(analysis) = self.analysis_for(uri).await else {
                continue;
            };
            // Every open document contributes an entry — even an
            // EMPTY one — so the editor clears stale errors when
            // the user fixes them. Without this, a file that
            // *used* to have errors keeps showing them until it's
            // reopened. `insert` (not `or_insert_with`) so a
            // freshly-committed analysis wins over a stale entry
            // some earlier iteration's cross-file merge left
            // behind.
            by_uri.insert(uri.clone(), analysis.diagnostics.clone());
            // Seed an empty entry for every imported file that
            // sits inside the workspace. This is what makes fixed
            // errors CLEAR: after the fix, `cross_file_diagnostics`
            // has no entry for that file, but the fan-out still
            // sees the URI (from `view.imports`) and publishes an
            // empty payload — the editor's cached "there were
            // errors here" state gets overwritten with "no
            // errors." Without this seed, cleared files wouldn't
            // reappear in the output map at all.
            for import in &analysis.view.imports {
                if !roots.is_empty()
                    && !uri_under_roots(&import.file_uri, &roots)
                {
                    continue;
                }
                by_uri.entry(import.file_uri.clone()).or_default();
            }
            for (u, d) in &analysis.cross_file_diagnostics {
                // Only surface diagnostics from files that sit
                // inside the editor's own workspace roots. Deps
                // (`~/.vw/deps`, the amd/ trees, etc.) get walked
                // by `build_view` for symbol resolution, but the
                // user isn't editing them from this workspace —
                // reporting a dep-side error in `space-D` is
                // noise. When no roots are set (e.g. the file was
                // opened standalone), fall through: nothing to
                // filter against.
                if !roots.is_empty() && !uri_under_roots(u, &roots) {
                    continue;
                }
                // Do NOT overlay cross-file diagnostics onto files
                // that have their own open analysis — that
                // analysis was just rebuilt from the live buffer
                // and supersedes whatever the srcing file's stale
                // analysis remembers. Skipping this was the entire
                // reason old errors stuck around in Helix after
                // the user fixed them: any parent doc's cached
                // analysis kept re-injecting the same diagnostic
                // on every workspace/diagnostic tick until that
                // parent reindexed.
                if open_uris.contains(u) {
                    continue;
                }
                by_uri.entry(u.clone()).or_default().push(d.clone());
            }
        }
        by_uri.into_iter().collect()
    }

    async fn document_symbols(&self, uri: &Url) -> Vec<DocumentSymbol> {
        let Some(analysis) = self.analysis_for(uri).await else {
            return Vec::new();
        };
        let parsed = &analysis.parsed_local;
        let line_index = &analysis.local_line_index;
        let mut symbols = Vec::new();
        for stmt in &parsed.document.stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            let CommandKind::Proc(proc) = &cmd.kind else {
                continue;
            };
            let name = proc.name.clone().unwrap_or_else(|| "<proc>".into());
            let (cmd_start, cmd_end) = line_index.range(cmd.span);
            let (name_start, name_end) = line_index.range(proc.name_span);
            let detail = if cmd.doc_comments.is_empty() {
                None
            } else {
                Some(cmd.doc_comments.join("\n"))
            };
            #[allow(deprecated)]
            symbols.push(DocumentSymbol {
                name,
                detail,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: Range {
                    start: lc_to_pos(cmd_start),
                    end: lc_to_pos(cmd_end),
                },
                selection_range: Range {
                    start: lc_to_pos(name_start),
                    end: lc_to_pos(name_end),
                },
                children: None,
            });
        }
        symbols
    }

    async fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        // Cap the response so a wide picker scroll doesn't pay for
        // thousands of entries when the user hasn't narrowed yet. The
        // editor applies its own scoring on top, so any reasonable
        // ceiling keeps the UX responsive.
        const MAX_RESULTS: usize = 500;

        let needle = query.to_ascii_lowercase();
        // Snapshot the list of open URIs — we release the docs
        // lock before calling `analysis_for` on each so an
        // in-flight indexer's write-lock acquisition doesn't
        // deadlock against our read lock.
        let uris: Vec<Url> = self.docs.read().await.keys().cloned().collect();
        // Files we've already harvested — dedupe so a header imported
        // by multiple open docs doesn't double up. Keyed on the URI as
        // a string for hashability.
        let mut seen_files: HashMap<String, ()> = HashMap::new();
        let mut out: Vec<SymbolInformation> = Vec::new();

        for uri in &uris {
            let Some(analysis) = self.analysis_for(uri).await else {
                continue;
            };
            // Visit the open doc itself first, then everything it
            // transitively `src`s. `build_view` already canonicalizes
            // paths during the walk, so the import file_uris are
            // stable across docs.
            if seen_files.insert(uri.to_string(), ()).is_none() {
                collect_workspace_symbols(
                    uri,
                    &analysis.local_text,
                    &needle,
                    &mut out,
                    MAX_RESULTS,
                );
                if out.len() >= MAX_RESULTS {
                    return out;
                }
            }

            for import in &analysis.view.imports {
                let key = import.file_uri.to_string();
                if seen_files.insert(key, ()).is_some() {
                    continue;
                }
                let text = &analysis.view.view_source
                    [import.start as usize..import.end as usize];
                collect_workspace_symbols(
                    &import.file_uri,
                    text,
                    &needle,
                    &mut out,
                    MAX_RESULTS,
                );
                if out.len() >= MAX_RESULTS {
                    return out;
                }
            }
        }
        out
    }

    async fn goto_definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<Location> {
        let Some(analysis) = self.analysis_for(uri).await else {
            return Vec::new();
        };
        let line_index = &analysis.local_line_index;
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });

        // Special case: cursor on a `src @dep/foo` path → jump to the
        // imported file. Resolved through the same `vw-lib` machinery
        // the CLI uses, so editor and CLI agree on the same target.
        let parsed_local = &analysis.parsed_local;
        if let Some(import) = src_import_at(&parsed_local.document, offset) {
            if let Some(raw) = import.path.as_deref() {
                let Ok(file_path) = uri.to_file_path() else {
                    return Vec::new();
                };
                if let Some(resolved) =
                    self.resolve_import(&file_path, raw).await
                {
                    if let Ok(target_uri) = Url::from_file_path(resolved) {
                        return vec![Location {
                            uri: target_uri,
                            range: Range::default(),
                        }];
                    }
                }
            }
            return Vec::new();
        }

        // General case: resolve against the workspace view so calls to
        // imported procs jump to the right file.
        let view = &analysis.view;
        let parsed_view = &analysis.parsed_view;
        let Some(target_span) =
            definition_at(&parsed_view.document, &view.view_source, offset)
        else {
            return Vec::new();
        };

        // Translate the target span back to its source file: local
        // file when in the original region, otherwise the imported
        // file whose appended region contains it.
        match view.locate(target_span.start) {
            None => {
                // Local hit — line_index is over analysis.local_text.
                let (start, end) = analysis.local_line_index.range(target_span);
                vec![Location {
                    uri: uri.clone(),
                    range: Range {
                        start: lc_to_pos(start),
                        end: lc_to_pos(end),
                    },
                }]
            }
            Some((region, _)) => {
                // Read the imported file's text so we can build a
                // file-local line index. (Already on disk; cheap.)
                let Ok(import_path) = region.file_uri.to_file_path() else {
                    return Vec::new();
                };
                let Ok(import_text) = std::fs::read_to_string(&import_path)
                else {
                    return Vec::new();
                };
                let import_index = LineIndex::new(&import_text);
                let local_start = target_span.start - region.start;
                let local_end = target_span.end - region.start;
                let (s, e) = import_index
                    .range(vw_htcl::Span::new(local_start, local_end));
                vec![Location {
                    uri: region.file_uri.clone(),
                    range: Range {
                        start: lc_to_pos(s),
                        end: lc_to_pos(e),
                    },
                }]
            }
        }
    }

    async fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        // Same strategy as completion: serve hover off the CURRENT
        // in-memory text so what the user's cursor is on maps to
        // real content. When a workspace analysis is available
        // (usually — 99% of hover requests fire between typing
        // bursts, when there IS a committed snapshot), we consult
        // it for cross-file lookups; when there isn't (fresh file,
        // still-building initial index), we degrade to local-only
        // — still useful for hovering over locally-defined procs.
        let current_text = self.current_text(uri).await.unwrap_or_default();
        let current_line_index = LineIndex::new(&current_text);
        let offset = current_line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        let current_parsed = vw_htcl::parse(&current_text);

        // Prefer the workspace snapshot's parsed_view (it contains
        // the imported proc definitions we want to hover through).
        // Fall back to the CURRENT local parse when nothing's
        // committed yet.
        let stale = self.analysis_for(uri).await;
        let (hover_doc, hover_source, hover_offset, doc_for_comments) =
            if let Some(a) = stale.as_ref() {
                // The offset was computed against CURRENT text. It
                // maps 1:1 into the workspace view AS LONG AS the
                // stale view's local prefix is a prefix of the
                // current text (typical: adds/dels midway through
                // the line only shift bytes past that point, but
                // the workspace view is only ~correct in the local
                // prefix anyway). For hover, the miscarriage is
                // harmless — worst case we hover on the wrong
                // token and return None.
                (
                    &a.parsed_view.document,
                    a.view.view_source.as_str(),
                    offset,
                    &a.parsed_view.document,
                )
            } else {
                (
                    &current_parsed.document,
                    current_text.as_str(),
                    offset,
                    &current_parsed.document,
                )
            };
        let target = hover_at(hover_doc, hover_source, hover_offset)?;
        // Prefer LOCAL line index for translating spans → line/col:
        // that's what Helix expects.
        let (start, end) = current_line_index.range(target.span());
        // The proc's own doc comments live on the surrounding Command,
        // not on its `Proc` payload — fetch them up here so the
        // formatters can stay focused on shape, not lookup plumbing.
        let proc_doc_comments = match &target {
            HoverTarget::ProcDef { proc, .. } => {
                proc_doc_comments_for(doc_for_comments, proc)
            }
            HoverTarget::CallSite { proc_name, .. } => {
                proc_doc_comments_by_name(doc_for_comments, proc_name)
            }
            _ => Vec::new(),
        };
        let markdown = format_hover(&target, &proc_doc_comments);
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(Range {
                start: lc_to_pos(start),
                end: lc_to_pos(end),
            }),
        })
    }

    async fn completion(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<CompletionItem> {
        // Serve completion off the CURRENT in-memory text (what the
        // user just typed), not the stale-cached workspace analysis's
        // local_text. Otherwise cmdline::analyze scans backward from
        // an offset in TEXT THAT DOESN'T CONTAIN WHAT WAS JUST TYPED
        // — the `partial` comes out blank or wrong, and completion
        // silently returns nothing. This is the "typed `-preset` and
        // got no enum values" symptom.
        //
        // Cross-file proc lookups (`gtwiz_versal::configure`, etc.)
        // still come from the stale workspace analysis via
        // `parsed_view` — those signatures don't change while the
        // user types locally, so stale is fine.
        let current_text = self.current_text(uri).await.unwrap_or_default();
        let current_line_index = LineIndex::new(&current_text);
        let offset = current_line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        // Suppress completion inside comments. Helix's LSP client
        // auto-triggers on typing, so writing prose inside a
        // `#`-line otherwise pops a dropdown on every space —
        // pure noise. The check: on the line up to the cursor,
        // the first non-whitespace char is `#`. That covers both
        // standalone `# text` and `## doc comment` lines, plus
        // mid-command `# comment` continuations (which in htcl
        // are still line-anchored — the comment starts at
        // column-0-after-ws, same shape). It doesn't catch `#`
        // inside strings / brackets — those don't start comments
        // in htcl anyway, so no false positives on real code.
        if in_line_comment(&current_text, offset) {
            return Vec::new();
        }
        let current_parsed = vw_htcl::parse(&current_text);

        // `src <partial>` is filesystem-aware, so it takes its own
        // path before we fall back to the htcl-level analyzer.
        let line = vw_htcl::cmdline::analyze(&current_text, offset);
        if crate::src_complete::is_src_path_context(&line) {
            if let Ok(entry_file) = uri.to_file_path() {
                let resolver = crate::workspace::build_resolver(&entry_file);
                return crate::src_complete::src_path_completions(
                    &entry_file,
                    &line,
                    &current_line_index,
                    &resolver,
                );
            }
        }

        // Grab the workspace analysis if it exists (stale or fresh).
        // If we've never had one commit, we complete against just
        // the local file — better than blocking indefinitely.
        let analysis = self.analysis_for(uri).await;
        let workspace_docs: Vec<&vw_htcl::Document> = analysis
            .as_ref()
            .map(|a| vec![&a.parsed_view.document])
            .unwrap_or_default();

        vw_htcl::complete_at_with_extras(
            &current_parsed.document,
            &current_text,
            offset,
            &workspace_docs,
        )
        .into_iter()
        .map(|c| completion_item(c, &current_line_index))
        .collect()
    }

    async fn signature_help(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<SignatureHelp> {
        let analysis = self.analysis_for(uri).await?;
        let offset = analysis.local_line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        // Workspace view so signatures of imported procs surface, and
        // so the cmdline scan can step into a `[ … ]` substitution
        // (the parser now carries a `body` inside `CmdSubst` and the
        // scan already treats `[` as a command boundary).
        let view = &analysis.view;
        let parsed = &analysis.parsed_view;
        let help =
            signature_help_at(&parsed.document, &view.view_source, offset)?;
        Some(signature_help_response(&help))
    }

    async fn rename(
        &self,
        uri: &Url,
        position: Position,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let (target, _decl_uri) =
            self.identify_target_at(uri, position).await?;
        // Build a per-URI edit list for every file the target
        // reaches. For file-local kinds (Local, ProcArg) this
        // resolves to just the current file; for cross-file
        // kinds it walks every `.htcl` file under the workspace
        // root.
        let per_file = self.collect_reference_spans(uri, &target).await;
        if per_file.is_empty() {
            return None;
        }
        let replacement = rename_replacement_for(&target, new_name)?;
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for (file_uri, text, spans) in per_file {
            let line_index = LineIndex::new(&text);
            let edits: Vec<TextEdit> = spans
                .into_iter()
                .map(|span| span_to_text_edit(span, &line_index, &replacement))
                .collect();
            if !edits.is_empty() {
                changes.insert(file_uri, edits);
            }
        }
        if changes.is_empty() {
            return None;
        }
        Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        })
    }

    async fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some((target, _)) = self.identify_target_at(uri, position).await
        else {
            return Vec::new();
        };
        let per_file = self.collect_reference_spans(uri, &target).await;
        let mut locations = Vec::new();
        for (file_uri, text, mut spans) in per_file {
            let line_index = LineIndex::new(&text);
            if !include_declaration {
                // Best-effort decl filter: the target's own decl
                // is contained inside the ref set (procs' name-
                // span, types' name-span, enum-variant name-
                // spans). The reference finder returns them all;
                // remove those that lie inside the current
                // target's OWN declaration span when the target
                // came from this file. For cross-file callers
                // there's no ambiguity — the decl is only in the
                // decl file.
                spans.retain(|s| {
                    !span_looks_like_decl(&target, *s, &file_uri, uri)
                });
            }
            for span in spans {
                let (start, end) = line_index.range(span);
                locations.push(Location {
                    uri: file_uri.clone(),
                    range: Range {
                        start: lc_to_pos(start),
                        end: lc_to_pos(end),
                    },
                });
            }
        }
        locations
    }
}

impl HtclBackend {
    /// Identify the reference target at `position` in `uri`. Also
    /// returns the URI that owned the identification so the
    /// per-file collector knows which document to treat as the
    /// origin (matters for the file-local Local/ProcArg kinds).
    async fn identify_target_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(ReferenceTarget, Url)> {
        let analysis = self.analysis_for(uri).await?;
        let line_index = &analysis.local_line_index;
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        let parsed = &analysis.parsed_local;
        let target =
            identify_at(&parsed.document, &analysis.local_text, offset)?;
        Some((target, uri.clone()))
    }

    /// For each file the target reaches, return `(uri, text,
    /// spans)`. File-local targets stay in the origin file; cross-
    /// file targets get every `.htcl` file under the workspace
    /// root read and scanned.
    async fn collect_reference_spans(
        &self,
        origin: &Url,
        target: &ReferenceTarget,
    ) -> Vec<(Url, String, Vec<Span>)> {
        match target {
            ReferenceTarget::Local { .. } | ReferenceTarget::ProcArg { .. } => {
                let Some(analysis) = self.analysis_for(origin).await else {
                    return Vec::new();
                };
                let spans = find_references_in(
                    &analysis.parsed_local.document,
                    &analysis.local_text,
                    target,
                );
                if spans.is_empty() {
                    Vec::new()
                } else {
                    vec![(origin.clone(), analysis.local_text.clone(), spans)]
                }
            }
            ReferenceTarget::Proc { .. }
            | ReferenceTarget::Type { .. }
            | ReferenceTarget::EnumVariant { .. } => {
                let mut files = self.workspace_htcl_files(origin).await;
                // Fallback: no workspace root (test URIs, files
                // opened outside a `vw.toml` tree, etc.) → operate
                // on the origin file only. The rename still
                // works locally; users adopting the LSP outside a
                // workspace get local-only semantics until they
                // set up a `vw.toml`.
                if files.is_empty() {
                    if let Some(analysis) = self.analysis_for(origin).await {
                        files.push((
                            origin.clone(),
                            analysis.local_text.clone(),
                        ));
                    }
                }
                let mut out = Vec::new();
                for (file_uri, text) in files {
                    let parsed = parse(&text);
                    let spans =
                        find_references_in(&parsed.document, &text, target);
                    if !spans.is_empty() {
                        out.push((file_uri, text, spans));
                    }
                }
                out
            }
        }
    }

    /// Enumerate every `.htcl` file under the workspace root that
    /// contains `origin`. Reads their current on-disk content —
    /// for files also open in the editor this may be one tick
    /// behind, but that's the cost of not requiring the editor to
    /// pre-open every workspace file. Skips typical non-workspace
    /// directories (`target/`, `.git/`, `.vw/`).
    ///
    /// The origin file itself is served from the in-memory
    /// analysis so unsaved edits round-trip through the rename.
    async fn workspace_htcl_files(&self, origin: &Url) -> Vec<(Url, String)> {
        let Ok(origin_path) = origin.to_file_path() else {
            return Vec::new();
        };
        let Some(origin_utf8) = camino::Utf8Path::from_path(&origin_path)
        else {
            return Vec::new();
        };
        let Some(root) = crate::workspace::workspace_root(origin_utf8) else {
            return Vec::new();
        };
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        walk_htcl_files(root.as_std_path(), &mut paths);
        let mut visited: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut out: Vec<(Url, String)> = Vec::new();
        for path in paths {
            let canonical =
                path.canonicalize().unwrap_or_else(|_| path.clone());
            if !visited.insert(canonical.clone()) {
                continue;
            }
            let file_uri = match Url::from_file_path(&canonical) {
                Ok(u) => u,
                Err(_) => continue,
            };
            // For the origin file, prefer the in-memory analysis
            // text so unsaved edits round-trip.
            let text = if same_file(&file_uri, origin) {
                if let Some(analysis) = self.analysis_for(&file_uri).await {
                    analysis.local_text.clone()
                } else {
                    std::fs::read_to_string(&canonical).unwrap_or_default()
                }
            } else {
                std::fs::read_to_string(&canonical).unwrap_or_default()
            };
            if text.is_empty() {
                continue;
            }
            out.push((file_uri, text));
        }
        out
    }
}

/// Recursively walk `dir` collecting `.htcl` files. Skips `target/`,
/// `.git/`, `.vw/`, `node_modules/` at any depth. Silently swallows
/// I/O errors on individual directories — a permission-denied
/// subtree just contributes nothing to the results.
/// True when `uri`'s filesystem path lies under any of `roots`.
/// Non-file URIs and paths that don't resolve into any root fall
/// through as `false` — the caller's default behavior is "not in
/// the workspace, don't fan out." Both sides get canonicalized so
/// a symlinked workspace root matches a real-path URI (`Path::
/// starts_with` is purely lexical). Canonicalization failures
/// (missing files, permission errors) fall back to the lexical
/// compare, which still catches the common case.
/// Same helper as vw-cli's — split blessed vs. banned family lists
/// so a dep whose `[targets]` only carries a `not-supported` list
/// doesn't get misreported as "declared families: versal" when
/// the versal families there are BANNED, not blessed.
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
            format!("blessed families: {}", m.supported_families.join(", "))
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

fn uri_under_roots(uri: &Url, roots: &[std::path::PathBuf]) -> bool {
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    let canonical_path = path.canonicalize().unwrap_or(path);
    roots.iter().any(|r| {
        let canonical_root = r.canonicalize().unwrap_or_else(|_| r.clone());
        canonical_path.starts_with(&canonical_root)
    })
}

/// True when two file URIs name the same file on disk.
///
/// Import URIs are built from resolver output, which canonicalizes;
/// document URIs come from the editor, which does not — Helix hands
/// back whatever path the user opened. Any path crossing a symlink
/// (`$TMPDIR` on macOS, a checkout under a symlinked home) gives the
/// two sides different spellings of one file, and a plain `==` then
/// answers "different file": the fan-out reindex stops firing, so an
/// edit to an imported file never reaches the open importer. Compare
/// canonical forms, falling back to the raw path when
/// canonicalization fails (deleted file, permission error).
fn same_file(a: &Url, b: &Url) -> bool {
    if a == b {
        return true;
    }
    let (Ok(pa), Ok(pb)) = (a.to_file_path(), b.to_file_path()) else {
        return false;
    };
    // Cheap prune before touching the filesystem: two spellings of
    // one file always agree on the final component. The fan-out
    // scan runs this against every import of every open doc — a
    // vivado-cmd tree is ~900 of them — and nearly all differ right
    // here, so this keeps the syscalls to the handful that could
    // plausibly match.
    if pa.file_name() != pb.file_name() {
        return false;
    }
    pa.canonicalize().unwrap_or(pa) == pb.canonicalize().unwrap_or(pb)
}

fn walk_htcl_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "target" | ".git" | ".vw" | "node_modules") {
                continue;
            }
            walk_htcl_files(&path, out);
        } else if file_type.is_file()
            && path.extension().and_then(|s| s.to_str()) == Some("htcl")
        {
            out.push(path);
        }
    }
}

/// Pick the exact text to substitute at each rename span for a
/// given target. Preserves namespace prefixes when the user typed
/// a bare replacement name.
fn rename_replacement_for(
    target: &ReferenceTarget,
    new_name: &str,
) -> Option<String> {
    if new_name.is_empty() {
        return None;
    }
    // Validate: bare identifier or `ns::segment(::segment)*`.
    for seg in new_name.split("::") {
        if seg.is_empty() {
            return None;
        }
        let mut bytes = seg.bytes();
        let first = bytes.next().unwrap();
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
    }
    Some(match target {
        ReferenceTarget::Proc { name }
            if name.contains("::") && !new_name.contains("::") =>
        {
            let ns = name.rsplit_once("::").map(|(n, _)| n).unwrap_or("");
            format!("{ns}::{new_name}")
        }
        ReferenceTarget::EnumVariant { enum_name, .. }
            if !new_name.contains("::") =>
        {
            format!("{enum_name}::{new_name}")
        }
        _ => new_name.to_string(),
    })
}

/// Map a source `Span` + replacement text to an LSP `TextEdit`.
fn span_to_text_edit(
    span: Span,
    line_index: &LineIndex,
    new_text: &str,
) -> TextEdit {
    let (start, end) = line_index.range(span);
    TextEdit {
        range: Range {
            start: lc_to_pos(start),
            end: lc_to_pos(end),
        },
        new_text: new_text.to_string(),
    }
}

/// Best-effort filter for the `!include_declaration` case. Skips
/// spans that plausibly correspond to a decl site by name-matching
/// the target's shape. This is imperfect (a proc named `X` and a
/// call `X` are indistinguishable at the span level), but the LSP
/// clients that pass `include_declaration=false` usually just want
/// to hide the decl in the results — an occasional inclusion is
/// benign.
fn span_looks_like_decl(
    _target: &ReferenceTarget,
    _span: Span,
    _file_uri: &Url,
    _origin_uri: &Url,
) -> bool {
    // Placeholder — the LSP protocol says clients CAN filter locally,
    // and most do. Returning false means we always include; safer
    // than accidentally dropping too much.
    false
}

// (`rename_edit_to_lsp` removed — the rename handler now emits
// `TextEdit`s directly via `span_to_text_edit` on the raw
// reference spans, so the intermediate `RenameEdit` type isn't
// crossed over anymore.)

// --- completion / signature-help formatters -------------------------------

fn completion_item(c: Completion, line_index: &LineIndex) -> CompletionItem {
    let kind = match c.kind {
        CompletionKind::Proc => CompletionItemKind::FUNCTION,
        CompletionKind::Flag => CompletionItemKind::FIELD,
        CompletionKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
        CompletionKind::Constructor => CompletionItemKind::CONSTRUCTOR,
    };
    let (start, end) = line_index.range(c.replace);
    let insert = c.insert_text.clone().unwrap_or_else(|| c.label.clone());
    let text_edit = TextEdit {
        range: Range {
            start: lc_to_pos(start),
            end: lc_to_pos(end),
        },
        new_text: insert,
    };
    let insert_text_format = if c.snippet {
        InsertTextFormat::SNIPPET
    } else {
        InsertTextFormat::PLAIN_TEXT
    };
    CompletionItem {
        label: c.label,
        kind: Some(kind),
        detail: c.detail,
        documentation: c.documentation.map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        }),
        insert_text_format: Some(insert_text_format),
        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(
            text_edit,
        )),
        ..Default::default()
    }
}

fn signature_help_response(help: &vw_htcl::SignatureHelp<'_>) -> SignatureHelp {
    // Build the rendered signature label and, in lockstep, the
    // [start, end) offsets each parameter occupies within it so the
    // editor highlights the active one. Names are identifiers, so
    // UTF-16 and char counts coincide.
    let mut label = help.proc_name.clone();
    let mut parameters = Vec::with_capacity(help.signature.args.len());
    for arg in &help.signature.args {
        label.push(' ');
        let start = label.chars().count() as u32;
        label.push('-');
        label.push_str(&arg.name);
        if let Some(ty) = arg.type_annotation.as_ref() {
            label.push_str(": ");
            label.push_str(&render_type(ty));
        }
        let end = label.chars().count() as u32;
        parameters.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([start, end]),
            documentation: vw_htcl::doc::brief(&arg.doc_comments)
                .map(Documentation::String),
        });
    }
    // Append the return type to the signature label when present.
    // Renders as `proc-name -arg1 -arg2 → bd_cell`.
    if let Some(ty) = help.signature.return_type.as_ref() {
        label.push_str(" → ");
        label.push_str(&render_type(ty));
    }

    let reflowed = vw_htcl::doc::reflow_doc_comments(help.doc_comments);
    let documentation = (!reflowed.is_empty()).then_some({
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: reflowed,
        })
    });

    #[allow(deprecated)] // `active_parameter` field on SignatureInformation
    let info = SignatureInformation {
        label,
        documentation,
        parameters: Some(parameters),
        active_parameter: help.active_parameter,
    };

    SignatureHelp {
        signatures: vec![info],
        active_signature: Some(0),
        active_parameter: help.active_parameter,
    }
}

// --- src import lookup ----------------------------------------------------

/// If the cursor at `offset` is on the path word of a `src <path>`
/// statement, return that import. Used by `goto_definition` to jump
/// to the imported module.
fn src_import_at(
    document: &vw_htcl::Document,
    offset: u32,
) -> Option<&vw_htcl::SrcImport> {
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Src(import) = &cmd.kind else {
            continue;
        };
        if import.path_span.contains(offset) {
            return Some(import);
        }
    }
    None
}

// --- doc-comment lookup ---------------------------------------------------

fn proc_doc_comments_for(
    document: &vw_htcl::Document,
    proc: &vw_htcl::Proc,
) -> Vec<String> {
    proc_doc_comments_for_in(&document.stmts, proc).unwrap_or_default()
}

fn proc_doc_comments_for_in(
    stmts: &[Stmt],
    proc: &vw_htcl::Proc,
) -> Option<Vec<String>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(p)
                // Pointer-identity match: `proc` was looked up out
                // of this same parse, so its address inside the AST
                // is unique.
                if std::ptr::eq(p, proc) => {
                    return Some(cmd.doc_comments.clone());
                }
            CommandKind::NamespaceEval(ns) => {
                if let Some(found) = proc_doc_comments_for_in(&ns.body, proc) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn proc_doc_comments_by_name(
    document: &vw_htcl::Document,
    name: &str,
) -> Vec<String> {
    proc_doc_comments_by_name_in(&document.stmts, "", name).unwrap_or_default()
}

fn proc_doc_comments_by_name_in(
    stmts: &[Stmt],
    prefix: &str,
    name: &str,
) -> Option<Vec<String>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(p) => {
                let Some(decl_name) = p.name.as_deref() else {
                    continue;
                };
                let qualified = if prefix.is_empty() {
                    decl_name.to_string()
                } else {
                    format!("{prefix}::{decl_name}")
                };
                if qualified == name {
                    return Some(cmd.doc_comments.clone());
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(ns_name) = ns.name.as_deref() else {
                    continue;
                };
                let nested = if prefix.is_empty() {
                    ns_name.to_string()
                } else {
                    format!("{prefix}::{ns_name}")
                };
                if let Some(found) =
                    proc_doc_comments_by_name_in(&ns.body, &nested, name)
                {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// Render a type expression in the canonical user-facing form —
/// `dict<string,bd_cell>`, `list<int>`, etc. Used by hover and
/// signature-help so the displayed type matches what the user
/// would write in source.
fn render_type(ty: &vw_htcl::TypeExpr) -> String {
    match ty {
        vw_htcl::TypeExpr::Named { name, .. } => name.clone(),
        vw_htcl::TypeExpr::Generic { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{name}<{}>", inner.join(","))
        }
        vw_htcl::TypeExpr::Qualified {
            namespace, variant, ..
        } => {
            format!("{namespace}::{variant}")
        }
    }
}

// --- markdown formatters --------------------------------------------------

fn format_hover(target: &HoverTarget, proc_doc_comments: &[String]) -> String {
    match target {
        HoverTarget::ProcDef { proc, .. } => format_proc(
            proc.name.as_deref().unwrap_or("<proc>"),
            proc.signature.as_ref(),
            proc_doc_comments,
        ),
        HoverTarget::CallSite {
            proc_name,
            signature,
            ..
        } => format_proc(proc_name, Some(signature), proc_doc_comments),
        HoverTarget::ProcArgDef { arg, .. }
        | HoverTarget::CallArg { arg, .. } => format_arg(arg),
        HoverTarget::LocalVar { name, ty, .. } => {
            format_local_var(name, ty.as_ref())
        }
        HoverTarget::EnumDef { decl, .. } => format_enum(decl),
        HoverTarget::TypeDef { decl, .. } => format_type_def(decl),
    }
}

fn format_type_def(decl: &vw_htcl::TypeDecl) -> String {
    let name = decl.name.as_deref().unwrap_or("<type>");
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    match decl.underlying.as_ref() {
        Some(ty) => writeln!(out, "type {name} = {}", render_type(ty)).unwrap(),
        None => writeln!(out, "type {name} = <unresolved>").unwrap(),
    }
    writeln!(out, "```").unwrap();
    out
}

fn format_enum(decl: &vw_htcl::EnumDecl) -> String {
    let mut out = String::new();
    let name = decl.name.as_deref().unwrap_or("<enum>");
    writeln!(out, "```htcl").unwrap();
    writeln!(out, "enum {name} = {{").unwrap();
    for v in &decl.variants {
        match v.payload.as_ref() {
            Some(p) => {
                writeln!(out, "  {}: {}", v.name, render_type(p)).unwrap()
            }
            None => writeln!(out, "  {}", v.name).unwrap(),
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out, "```").unwrap();
    out.push_str("\nTagged sum type. The compiler auto-generates ");
    out.push_str("constructors (`<Enum>::<Variant>`), repr, and ");
    out.push_str("`tag`/`payload` accessors. See ");
    out.push_str("docs/htcl-enums.md for the full semantics.\n");
    out
}

fn format_local_var(name: &str, ty: Option<&vw_htcl::TypeExpr>) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    match ty {
        Some(t) => writeln!(out, "${name}: {}", render_type(t)).unwrap(),
        None => writeln!(out, "${name}").unwrap(),
    }
    writeln!(out, "```").unwrap();
    out.push_str("\nLocal variable.\n");
    out
}

fn format_proc(
    name: &str,
    signature: Option<&ProcSignature>,
    proc_doc_comments: &[String],
) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    // Include the return type in the proc header when annotated:
    //   proc foo → string
    // Unannotated procs render unchanged (`proc foo`).
    let return_ty = signature.and_then(|s| s.return_type.as_ref());
    match return_ty {
        Some(ty) => {
            writeln!(out, "proc {name} → {}", render_type(ty)).unwrap();
        }
        None => {
            writeln!(out, "proc {name}").unwrap();
        }
    }
    writeln!(out, "```").unwrap();
    let reflowed = vw_htcl::doc::reflow_doc_comments(proc_doc_comments);
    if !reflowed.is_empty() {
        out.push('\n');
        out.push_str(&reflowed);
        out.push('\n');
    }
    if let Some(sig) = signature {
        if !sig.args.is_empty() {
            out.push_str("\n### Parameters\n\n");
            for arg in &sig.args {
                match arg.type_annotation.as_ref() {
                    Some(ty) => {
                        write!(out, "- `-{}: {}`", arg.name, render_type(ty))
                            .unwrap();
                    }
                    None => {
                        write!(out, "- `-{}`", arg.name).unwrap();
                    }
                }
                let reflowed =
                    vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
                let mut paragraphs = reflowed.split("\n\n");
                if let Some(brief) = paragraphs.next().filter(|s| !s.is_empty())
                {
                    write!(out, " — {brief}").unwrap();
                }
                out.push('\n');
                for extra in paragraphs.filter(|s| !s.is_empty()) {
                    writeln!(out, "  {extra}").unwrap();
                }
                for attr in &arg.attributes {
                    writeln!(out, "  - `{}`", format_attribute(attr)).unwrap();
                }
            }
        }
    }
    out
}

fn format_arg(arg: &ProcArg) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    match arg.type_annotation.as_ref() {
        Some(ty) => {
            writeln!(out, "-{}: {}", arg.name, render_type(ty)).unwrap()
        }
        None => writeln!(out, "-{}", arg.name).unwrap(),
    }
    writeln!(out, "```").unwrap();
    let reflowed = vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
    if !reflowed.is_empty() {
        out.push('\n');
        out.push_str(&reflowed);
        out.push('\n');
    }
    if !arg.attributes.is_empty() {
        out.push('\n');
        for attr in &arg.attributes {
            writeln!(out, "- `{}`", format_attribute(attr)).unwrap();
        }
    }
    out
}

fn format_attribute(attr: &Attribute) -> String {
    if attr.values.is_empty() {
        format!("@{}", attr.name)
    } else {
        let values: Vec<String> =
            attr.values.iter().map(format_attribute_value).collect();
        format!("@{}({})", attr.name, values.join(", "))
    }
}

fn format_attribute_value(v: &AttributeValue) -> String {
    v.to_tcl_literal()
}

fn lc_to_pos(lc: LineCol) -> Position {
    Position {
        line: lc.line,
        character: lc.character,
    }
}

/// True when `offset` lies on a line whose first non-whitespace
/// byte before the cursor is `#`. That's the shape of every htcl
/// comment — standalone `# text`, `## doc comment`, and mid-command
/// `# inline comment` continuations all start `<ws>#…` at column 0
/// of their line. Used by the completion handler to suppress its
/// dropdown while the user is writing prose in a comment.
///
/// Bytes only (ASCII-fast); no UTF-8 walking. `\n` bounds the
/// backward scan so we never look past the current line. `#` inside
/// a string / bracket on a preceding line can't reach here because
/// the scan stops at the enclosing `\n` first.
fn in_line_comment(source: &str, offset: u32) -> bool {
    let bytes = source.as_bytes();
    let end = (offset as usize).min(bytes.len());
    let line_start = bytes[..end]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    for &b in &bytes[line_start..end] {
        match b {
            b' ' | b'\t' | b'\r' => continue,
            b'#' => return true,
            _ => return false,
        }
    }
    false
}

/// Parse one htcl `text` and push every `proc` / `type` / `enum`
/// declaration whose name contains `needle` (case-insensitive, empty
/// `needle` matches all) into `out`. Stops as soon as `out` reaches
/// `cap` entries so a `workspace/symbol` request never assembles an
/// unbounded response. Variants of an enum are emitted as siblings
/// with `container_name` set to the enum, matching how
/// rust-analyzer surfaces variants in the workspace picker.
fn collect_workspace_symbols(
    uri: &Url,
    text: &str,
    needle: &str,
    out: &mut Vec<SymbolInformation>,
    cap: usize,
) {
    let parsed = parse(text);
    let line_index = LineIndex::new(text);
    for stmt in &parsed.document.stmts {
        if out.len() >= cap {
            return;
        }
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(name) = proc.name.as_deref() {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        proc.name_span,
                        SymbolKind::FUNCTION,
                        None,
                        needle,
                        out,
                    );
                }
            }
            CommandKind::TypeDecl(td) => {
                if let Some(name) = td.name.as_deref() {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        td.name_span,
                        SymbolKind::STRUCT,
                        None,
                        needle,
                        out,
                    );
                }
            }
            CommandKind::EnumDecl(ed) => {
                let enum_name = ed.name.as_deref();
                if let Some(name) = enum_name {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        ed.name_span,
                        SymbolKind::ENUM,
                        None,
                        needle,
                        out,
                    );
                }
                for v in &ed.variants {
                    if out.len() >= cap {
                        return;
                    }
                    push_symbol(
                        uri,
                        &line_index,
                        &v.name,
                        v.name_span,
                        SymbolKind::ENUM_MEMBER,
                        enum_name.map(str::to_string),
                        needle,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_symbol(
    uri: &Url,
    line_index: &LineIndex,
    name: &str,
    span: vw_htcl::Span,
    kind: SymbolKind,
    container_name: Option<String>,
    needle: &str,
    out: &mut Vec<SymbolInformation>,
) {
    if !needle.is_empty() && !name.to_ascii_lowercase().contains(needle) {
        return;
    }
    let (start, end) = line_index.range(span);
    #[allow(deprecated)]
    out.push(SymbolInformation {
        name: name.to_string(),
        kind,
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: Range {
                start: lc_to_pos(start),
                end: lc_to_pos(end),
            },
        },
        container_name,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir plus its **canonicalized** path.
    ///
    /// Always derive test paths from the returned `PathBuf`, never
    /// from `TempDir::path()`. On macOS `$TMPDIR` is
    /// `/var/folders/…`, a symlink to `/private/var/folders/…`, so
    /// the two spell the same file differently. The htcl resolver
    /// canonicalizes every path it resolves
    /// ([`vw_htcl::src_path`], [`vw_htcl::loader`]), so analyses
    /// commit — and goto-definition answers — under the
    /// `/private/…` form. A test that builds its expectation from
    /// the raw `TempDir` path is then comparing two spellings of
    /// one file, and either asserts a URI mismatch or waits
    /// forever for an analysis filed under the other spelling.
    /// Canonicalizing here puts the test on the same footing as
    /// the resolver. (Linux `/tmp` isn't symlinked, which is why
    /// this only ever bites locally on a Mac.)
    fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    fn uri() -> Url {
        Url::parse("file:///tmp/x.htcl").unwrap()
    }

    /// The smallest thing `find_workspace_dir` will stop at.
    const MANIFEST: &str =
        "[workspace]\nname = \"test\"\nversion = \"0.1.0\"\n";

    /// A workspace of its own, holding the one document a test is about.
    ///
    /// The cross-file targets — procs, types, enum variants — do not answer
    /// from the open document alone. They walk up from it for a `vw.toml` and
    /// then scan every `.htcl` underneath, so a test pointing at a fixed path
    /// like `/tmp` is at the mercy of whatever else is on the machine: one
    /// stray `vw.toml` above it and one stray `.htcl` below are enough
    /// between them to make the document under test drop out of its own
    /// rename, and the test fails somewhere with nothing wrong with it.
    /// Carrying its own manifest stops that search inside this directory, so
    /// nothing above it can reach the test.
    ///
    /// Canonicalized because macOS hands out `/var/…` temp directories that
    /// are really `/private/var/…`, and the walk canonicalizes every path it
    /// finds. An origin spelled the other way would not match the file the
    /// walk turned up — the same failure again, by a shorter route.
    ///
    /// The document is written to disk as well as handed to the backend: it
    /// has to be there for the walk to find it at all, which is the path
    /// these tests exist to cover.
    struct Workspace {
        _dir: tempfile::TempDir,
        uri: Url,
    }

    impl Workspace {
        fn holding(source: &str) -> Workspace {
            let dir = tempfile::TempDir::new().expect("scratch directory");
            let root = dir
                .path()
                .canonicalize()
                .expect("canonical scratch directory");
            std::fs::write(root.join("vw.toml"), MANIFEST)
                .expect("write a manifest");
            let document = root.join("x.htcl");
            std::fs::write(&document, source).expect("write the document");
            let uri =
                Url::from_file_path(&document).expect("a file url for it");
            Workspace { _dir: dir, uri }
        }

        fn uri(&self) -> Url {
            self.uri.clone()
        }
    }

    #[tokio::test]
    async fn handles_htcl_extension() {
        let backend = HtclBackend::new();
        assert!(backend.handles(&uri()));
        assert!(!backend.handles(&Url::parse("file:///tmp/x.vhd").unwrap()));
    }

    #[tokio::test]
    async fn diagnostics_for_unterminated_string() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(uri(), "puts \"oops\nputs ok\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diags[0].message.contains("unterminated string"));
    }

    #[tokio::test]
    async fn document_symbols_include_proc() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(
                uri(),
                "## greet someone\nproc greet {name} { puts hi }\n".into(),
            )
            .await;
        let symbols = backend.document_symbols(&uri()).await;
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[0].detail.as_deref(), Some("greet someone"));
    }

    #[tokio::test]
    async fn workspace_symbols_surface_procs_types_and_enum_variants() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(
                uri(),
                "proc greet {name} { puts hi }\n\
                 type Foo = int\n\
                 enum Color = {\n  Red\n  Green\n  Blue\n}\n"
                    .into(),
            )
            .await;
        let all = backend.workspace_symbols("").await;
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "{names:?}");
        assert!(names.contains(&"Foo"), "{names:?}");
        assert!(names.contains(&"Color"), "{names:?}");
        assert!(names.contains(&"Red"), "{names:?}");
        let red = all.iter().find(|s| s.name == "Red").unwrap();
        assert_eq!(red.kind, SymbolKind::ENUM_MEMBER);
        assert_eq!(red.container_name.as_deref(), Some("Color"));

        // Substring filter, case-insensitive.
        let filtered = backend.workspace_symbols("gre").await;
        assert!(filtered.iter().any(|s| s.name == "greet"));
        assert!(filtered.iter().any(|s| s.name == "Green"));
        assert!(!filtered.iter().any(|s| s.name == "Foo"));
    }

    #[tokio::test]
    async fn validator_diagnostics_surface_in_lsp() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(
                uri(),
                "proc axis {\n  @enum(1, 2, 4) width\n} { puts $width }\n\
                 axis -width 3\n"
                    .into(),
            )
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(
            diags.iter().any(|d| d.message.contains("@enum")),
            "{:?}",
            diags
        );
    }

    /// Unused-variable warnings from the `vw-htcl::unused` pass
    /// reach LSP clients with `DiagnosticSeverity::WARNING` and
    /// point at the offending decl. Underscore-prefixed names are
    /// exempt.
    #[tokio::test]
    async fn unused_var_warning_surfaces_in_lsp() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(uri(), "proc f {unused_arg} { return 1 }\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
            .filter(|d| d.message.contains("unused proc arg"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", diags);
        assert!(
            warnings[0].message.contains("unused_arg"),
            "{:?}",
            warnings[0]
        );
    }

    #[tokio::test]
    async fn unused_var_underscore_prefix_suppresses_lsp_warning() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(uri(), "proc f {_ignored} { return 1 }\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(
            !diags.iter().any(|d| d.message.contains("unused")),
            "{:?}",
            diags
        );
    }

    /// Rename produces a WorkspaceEdit whose TextEdits, when
    /// applied in reverse order, transform the source correctly.
    /// Covers the end-to-end LSP path: cursor → offset → rename_at →
    /// edits → LSP `WorkspaceEdit`.
    #[tokio::test]
    async fn rename_local_via_lsp() {
        let backend = HtclBackend::new();
        // `mode` is a local; renaming it should update the decl and
        // the two `$mode` refs.
        let src = "\
proc f {} {
  set mode fast
  puts $mode
  return $mode
}
";
        let ws = Workspace::holding(src);
        backend.set_text_sync(ws.uri(), src.into()).await;
        // Cursor on the `m` of `set mode` (line 1, column 6). 0-indexed.
        let workspace_edit = backend
            .rename(
                &ws.uri(),
                Position {
                    line: 1,
                    character: 6,
                },
                "kind",
            )
            .await
            .expect("rename should succeed");
        let changes = workspace_edit.changes.expect("expected changes");
        let text_edits = changes.get(&ws.uri()).expect("edits for this file");
        assert_eq!(text_edits.len(), 3, "{text_edits:?}");
        // Apply edits from tail to head to preserve earlier offsets.
        let mut renamed = src.to_string();
        let mut edits = text_edits.clone();
        edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
        for edit in edits.iter().rev() {
            let start = position_to_offset(&renamed, edit.range.start);
            let end = position_to_offset(&renamed, edit.range.end);
            renamed.replace_range(start..end, &edit.new_text);
        }
        assert!(renamed.contains("set kind fast"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
        assert!(renamed.contains("return $kind"), "{renamed}");
        assert!(!renamed.contains("mode"), "{renamed}");
    }

    /// Proc-name rename now works within the local file — the
    /// declaration span and every call site in the same document
    /// get rewritten. Cross-file callers (in other `.htcl` files
    /// the LSP hasn't yet been asked about) still need the
    /// workspace-scan variant.
    #[tokio::test]
    async fn rename_proc_name_via_lsp_covers_decl_and_call() {
        let backend = HtclBackend::new();
        let source = "proc greet {} { puts hi }\ngreet\n";
        let workspace = Workspace::holding(source);
        backend.set_text_sync(workspace.uri(), source.into()).await;
        // Cursor on the `g` of the proc's own name.
        let result = backend
            .rename(
                &workspace.uri(),
                Position {
                    line: 0,
                    character: 5,
                },
                "hello",
            )
            .await;
        let ws = result.expect("expected an edit set");
        let changes = ws.changes.expect("expected changes map");
        let edits = changes
            .get(&workspace.uri())
            .expect("edits for the local uri");
        assert_eq!(edits.len(), 2, "decl + one call site");
    }

    #[tokio::test]
    async fn references_returns_all_local_call_sites() {
        let backend = HtclBackend::new();
        let source =
            "proc greet {} { puts hi }\ngreet\nproc other {} { greet }\n";
        let ws = Workspace::holding(source);
        backend.set_text_sync(ws.uri(), source.into()).await;
        let locs = backend
            .references(
                &ws.uri(),
                Position {
                    line: 0,
                    character: 5,
                },
                true,
            )
            .await;
        // 3 hits: decl name + top-level call + nested call in `other`.
        assert_eq!(locs.len(), 3, "{locs:?}");
        for loc in &locs {
            assert_eq!(loc.uri, ws.uri());
        }
    }

    #[tokio::test]
    async fn references_on_type_covers_annotations() {
        let backend = HtclBackend::new();
        let source = "type MyThing = string\nproc a {v: MyThing} MyThing { }\nproc b {v: MyThing} { }\n";
        let ws = Workspace::holding(source);
        backend.set_text_sync(ws.uri(), source.into()).await;
        // Cursor on `MyThing` at the type decl (char 5..12 = "MyThing").
        let locs = backend
            .references(
                &ws.uri(),
                Position {
                    line: 0,
                    character: 5,
                },
                true,
            )
            .await;
        // decl + a's arg-type + a's return-type + b's arg-type = 4.
        assert_eq!(locs.len(), 4, "{locs:?}");
    }

    #[tokio::test]
    async fn rename_type_covers_all_annotations() {
        let backend = HtclBackend::new();
        let source = "type MyThing = string\nproc a {v: MyThing} MyThing { }\n";
        let workspace = Workspace::holding(source);
        backend.set_text_sync(workspace.uri(), source.into()).await;
        let ws = backend
            .rename(
                &workspace.uri(),
                Position {
                    line: 0,
                    character: 5,
                },
                "YourThing",
            )
            .await
            .expect("edit set");
        let changes = ws.changes.expect("changes");
        let edits = changes.get(&workspace.uri()).expect("local edits");
        // Same 3 hits: decl + arg-type + return-type.
        assert_eq!(edits.len(), 3, "{edits:?}");
        for e in edits {
            assert_eq!(e.new_text, "YourThing");
        }
    }

    /// Utility: convert an LSP `Position` (line + UTF-16 char offset,
    /// but at ASCII we treat as byte offset) into a byte index in the
    /// given source. Used to apply text edits in tests.
    fn position_to_offset(source: &str, pos: Position) -> usize {
        let mut cur_line = 0u32;
        let mut cur_col = 0u32;
        for (idx, byte) in source.bytes().enumerate() {
            if cur_line == pos.line && cur_col == pos.character {
                return idx;
            }
            if byte == b'\n' {
                cur_line += 1;
                cur_col = 0;
            } else {
                cur_col += 1;
            }
        }
        source.len()
    }

    #[tokio::test]
    async fn hover_on_call_site_shows_signature() {
        let backend = HtclBackend::new();
        let src = "\
## Greet someone by name.\n\
proc greet {\n\
  ## Who to greet.\n\
  @default(\"world\") name\n\
} { puts \"hi $name\" }\n\
greet -name there\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor on the `g` of the call-site `greet`. Line indices
        // are 0-based.
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 5,
                    character: 0,
                },
            )
            .await
            .expect("hover should return content");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(body.contains("proc greet"), "{body}");
        assert!(body.contains("Greet someone by name."), "{body}");
        assert!(body.contains("### Parameters"), "{body}");
        assert!(body.contains("-name"), "{body}");
        assert!(body.contains("Who to greet."), "{body}");
        assert!(body.contains("@default"), "{body}");
    }

    #[tokio::test]
    async fn hover_on_call_arg_shows_arg_doc() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {\n\
  ## Who to greet.\n\
  @default(\"world\") name\n\
} { puts hi }\n\
greet -name there\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Position cursor on `-name` of the call site (line 4 in the
        // 0-indexed scheme).
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 4,
                    character: 7,
                },
            )
            .await
            .expect("hover should return content");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(body.contains("-name"), "{body}");
        assert!(body.contains("Who to greet."), "{body}");
        assert!(body.contains("@default"), "{body}");
        // Shouldn't include the proc-level header.
        assert!(!body.contains("### Parameters"), "{body}");
    }

    #[tokio::test]
    async fn hover_outside_known_construct_returns_none() {
        let backend = HtclBackend::new();
        backend
            .set_text_sync(uri(), "puts hello world\n".into())
            .await;
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(hover.is_none());
    }

    #[tokio::test]
    async fn goto_definition_jumps_call_to_proc_decl() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {\n  name\n} { puts hi }\n\
greet -name there\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor on the `g` of the call-site `greet` (line 3).
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 3,
                    character: 0,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        // Decl name `greet` is on line 0 at character 5.
        assert_eq!(locs[0].range.start.line, 0);
        assert_eq!(locs[0].range.start.character, 5);
    }

    #[tokio::test]
    async fn goto_definition_resolves_attribute_ident() {
        let backend = HtclBackend::new();
        let src = "\
proc f {\n  has_a\n  @requires(has_a) has_b\n} { }\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor on `has_a` inside `@requires(has_a)`.
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 2,
                    character: 13,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        // Decl `has_a` is on line 1 at character 2.
        assert_eq!(locs[0].range.start.line, 1);
        assert_eq!(locs[0].range.start.character, 2);
    }

    #[tokio::test]
    async fn completion_offers_proc_names_in_command_position() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {} { }\n\
proc grumble {} { }\n\
gr\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor at end of `gr` on line 2.
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 2,
                    character: 2,
                },
            )
            .await;
        let mut labels: Vec<String> =
            items.iter().map(|i| i.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["greet", "grumble"]);
        assert_eq!(items[0].kind, Some(CompletionItemKind::FUNCTION));
    }

    #[test]
    fn in_line_comment_recognizes_standalone_and_indented_comments() {
        // Cursor after `# hello` on a standalone comment line.
        let src = "# hello world\nputs hi\n";
        assert!(in_line_comment(src, 8), "cursor after `#` should count");
        assert!(!in_line_comment(src, 0), "cursor before `#` should not");
        // Indented comment (common inline_comment shape inside a
        // multi-line call).
        let src = "  # inner\nputs hi\n";
        assert!(in_line_comment(src, 5), "cursor after indent + `#`");
        // Doc comments (`##`) share the same shape — start with `#`.
        let src = "## doc\n";
        assert!(in_line_comment(src, 4));
    }

    #[test]
    fn in_line_comment_returns_false_on_code_line() {
        // A `#` appearing later inside a code line (e.g., inside a
        // quoted string) is NOT the start of a comment in htcl —
        // the check requires `#` to be the FIRST non-ws char of
        // the line before the cursor.
        let src = "puts \"hi # not comment\"\n";
        // Cursor right after `#`.
        assert!(!in_line_comment(src, 10));
        // Cursor mid-code, no `#` on the line.
        assert!(!in_line_comment(src, 3));
    }

    #[test]
    fn in_line_comment_scan_stops_at_line_boundary() {
        // A `#` on the PREVIOUS line must not leak into the
        // current line's classification — the backward scan must
        // stop at `\n`.
        let src = "# prev line\nfoo bar\n";
        // Cursor on the code line at `bar`.
        let off = src.find("bar").unwrap() as u32;
        assert!(!in_line_comment(src, off));
    }

    #[tokio::test]
    async fn completion_returns_empty_when_cursor_is_inside_a_comment() {
        // Regression: without this suppression, Helix's LSP client
        // auto-triggers completion on every keystroke inside a
        // `#` comment line, so writing prose pops a dropdown on
        // each space. The suppression fires when the cursor sits
        // on a line whose first non-ws char before the cursor is
        // `#`.
        let backend = HtclBackend::new();
        let src = "\
proc greet {} { }\n\
# writing a note about greet\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor at end of the comment line (line 1, char 27).
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 1,
                    character: 27,
                },
            )
            .await;
        assert!(
            items.is_empty(),
            "expected no completions inside comment, got {items:?}",
        );
        // Sanity: completion on the next (code) line still fires.
        let src = "\
proc greet {} { }\n\
# writing a note about greet\n\
gr\n";
        backend.set_text_sync(uri(), src.into()).await;
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 2,
                    character: 2,
                },
            )
            .await;
        assert!(
            items.iter().any(|i| i.label == "greet"),
            "expected greet in completions on code line, got {items:?}",
        );
    }

    #[tokio::test]
    async fn completion_offers_flags_in_argument_position() {
        let backend = HtclBackend::new();
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg \n";
        backend.set_text_sync(uri(), src.into()).await;
        // Line 4, just after `cfg ` (character 4).
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 4,
                    character: 4,
                },
            )
            .await;
        let mut labels: Vec<String> =
            items.iter().map(|i| i.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["-depth", "-width"]);
        assert_eq!(items[0].kind, Some(CompletionItemKind::FIELD));
    }

    #[tokio::test]
    async fn signature_help_highlights_active_parameter() {
        let backend = HtclBackend::new();
        let src = "\
## Configure the bus.\n\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -depth \n";
        backend.set_text_sync(uri(), src.into()).await;
        // Line 5, after `cfg -depth ` (character 11).
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 5,
                    character: 11,
                },
            )
            .await
            .expect("signature help expected");
        assert_eq!(help.active_parameter, Some(1));
        let info = &help.signatures[0];
        assert!(info.label.starts_with("cfg "), "{}", info.label);
        assert_eq!(info.parameters.as_ref().unwrap().len(), 2);
        match &info.documentation {
            Some(Documentation::MarkupContent(m)) => {
                assert!(m.value.contains("Configure the bus."), "{}", m.value);
            }
            other => panic!("expected markup documentation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn signature_help_includes_return_type_arrow() {
        let backend = HtclBackend::new();
        let src = "\
proc make_widget {} bd_cell { return foo }\n\
make_widget \n";
        backend.set_text_sync(uri(), src.into()).await;
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await
            .expect("signature help expected");
        let info = &help.signatures[0];
        // Label should carry the `→ bd_cell` suffix.
        assert!(info.label.contains("→ bd_cell"), "{}", info.label);
    }

    #[tokio::test]
    async fn hover_on_enum_decl_shows_variants() {
        let backend = HtclBackend::new();
        let src = "\
enum Property = {\n  Scalar: string\n  Nested: int\n}\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Cursor on the enum name (line 0, col 5: 'Property').
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 0,
                    character: 7,
                },
            )
            .await
            .expect("hover on enum decl name");
        if let HoverContents::Markup(MarkupContent { value, .. }) =
            hover.contents
        {
            assert!(value.contains("enum Property"), "{value}");
            assert!(value.contains("Scalar: string"), "{value}");
            assert!(value.contains("Nested: int"), "{value}");
        } else {
            panic!("expected Markup hover");
        }
    }

    #[tokio::test]
    async fn hover_proc_def_includes_return_type() {
        let backend = HtclBackend::new();
        let src = "\
## Builds a widget.\n\
proc make_widget {} dict<string,bd_cell> { return {} }\n";
        backend.set_text_sync(uri(), src.into()).await;
        // Hover on the proc name `make_widget` at line 1.
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 1,
                    character: 8,
                },
            )
            .await
            .expect("hover expected on proc def");
        if let HoverContents::Markup(MarkupContent { value, .. }) =
            hover.contents
        {
            assert!(
                value.contains("→ dict<string,bd_cell>"),
                "expected return type in hover: {value}"
            );
        } else {
            panic!("expected Markup hover contents");
        }
    }

    #[tokio::test]
    async fn signature_help_none_outside_call() {
        let backend = HtclBackend::new();
        backend.set_text_sync(uri(), "puts hi\n".into()).await;
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(help.is_none());
    }

    #[tokio::test]
    async fn goto_definition_unknown_returns_empty() {
        let backend = HtclBackend::new();
        backend.set_text_sync(uri(), "puts hello\n".into()).await;
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(locs.is_empty());
    }

    // --- cross-file (workspace view) tests --------------------------------

    /// Build a temp workspace with a `lib.htcl` defining `greet` and
    /// a `main.htcl` that imports it. Returns the backend with both
    /// files already opened and the URIs.
    async fn temp_workspace_with_import() -> (
        tempfile::TempDir,
        HtclBackend,
        Url, // main.htcl
        Url, // lib.htcl
    ) {
        let (tmp, dir) = canonical_tempdir();
        let lib_path = dir.as_path().join("lib.htcl");
        std::fs::write(
            &lib_path,
            "## Greet someone.\n\
proc greet {\n  ## Who to greet.\n  who\n} { puts \"hi $who\" }\n",
        )
        .unwrap();
        let main_path = dir.as_path().join("main.htcl");
        let main_src = "src lib\ngreet -who world\n";
        std::fs::write(&main_path, main_src).unwrap();

        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        (tmp, backend, main_uri, lib_uri)
    }

    #[tokio::test]
    async fn goto_on_src_import_jumps_to_imported_file() {
        let (_dir, backend, main_uri, lib_uri) =
            temp_workspace_with_import().await;
        // Cursor on the `l` of `src lib` (line 0, col 4).
        let locs = backend
            .goto_definition(
                &main_uri,
                Position {
                    line: 0,
                    character: 4,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, lib_uri);
    }

    #[tokio::test]
    async fn goto_on_call_to_imported_proc_jumps_to_lib() {
        let (_dir, backend, main_uri, lib_uri) =
            temp_workspace_with_import().await;
        // Cursor on `greet` at line 1.
        let locs = backend
            .goto_definition(
                &main_uri,
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .await;
        assert_eq!(locs.len(), 1, "{locs:?}");
        assert_eq!(locs[0].uri, lib_uri);
        // The declaration of `greet` is on lib.htcl line 1 col 5.
        assert_eq!(locs[0].range.start.line, 1);
        assert_eq!(locs[0].range.start.character, 5);
    }

    /// Regression: a call from inside an `if { … }` body should
    /// still find its proc's declaration. The parser leaves the
    /// brace-body as an opaque word, so without an explicit
    /// reparse pass in [`vw_htcl::goto`] the search never reaches
    /// the nested call.
    #[tokio::test]
    async fn goto_from_inside_if_body() {
        let (_tmp, dir) = canonical_tempdir();
        let path = dir.as_path().join("m.htcl");
        let src = "proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   target -x 1\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text_sync(uri.clone(), src.into()).await;
        let locs = backend
            .goto_definition(
                &uri,
                Position {
                    line: 3,
                    character: 4,
                },
            )
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def from inside `if {{…}}` body failed"
        );
    }

    /// Regression: a call from inside `[…]` command substitution
    /// inside `if {…} { … }` — the double-nested shape the IP
    /// wrapper's `if {$bd} { set cell [create_bd_cell …] }
    /// else { set cell [create_ip …] }` scaffold produces. The
    /// reparse pass has to also run `populate_procs` so the
    /// inner CmdSubst.body gets filled in.
    #[tokio::test]
    async fn goto_from_cmdsubst_inside_if_body() {
        let (_tmp, dir) = canonical_tempdir();
        let path = dir.as_path().join("m.htcl");
        let src = "proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   set cell [target -x 1]\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text_sync(uri.clone(), src.into()).await;
        // Cursor on `target` inside `[target -x 1]` on line 3.
        // Line 3 is `    set cell [target -x 1]`; `target` starts
        // at col 17.
        let locs = backend
            .goto_definition(
                &uri,
                Position {
                    line: 3,
                    character: 17,
                },
            )
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def from inside `[[…]]`-inside-`if` failed"
        );
    }

    /// Companion to [`goto_from_cmdsubst_inside_if_body`] for hover.
    #[tokio::test]
    async fn hover_from_cmdsubst_inside_if_body() {
        let (_tmp, dir) = canonical_tempdir();
        let path = dir.as_path().join("m.htcl");
        let src = "## Target proc doc.\n\
                   proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   set cell [target -x 1]\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text_sync(uri.clone(), src.into()).await;
        // Cursor on `target` inside `[target -x 1]` on line 4.
        let hover = backend
            .hover(
                &uri,
                Position {
                    line: 4,
                    character: 17,
                },
            )
            .await;
        assert!(
            hover.is_some(),
            "hover from inside `[[…]]`-inside-`if` returned None"
        );
    }

    /// Same regression as [`goto_from_inside_if_body`], but for
    /// hover — the two share the "reparse brace-body" fix in
    /// [`vw_htcl::goto`] / [`vw_htcl::hover`].
    #[tokio::test]
    async fn hover_from_inside_if_body() {
        let (_tmp, dir) = canonical_tempdir();
        let path = dir.as_path().join("m.htcl");
        let src = "## Target proc doc.\n\
                   proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   target -x 1\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text_sync(uri.clone(), src.into()).await;
        let hover = backend
            .hover(
                &uri,
                Position {
                    line: 4,
                    character: 4,
                },
            )
            .await;
        assert!(
            hover.is_some(),
            "hover from inside `if {{…}}` body returned None"
        );
    }

    /// Reproduces the exact user scenario against the on-disk
    /// `~/src/htcl/amd/` tree. Only runs when that path exists, so
    /// the test is a no-op in CI / fresh checkouts.
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep_real_htcl_tree() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            eprintln!("skipping — {} not present", cpm5_module.display());
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text_sync(cpm5_uri.clone(), text.clone()).await;

        // Find the line + column of `vivado_cmd::set_property` —
        // avoids hard-coding a line number that will drift as the
        // wrapper regenerates.
        let mut target_line = None;
        for (i, line) in text.lines().enumerate() {
            if let Some(col) = line.find("vivado_cmd::set_property") {
                // Cursor on the `set_property` word, past the
                // `vivado_cmd::` prefix (12 chars).
                target_line = Some((i as u32, (col + 12) as u32));
                break;
            }
        }
        let Some((line, character)) = target_line else {
            panic!("no `vivado_cmd::set_property` in cpm5/module.htcl");
        };
        let locs = backend
            .goto_definition(&cpm5_uri, Position { line, character })
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def against real htcl tree returned no location \
             for cpm5/module.htcl:{line}:{character}"
        );
        let hit = &locs[0];
        let path = hit.uri.to_file_path().unwrap();
        assert!(
            path.to_string_lossy().contains("vivado-cmd"),
            "expected to land in the vivado-cmd tree, got {:?}",
            hit
        );
    }

    /// Regression against the on-disk cpm5 tree for goto and hover
    /// on `vivado_cmd::create_bd_cell` — the sole cell-creation
    /// call at the top of `create_cpm5`. (Previously covered
    /// `vivado_cmd::create_ip` too, but the IP generator's
    /// `-bd 0` path is now rejected up front with an `error`, so
    /// the generated wrapper only calls `create_bd_cell`.)
    #[tokio::test]
    async fn goto_and_hover_on_create_bd_cell_and_create_ip_in_cpm5() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text_sync(cpm5_uri.clone(), text.clone()).await;
        for needle in &["vivado_cmd::create_bd_cell"] {
            let (line, character) = text
                .lines()
                .enumerate()
                .find_map(|(i, l)| {
                    l.find(needle).map(|c| (i as u32, (c + 12) as u32))
                })
                .unwrap_or_else(|| panic!("no {needle} in cpm5/module.htcl"));
            let locs = backend
                .goto_definition(&cpm5_uri, Position { line, character })
                .await;
            assert!(
                !locs.is_empty(),
                "goto-def on {needle} at {line}:{character} returned nothing"
            );
            let hover =
                backend.hover(&cpm5_uri, Position { line, character }).await;
            assert!(
                hover.is_some(),
                "hover on {needle} at {line}:{character} returned None"
            );
        }
    }

    /// Companion to [`goto_finds_sibling_workspace_dep_real_htcl_tree`]
    /// for hover — same file, same cursor position, same expected
    /// outcome: the imported proc's signature resolves and hover
    /// returns something rather than `None`.
    #[tokio::test]
    async fn hover_finds_imported_proc_real_htcl_tree() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text_sync(cpm5_uri.clone(), text.clone()).await;
        let target = text.lines().enumerate().find_map(|(i, line)| {
            line.find("vivado_cmd::set_property")
                .map(|col| (i as u32, (col + 12) as u32))
        });
        let Some((line, character)) = target else {
            panic!("no `vivado_cmd::set_property` in cpm5/module.htcl");
        };
        let hover =
            backend.hover(&cpm5_uri, Position { line, character }).await;
        assert!(
            hover.is_some(),
            "hover against real htcl tree returned None \
             for cpm5/module.htcl:{line}:{character}"
        );
    }

    /// Hover + goto on a namespaced newtype (`dcmac::MacPortProps`)
    /// used as a return-type annotation resolve to the type
    /// declaration. Guards the analyzer's type-annotation path
    /// against regressions and validates end-to-end with a real
    /// generated wrapper.
    #[tokio::test]
    async fn hover_goto_on_namespaced_newtype_return_type() {
        let dcmac_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/dcmac/module.htcl");
        if !dcmac_module.exists() {
            return;
        }
        let backend = HtclBackend::new();
        let dcmac_uri = Url::from_file_path(&dcmac_module).unwrap();
        let text = std::fs::read_to_string(&dcmac_module).unwrap();
        backend.set_text_sync(dcmac_uri.clone(), text.clone()).await;
        // Find a real type-annotation site (arg-type slot on
        // `MacPortProps::from` etc.) — not the `namespace eval
        // dcmac::MacPortProps {}` word, which passes the string as
        // a namespace name rather than a type annotation.
        let target = text.lines().enumerate().find_map(|(i, line)| {
            line.find(": dcmac::MacPortProps")
                .map(|col| (i as u32, (col + 9) as u32))
        });
        let Some((line, character)) = target else {
            panic!("no `: dcmac::MacPortProps` in dcmac/module.htcl");
        };
        let hover = backend
            .hover(&dcmac_uri, Position { line, character })
            .await;
        assert!(
            hover.is_some(),
            "hover on `dcmac::MacPortProps` at line {line}:{character} \
             returned None — type-annotation path not wired"
        );
        let locs = backend
            .goto_definition(&dcmac_uri, Position { line, character })
            .await;
        assert!(
            !locs.is_empty(),
            "goto on `dcmac::MacPortProps` at line {line}:{character} \
             returned no locations"
        );
    }

    /// Sibling-workspace fallback with a NESTED src chain — mirrors
    /// the real vivado-cmd layout where `module.htcl` re-sources
    /// per-command files under `cmd/`. `set_property` doesn't live
    /// in the module.htcl entry directly; it's reached through
    /// `src "cmd/set_property.htcl"` inside the dep module. This
    /// caught the actual reproduction case where a shallower test
    /// (proc in the dep's module.htcl) passed but goto-def against
    /// the real vivado-cmd tree still returned nothing.
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep_via_nested_src() {
        let (_tmp, dir) = canonical_tempdir();
        let amd = dir.as_path().join("amd");
        let cpm5 = amd.join("cpm5");
        let vivado_cmd = amd.join("vivado-cmd");
        let vivado_cmd_cmd = vivado_cmd.join("cmd");
        std::fs::create_dir_all(&cpm5).unwrap();
        std::fs::create_dir_all(&vivado_cmd_cmd).unwrap();
        std::fs::write(
            cpm5.join("vw.toml"),
            "[workspace]\nname=\"cpm5\"\nversion=\"0.1.0\"\n\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(
            vivado_cmd.join("vw.toml"),
            "[workspace]\nname=\"vivado-cmd\"\nversion=\"0.1.0\"\n\n\
             [dependencies]\n",
        )
        .unwrap();
        // vivado-cmd/module.htcl re-sources set_property.htcl —
        // matching the real layout.
        std::fs::write(
            vivado_cmd.join("module.htcl"),
            "src \"cmd/set_property.htcl\"\n",
        )
        .unwrap();
        // vivado-cmd/cmd/set_property.htcl defines the proc.
        let set_property_path = vivado_cmd_cmd.join("set_property.htcl");
        std::fs::write(
            &set_property_path,
            "namespace eval vivado_cmd {\n  \
                proc set_property { args } { }\n}\n",
        )
        .unwrap();
        let cpm5_module = cpm5.join("module.htcl");
        std::fs::write(
            &cpm5_module,
            "src @vivado-cmd\nvivado_cmd::set_property -dict {} -objects x\n",
        )
        .unwrap();

        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        backend
            .set_text_sync(
                cpm5_uri.clone(),
                std::fs::read_to_string(&cpm5_module).unwrap(),
            )
            .await;
        // Cursor on `set_property` — the call is
        // `vivado_cmd::set_property ...` (col 0). `vivado_cmd::`
        // is 12 chars; `set_property` starts at col 12.
        let locs = backend
            .goto_definition(
                &cpm5_uri,
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await;
        assert!(!locs.is_empty(), "goto-def returned no location");
        let set_property_uri = Url::from_file_path(&set_property_path).unwrap();
        assert_eq!(
            locs[0].uri, set_property_uri,
            "expected jump to {set_property_uri}, got {:?}",
            locs[0]
        );
    }

    /// Sibling-workspace fallback: when a file's own workspace
    /// doesn't declare a `@dep/…` import but a sibling directory
    /// under a shared parent DOES have its own `vw.toml` with a
    /// matching basename, the resolver should still find it.
    ///
    /// Layout (mirrors `~/src/htcl/amd/{cpm5,vivado-cmd}` as the
    /// user's actual reproduction):
    ///
    ///   <tmp>/amd/cpm5/vw.toml            # empty deps
    ///        /amd/cpm5/module.htcl        # calls vivado_cmd::foo
    ///        /amd/vivado-cmd/vw.toml
    ///        /amd/vivado-cmd/module.htcl  # namespace eval vivado_cmd { proc foo … }
    ///
    /// Regression for "goto-def returns 'No definition found' once
    /// I'm in a vw-tracked dependency."
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep() {
        let (_tmp, dir) = canonical_tempdir();
        let amd = dir.as_path().join("amd");
        let cpm5 = amd.join("cpm5");
        let vivado_cmd = amd.join("vivado-cmd");
        std::fs::create_dir_all(&cpm5).unwrap();
        std::fs::create_dir_all(&vivado_cmd).unwrap();
        std::fs::write(
            cpm5.join("vw.toml"),
            "[workspace]\nname=\"cpm5\"\nversion=\"0.1.0\"\n\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(
            vivado_cmd.join("vw.toml"),
            "[workspace]\nname=\"vivado-cmd\"\nversion=\"0.1.0\"\n\n\
             [dependencies]\n",
        )
        .unwrap();
        // The vivado-cmd module: define namespace `vivado_cmd` with
        // a `foo` proc so a call to `vivado_cmd::foo` from cpm5 has
        // somewhere to land.
        let vivado_module = vivado_cmd.join("module.htcl");
        std::fs::write(
            &vivado_module,
            "namespace eval vivado_cmd {\n  proc foo { x } { }\n}\n",
        )
        .unwrap();
        let cpm5_module = cpm5.join("module.htcl");
        std::fs::write(&cpm5_module, "src @vivado-cmd\nvivado_cmd::foo -x 1\n")
            .unwrap();

        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        backend
            .set_text_sync(
                cpm5_uri.clone(),
                std::fs::read_to_string(&cpm5_module).unwrap(),
            )
            .await;

        // Cursor on `foo` — line 1, at the start of the call word
        // (`vivado_cmd::foo` starts at column 0, `foo` starts after
        // `vivado_cmd::` which is 12 chars).
        let locs = backend
            .goto_definition(
                &cpm5_uri,
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await;
        assert!(!locs.is_empty(), "goto-def returned no location");
        let vivado_uri = Url::from_file_path(&vivado_module).unwrap();
        assert_eq!(locs[0].uri, vivado_uri, "landed on wrong file");
    }

    #[tokio::test]
    async fn completion_in_command_position_lists_imported_procs() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Append a partial proc name at end of file so cursor lands in
        // command position.
        let new_text = "src lib\ngreet -who world\ngre\n";
        backend
            .set_text_sync(main_uri.clone(), new_text.into())
            .await;
        let items = backend
            .completion(
                &main_uri,
                Position {
                    line: 2,
                    character: 3,
                },
            )
            .await;
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"greet"), "labels = {labels:?}");
    }

    #[tokio::test]
    async fn hover_on_imported_call_shows_signature() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Hover on `greet` on line 1.
        let hover = backend
            .hover(
                &main_uri,
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .await
            .expect("hover");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!(),
        };
        assert!(body.contains("proc greet"), "{body}");
        assert!(body.contains("Greet someone."), "{body}");
        assert!(body.contains("-who"), "{body}");
    }

    #[tokio::test]
    async fn diagnostics_accept_calls_to_imported_procs() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // No errors when the call matches the imported signature.
        let diags = backend.diagnostics(&main_uri).await;
        let errs: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[tokio::test]
    async fn hover_works_on_call_inside_command_substitution() {
        // Mirrors the user's cips.htcl shape:
        //   src lib
        //   set cell [greet -who x]
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        let new_text = "src lib\nset cell [greet -who x]\n";
        backend
            .set_text_sync(main_uri.clone(), new_text.into())
            .await;
        // Cursor on `greet` inside the `[ … ]` on line 1.
        let hover = backend
            .hover(
                &main_uri,
                Position {
                    line: 1,
                    character: 11,
                },
            )
            .await
            .expect("hover should resolve calls inside `[…]`");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!(),
        };
        assert!(body.contains("proc greet"), "{body}");
    }

    #[tokio::test]
    async fn signature_help_works_on_call_inside_command_substitution() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Cursor right after `greet ` inside `[ … ]`.
        let new_text = "src lib\nset cell [greet ]\n";
        backend
            .set_text_sync(main_uri.clone(), new_text.into())
            .await;
        let help = backend
            .signature_help(
                &main_uri,
                Position {
                    line: 1,
                    character: 16,
                },
            )
            .await
            .expect("signature help inside `[…]`");
        assert!(
            help.signatures[0].label.starts_with("greet"),
            "{:?}",
            help.signatures[0].label
        );
    }

    #[tokio::test]
    async fn diagnostics_still_flag_wrong_flag_on_imported_call() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        backend
            .set_text_sync(
                main_uri.clone(),
                "src lib\ngreet -whoz world\n".into(),
            )
            .await;
        let diags = backend.diagnostics(&main_uri).await;
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("undefined argument -whoz")),
            "{diags:?}"
        );
    }

    #[tokio::test]
    async fn workspace_diagnostics_surface_errors_in_imported_files() {
        // Break the imported lib (return with a value in an
        // unannotated proc — one of the new checks) and open the
        // main file that `src`s it. The main file itself is
        // error-free. workspace_diagnostics must report the lib's
        // diagnostic against the LIB's URI so the editor's
        // workspace picker points to the right file.
        let (_tmp, dir) = canonical_tempdir();
        let lib_path = dir.as_path().join("broken.htcl");
        std::fs::write(&lib_path, "proc broken {} { return 42 }\n").unwrap();
        let main_path = dir.as_path().join("main.htcl");
        let main_src = "src broken\n";
        std::fs::write(&main_path, main_src).unwrap();
        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        // Set the editor's workspace root to the temp dir so the
        // filter accepts the lib file (which lives inside it).
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        // Main entry gets an entry (possibly empty), so the editor
        // can clear stale state.
        assert!(ws.contains_key(&main_uri), "main uri missing: {ws:?}");
        let lib_diags = ws.get(&lib_uri).unwrap_or_else(|| {
            panic!("no diagnostics routed to {lib_uri}: {ws:?}")
        });
        assert!(
            lib_diags
                .iter()
                .any(|d| d.message.contains("no declared return type")),
            "expected the return-in-unannotated-proc error in lib: {lib_diags:?}",
        );
    }

    #[tokio::test]
    async fn workspace_diagnostics_clear_when_import_is_fixed() {
        // When the user fixes an error in an imported file, the
        // next workspace_diagnostics call must include an
        // entry for that file — with an EMPTY diagnostic list.
        // That empty payload is what the editor overwrites its
        // cached "had errors" state with; without it, the
        // stale errors linger in `space-D` even after the fix.
        let (_tmp, dir) = canonical_tempdir();
        let lib_path = dir.as_path().join("lib.htcl");
        std::fs::write(&lib_path, "proc broken {} { return 42 }\n").unwrap();
        let main_path = dir.as_path().join("main.htcl");
        let main_src = "src lib\n";
        std::fs::write(&main_path, main_src).unwrap();
        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        // Sanity: broken lib produces a workspace diagnostic.
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        assert!(
            !ws.get(&lib_uri).map(|v| v.is_empty()).unwrap_or(true),
            "expected non-empty lib diagnostics before fix: {ws:?}",
        );
        // Fix the lib on disk. Since main.htcl is what's open,
        // resetting main's text re-triggers the workspace build
        // and reloads lib from disk.
        std::fs::write(&lib_path, "proc fixed {} { puts hi }\n").unwrap();
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        // The lib URI must appear with an empty list so the
        // editor clears its cached errors.
        let lib_after = ws.get(&lib_uri).unwrap_or_else(|| {
            panic!("lib uri missing from post-fix workspace diags: {ws:?}")
        });
        assert!(
            lib_after.is_empty(),
            "expected empty lib diagnostics after fix, got {lib_after:?}",
        );
    }

    #[tokio::test]
    async fn workspace_diagnostics_clear_when_open_import_is_fixed() {
        // Regression for the "stuck diagnostic in Helix" bug:
        // both `main.htcl` and `lib.htcl` are open. `main` srcs
        // `lib`. Fixing `lib.htcl` should IMMEDIATELY clear its
        // diagnostics — even though `main`'s analysis (which
        // still holds a cross-file diag pointing at `lib`) hasn't
        // been reindexed yet. The aggregator must trust `lib`'s
        // own opened analysis over any stale cross-file entries
        // targeting it from other files.
        let (_tmp, dir) = canonical_tempdir();
        let lib_path = dir.as_path().join("lib.htcl");
        let bad_lib = "proc broken {} { return 42 }\n";
        std::fs::write(&lib_path, bad_lib).unwrap();
        let main_path = dir.as_path().join("main.htcl");
        let main_src = "src lib\n";
        std::fs::write(&main_path, main_src).unwrap();
        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        // Open both. `main`'s analysis will contain a cross-file
        // diagnostic for `lib`; `lib`'s own analysis will contain
        // its own local diagnostic.
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        backend.set_text_sync(lib_uri.clone(), bad_lib.into()).await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        assert!(
            !ws.get(&lib_uri).map(|v| v.is_empty()).unwrap_or(true),
            "expected non-empty lib diagnostics before fix: {ws:?}",
        );
        // Fix `lib` via the OPEN buffer — but DO NOT touch `main`.
        // `main`'s analysis is now stale; its cross-file entry
        // still points at `lib:<old-error>`.
        backend
            .set_text_sync(lib_uri.clone(), "proc fixed {} unit {}\n".into())
            .await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        let lib_after = ws.get(&lib_uri).unwrap_or_else(|| {
            panic!("lib uri missing from post-fix workspace diags: {ws:?}")
        });
        assert!(
            lib_after.is_empty(),
            "expected empty lib diagnostics after fixing the open buffer, got \
             {lib_after:?} (main.htcl's stale analysis re-injected them)",
        );
    }

    /// Wait until a preloaded/virtual-open URI has a committed
    /// analysis. Test-only helper — `wait_for_reindex` only fires
    /// on the NEXT commit, so it hangs when the preload's indexer
    /// has already committed by the time the test observer
    /// subscribes.
    #[cfg(test)]
    async fn wait_until_analysis_present(backend: &HtclBackend, uri: &Url) {
        for _ in 0..200 {
            if backend.analysis_for(uri).await.is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for analysis of {uri}");
    }

    /// Poll `analysis_for(uri)` until the returned analysis
    /// satisfies `predicate`, or timeout. Test-only helper — the
    /// fan-out reindex fires a NEW commit under a NEW generation,
    /// so callers waiting for downstream refresh can't just re-use
    /// `wait_for_reindex`; they need a predicate that recognizes
    /// "the analysis I want has arrived."
    #[cfg(test)]
    async fn wait_until_analysis_matches(
        backend: &HtclBackend,
        uri: &Url,
        predicate: impl Fn(&DocAnalysis) -> bool,
    ) {
        for _ in 0..200 {
            if let Some(a) = backend.analysis_for(uri).await {
                if predicate(&a) {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for matching analysis of {uri}");
    }

    #[tokio::test]
    async fn edit_to_imported_file_ripples_to_open_importer() {
        // Regression: user edits `clock.htcl` (adds a doc comment
        // to `configure_clocks`) but hovers in `module.htcl` still
        // show the pre-edit signature. Root cause was no fan-out
        // reindex — module's analysis committed BEFORE the clock
        // edit and had no mechanism to re-fire on upstream changes.
        //
        // Test shape: dir with `lib.htcl` (defines a proc) and
        // `main.htcl` (`src lib`). Open both. Update lib on disk
        // AND via `set_text` to add a doc comment. Assert main's
        // analysis picks it up WITHOUT re-touching main.
        let (_tmp, dir) = canonical_tempdir();
        std::fs::write(
            dir.as_path().join("vw.toml"),
            "[workspace]\nname = \"t\"\n",
        )
        .unwrap();
        let lib_path = dir.as_path().join("lib.htcl");
        let lib_v1 = "proc greet {} unit { puts hi }\n";
        std::fs::write(&lib_path, lib_v1).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        let main_path = dir.as_path().join("main.htcl");
        let main_src = "src lib\ngreet\n";
        std::fs::write(&main_path, main_src).unwrap();
        let main_uri = Url::from_file_path(&main_path).unwrap();

        let backend = HtclBackend::new();
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        backend
            .set_text_sync(main_uri.clone(), main_src.into())
            .await;
        backend.set_text_sync(lib_uri.clone(), lib_v1.into()).await;
        // Sanity: main's initial view contains lib_v1's proc but
        // NO doc comment yet.
        let a = backend.analysis_for(&main_uri).await.unwrap();
        assert!(a.view.view_source.contains("proc greet"));
        assert!(
            !a.view.view_source.contains("## greeting"),
            "pre-edit view unexpectedly has doc comment: {}",
            a.view.view_source,
        );

        // User edits lib.htcl: add a doc comment. Update disk
        // (equivalent of `did_save`) AND the open buffer.
        let lib_v2 = "## greeting proc\nproc greet {} unit { puts hi }\n";
        std::fs::write(&lib_path, lib_v2).unwrap();
        backend.set_text_sync(lib_uri.clone(), lib_v2.into()).await;

        // main.htcl is NOT touched here — the fan-out is what
        // should ripple the change through.
        wait_until_analysis_matches(&backend, &main_uri, |a| {
            a.view.view_source.contains("## greeting")
        })
        .await;
    }

    #[tokio::test]
    async fn workspace_diagnostics_preload_covers_unopened_entry_points() {
        // The workspace has a `design.htcl` (entry point `vw check`
        // would discover) with warnings. The editor has NOT opened
        // it. `workspace_diagnostics` should STILL surface those
        // warnings because `set_workspace_roots` preloads the same
        // entry-point set. Without this, Helix's space-D picker
        // would show nothing for warnings in files the user
        // hasn't visited.
        let (_tmp, dir) = canonical_tempdir();
        // Minimal vw.toml to make this a valid workspace root
        // (workspace-discovery walks up looking for it).
        std::fs::write(
            dir.as_path().join("vw.toml"),
            "[workspace]\nname = \"t\"\n",
        )
        .unwrap();
        // design.htcl carries a stub proc with a `@default(0)`
        // arg; the redundant-default warning fires on the call
        // site below.
        let design_src = "\
proc use_it { @default(0) count } unit { puts $count }
use_it -count 0
";
        let design_path = dir.as_path().join("design.htcl");
        std::fs::write(&design_path, design_src).unwrap();
        let design_uri = Url::from_file_path(&design_path).unwrap();
        // Open a DIFFERENT file — `other.htcl` — that does NOT
        // src design.htcl. Without preload, design.htcl wouldn't
        // appear in the docs map at all.
        let other_path = dir.as_path().join("other.htcl");
        std::fs::write(&other_path, "puts hi\n").unwrap();
        let other_uri = Url::from_file_path(&other_path).unwrap();
        let backend = HtclBackend::new();
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        backend
            .set_text_sync(other_uri.clone(), "puts hi\n".into())
            .await;
        // Wait for the preload's design.htcl indexer to commit.
        // `wait_for_reindex` would hang if the preload already
        // committed (it waits for the NEXT commit); the poll
        // helper handles the already-committed case correctly.
        wait_until_analysis_present(&backend, &design_uri).await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        let design_diags = ws.get(&design_uri).unwrap_or_else(|| {
            panic!(
                "design.htcl missing from workspace diags — preload didn't \
                 register it. Full map: {ws:?}"
            )
        });
        assert!(
            design_diags.iter().any(|d| d.message.contains("redundant")),
            "expected redundant-default warning from preloaded design.htcl, \
             got {design_diags:?}",
        );
    }

    #[tokio::test]
    async fn workspace_diagnostics_awaits_pending_preload_indexers() {
        // The race the previous test doesn't cover: the user
        // opens a file BEFORE preload indexers commit. If
        // `workspace_diagnostics` doesn't wait for preloads, the
        // fan-out that publishes for space-D reads a partial
        // snapshot — preloaded URIs whose indexer is still
        // running contribute NOTHING, so their diagnostics are
        // silently dropped from the picker.
        //
        // We simulate the race by NOT awaiting the preload
        // between `set_workspace_roots` and the first
        // `workspace_diagnostics` call. Preload for `warn.htcl`
        // hasn't committed at that instant; the assertion is
        // that `workspace_diagnostics` still returns the warning
        // (having awaited the commit internally).
        let (_tmp, dir) = canonical_tempdir();
        std::fs::write(
            dir.as_path().join("vw.toml"),
            "[workspace]\nname = \"t\"\n",
        )
        .unwrap();
        let warn_src = "\
proc use_it { @default(0) count } unit { puts $count }
use_it -count 0
";
        let warn_path = dir.as_path().join("design.htcl");
        std::fs::write(&warn_path, warn_src).unwrap();
        let warn_uri = Url::from_file_path(&warn_path).unwrap();
        let backend = HtclBackend::new();
        // set_workspace_roots kicks off the preload but returns
        // BEFORE any preload indexer commits.
        backend
            .set_workspace_roots(vec![dir.as_path().to_path_buf()])
            .await;
        // Straight to workspace_diagnostics — no
        // wait_until_analysis_present. This is the racey path.
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        let diags = ws.get(&warn_uri).unwrap_or_else(|| {
            panic!("preloaded design.htcl missing from ws diags: {ws:?}")
        });
        assert!(
            diags.iter().any(|d| d.message.contains("redundant")),
            "expected redundant-default warning, got {diags:?}",
        );
    }

    #[tokio::test]
    async fn workspace_diagnostics_skip_out_of_workspace_deps() {
        // The workspace is `main_dir`; the imported `dep.htcl`
        // lives OUTSIDE it. Errors in the dep should NOT show up
        // in workspace diagnostics — that's just noise for a file
        // the user isn't editing from this workspace.
        let (_dep_tmp, dep_dir) = canonical_tempdir();
        let dep_path = dep_dir.as_path().join("dep.htcl");
        std::fs::write(&dep_path, "proc broken {} { return 42 }\n").unwrap();
        let (_main_tmp, main_dir) = canonical_tempdir();
        let main_path = main_dir.as_path().join("main.htcl");
        // Use an absolute `src` pointing at the dep tempfile.
        let dep_str = dep_path.to_string_lossy().into_owned();
        // Strip the .htcl since `src` re-adds it.
        let dep_no_ext = dep_str.trim_end_matches(".htcl");
        let main_src = format!("src {dep_no_ext}\n");
        std::fs::write(&main_path, &main_src).unwrap();
        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let dep_uri = Url::from_file_path(&dep_path).unwrap();
        backend
            .set_workspace_roots(vec![main_dir.as_path().to_path_buf()])
            .await;
        backend
            .set_text_sync(main_uri.clone(), main_src.clone())
            .await;
        let ws: std::collections::HashMap<Url, Vec<Diagnostic>> =
            backend.workspace_diagnostics().await.into_iter().collect();
        assert!(
            !ws.contains_key(&dep_uri),
            "dep diagnostics should be filtered out: {ws:?}",
        );
    }

    #[tokio::test]
    async fn save_skips_debounce_and_commits_immediately() {
        // Simulates the "small edit + Ctrl-s" flow: the user made
        // a tiny change (so set_text just fired a 250ms-debounced
        // indexer that hasn't started yet), then saved. `save`
        // must abort the pending debounced task and commit its
        // OWN indexer without waiting.
        //
        // We verify by racing `save` against a bounded timeout:
        // if `save` still went through the debounce, this would
        // time out because the sleep would still be pending.
        let backend = HtclBackend::new();
        let uri = Url::parse("file:///tmp/save-test.htcl").unwrap();
        // `set_text` puts a debounced indexer in flight — it
        // won't commit for 250ms even though the analysis itself
        // is fast for this trivial input.
        backend
            .set_text(uri.clone(), "proc f {} unit { puts hi }\n".into())
            .await;
        // `save` bumps the generation and spawns a fresh
        // zero-debounce indexer, which should commit essentially
        // right away. If we're wrong and save honors the
        // debounce, the `changed()` await would block ~250ms;
        // set a 100ms timeout to catch that regression.
        let saved = backend.save(&uri);
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            async {
                saved.await;
                backend.wait_for_reindex(&uri).await;
            },
        )
        .await;
        assert!(waited.is_ok(), "save didn't commit within timeout");
        // And the committed analysis must actually exist.
        let a = backend.analysis_for(&uri).await;
        assert!(a.is_some(), "save didn't leave a committed analysis");
    }
}
