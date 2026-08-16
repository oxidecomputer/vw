// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! LSP server entry point. Owns the per-language backends and
//! dispatches `textDocument/*` requests by URI.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::notification::Progress;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, info};

use crate::backend::LanguageBackend;
use crate::htcl_backend::HtclBackend;

pub struct Analyzer {
    client: Client,
    backends: Vec<Arc<dyn LanguageBackend>>,
    /// Monotonic counter for `$/progress` tokens. Every user-facing
    /// slow operation (diagnostics, goto-def, hover, completion)
    /// generates a fresh token and reports begin/end so Helix and
    /// other LSP clients render a pulsating "indexing" indicator
    /// while the request is in flight. Wrapped in Arc so the
    /// background diagnostic-publish task fired from did_change
    /// can share the counter with the foreground handlers.
    progress_seq: Arc<AtomicU64>,
}

impl Analyzer {
    pub fn new(client: Client) -> Self {
        let backends: Vec<Arc<dyn LanguageBackend>> = vec![
            Arc::new(HtclBackend::new()),
            Arc::new(crate::VhdlBackend::new(client.clone())),
        ];
        Self {
            client,
            backends,
            progress_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    fn backend_for(&self, uri: &Url) -> Option<Arc<dyn LanguageBackend>> {
        self.backends.iter().find(|b| b.handles(uri)).cloned()
    }

    /// Fire a background diagnostics publish for `uri`. Returns
    /// immediately; the spawned task awaits the current indexer
    /// via the backend's `analysis_for` and publishes when it
    /// completes. Used from `did_open`/`did_change` so the LSP's
    /// notification queue isn't stalled by the ~1s indexer
    /// wall-clock — rapid typing no longer serializes into a
    /// queue of stale re-indexes.
    ///
    /// The progress token creation is fire-and-forget from a
    /// separate task, wrapping only the `analysis_for` await —
    /// the diagnostics publish is NOT gated on the client's
    /// progress-create response. If we wrapped publish itself in
    /// `with_progress`, a client that never responds to
    /// `window/workDoneProgress/create` would hang the entire
    /// pipeline. Progress is UX polish; diagnostics are the
    /// contract, so diagnostics win.
    fn spawn_publish_diagnostics(&self, uri: Url, version: Option<i32>) {
        let Some(backend) = self.backend_for(&uri) else {
            return;
        };
        if backend.pushes_diagnostics() {
            // Backend already published diagnostics inside
            // `set_text` via its own outbound RPC. Skipping the
            // pull path avoids racing with (and empty-clobbering)
            // that side-channel publish. See
            // `LanguageBackend::pushes_diagnostics` docs.
            return;
        }
        let client = self.client.clone();
        let progress_seq = self.progress_seq.clone();
        let uri_task = uri.clone();
        // Detached task: wait for the next indexer commit while an
        // LSP progress spinner is active, then publish the diagnostics
        // from THAT fresh analysis. Wrapping the wait in
        // `with_progress` is what makes Helix's pulsing "indexing…"
        // indicator show up during the rebuild — the previous version
        // wrapped an empty `async {}` future so Begin+End fired in
        // the same millisecond, effectively no-op.
        //
        // Reads (completion, hover, goto-def) DO NOT go through this
        // task — they use `backend.diagnostics` / `analysis_for`
        // directly and are served instantly from the stale-cache. So
        // typing latency stays great; only the diagnostics-refresh +
        // progress spinner are gated on the actual rebuild.
        tokio::spawn(async move {
            with_progress(
                &client,
                &progress_seq,
                "Indexing",
                uri_task.as_ref(),
                backend.wait_for_reindex(&uri_task),
            )
            .await;
            let diags = backend.diagnostics(&uri_task).await;
            debug!(
                uri = %uri_task,
                count = diags.len(),
                "publishing diagnostics"
            );
            client
                .publish_diagnostics(uri_task.clone(), diags, version)
                .await;
            // Fan out cross-file diagnostics for EVERY file the
            // just-completed analysis touched. Helix's `space-D`
            // workspace-diagnostic view reads its cache of pushed
            // `publishDiagnostics` — the LSP 3.17 pull path we
            // implement in `workspace_diagnostic` isn't wired in
            // there yet, so without this fan-out the picker stays
            // empty until the user actually opens each broken
            // file. We publish empty diagnostics for files with
            // no findings too, so a fixed error clears from the
            // picker as soon as the change commits.
            for (uri, diagnostics) in backend.workspace_diagnostics().await {
                if uri == uri_task {
                    continue;
                }
                client.publish_diagnostics(uri, diagnostics, None).await;
            }
        });
    }
}

/// Free-function `with_progress` that takes just the components
/// needed to negotiate a workDoneProgress token. Sharing the impl
/// this way lets the background diagnostic-publish task fire
/// progress notifications without cloning the whole Analyzer.
async fn with_progress<T>(
    client: &Client,
    progress_seq: &AtomicU64,
    title: &str,
    message: &str,
    fut: impl Future<Output = T>,
) -> T {
    let seq = progress_seq.fetch_add(1, Ordering::Relaxed);
    let token = NumberOrString::String(format!("vw-analyzer-{seq}"));
    // Server-initiated progress: create the token first. If the
    // client refuses, fall through to the future without
    // reporting — the request still runs, just without the
    // spinner.
    let created = client
        .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
            token: token.clone(),
        })
        .await
        .is_ok();
    if created {
        let begin = ProgressParams {
            token: token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                WorkDoneProgressBegin {
                    title: title.to_string(),
                    cancellable: Some(false),
                    message: Some(message.to_string()),
                    percentage: None,
                },
            )),
        };
        client.send_notification::<Progress>(begin).await;
    }
    let result = fut.await;
    if created {
        let end = ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                WorkDoneProgressEnd { message: None },
            )),
        };
        client.send_notification::<Progress>(end).await;
    }
    result
}

#[tower_lsp::async_trait]
impl LanguageServer for Analyzer {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        info!("vw-analyzer initializing");
        // Capture the editor's workspace roots so backends can use
        // them as fallback dep-lookup dirs when analyzing files
        // opened outside the nearest `vw.toml`. Newer LSP clients
        // send `workspaceFolders`; older ones use `rootUri` — we
        // accept whichever is present. Missing → empty (each
        // file's own workspace still resolves in isolation, which
        // was the pre-fallback behavior).
        let roots = collect_workspace_roots(&params);
        for backend in &self.backends {
            backend.set_workspace_roots(roots.clone()).await;
        }
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "vw-analyzer".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // FULL sync — the client sends the whole buffer
                // on every change. Do NOT switch this to
                // `TextDocumentSyncCapability::Options { ... }`
                // to opt into `didSave`: Helix's LSP client
                // stopped sending `didChange` altogether when we
                // tried that (verified 2026-07 — no
                // notifications reached the server after the
                // switch, and diagnostics froze until reload).
                // Keep this `Kind(FULL)` until we find a Helix-
                // safe way to also receive save events (e.g.
                // dynamic registration via
                // `client/registerCapability`).
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    // `-` opens a flag list; a space after a flag pops
                    // its `@enum(…)` choices (or the next available
                    // flags when there are no enum constraints), so
                    // the user doesn't have to start typing blind to
                    // discover options.
                    trigger_characters: Some(vec![
                        "-".to_string(),
                        " ".to_string(),
                    ]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![
                        " ".to_string(),
                        "-".to_string(),
                    ]),
                    retrigger_characters: Some(vec!["-".to_string()]),
                    work_done_progress_options: Default::default(),
                }),
                rename_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // LSP 3.17 pull-based diagnostics — Helix uses this
                // for `space-D`'s workspace-wide diagnostic picker.
                // `workspace_diagnostics: true` also opts us into
                // the `workspace/diagnostic` request; without it the
                // editor only knows about diagnostics we've
                // proactively pushed for open files.
                diagnostic_provider: Some(
                    DiagnosticServerCapabilities::Options(DiagnosticOptions {
                        identifier: Some("vw-htcl".into()),
                        inter_file_dependencies: true,
                        workspace_diagnostics: true,
                        work_done_progress_options: Default::default(),
                    }),
                ),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("vw-analyzer initialized");
        // Dynamically register `workspace/didChangeWatchedFiles`
        // for the paths any backend cares about. We don't declare
        // static capability at `initialize` time because dynamic
        // registration lets each backend pick its own patterns —
        // today the VHDL backend needs `vw.toml`, `vw.lock`, and
        // `ip/**/*.htcl` to reflect `vw update` and IP-config
        // edits back into the wrapped `vhdl_ls::VHDLServer`, plus
        // `**/*.vhd{,l}` because that server's config is a concrete
        // file list: a source added or removed on disk changes the
        // library mapping, and until the config is re-rendered the
        // new file resolves nothing (`No primary unit '<pkg>'
        // within library 'work'`). The VHDL backend also re-checks
        // membership on open/save, so this registration failing
        // degrades rather than breaks.
        //
        // Registration failures (client that doesn't advertise
        // dynamic registration, or refuses this specific one) are
        // logged and swallowed — the LSP still runs, just without
        // the reactive config-reload path.
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/vw.toml".into()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/vw.lock".into()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/ip/**/*.htcl".into()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.vhd".into()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.vhdl".into()),
                kind: None,
            },
        ];
        let registration = Registration {
            id: "vw-analyzer-watched-files".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(
                DidChangeWatchedFilesRegistrationOptions { watchers },
            )
            .ok(),
        };
        if let Err(e) =
            self.client.register_capability(vec![registration]).await
        {
            info!(
                "vw-analyzer: dynamic watched-files registration \
                 failed ({e}); config reactivity disabled"
            );
        }
    }

    async fn did_change_watched_files(
        &self,
        params: DidChangeWatchedFilesParams,
    ) {
        for backend in &self.backends {
            backend.did_change_watched_files(&params).await;
        }
    }

    async fn did_change_workspace_folders(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        // Rebuild the roots list from scratch on any change. We
        // don't retain state between initialize and here, so an
        // added-only event still tells us the FULL updated set —
        // both `added` and `removed` are already applied by the
        // client before this notification per LSP spec.
        let added: Vec<std::path::PathBuf> = params
            .event
            .added
            .iter()
            .filter_map(|f| f.uri.to_file_path().ok())
            .collect();
        for backend in &self.backends {
            backend.set_workspace_roots(added.clone()).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        info!("vw-analyzer shutting down");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = Some(params.text_document.version);
        debug!(%uri, "did_open");
        if let Some(backend) = self.backend_for(&uri) {
            backend
                .set_text(uri.clone(), params.text_document.text)
                .await;
        }
        // Fire the diagnostic publish as a background task so the
        // ~1s indexer wall-clock doesn't stall tower-lsp's
        // notification queue. Rapid typing (each keystroke fires
        // did_change → set_text → publish) previously serialized
        // into a queue of stale re-indexes; now every did_change
        // returns in microseconds and the LATEST index's
        // diagnostics arrive whenever it wins the abort race.
        self.spawn_publish_diagnostics(uri, version);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = Some(params.text_document.version);
        let Some(backend) = self.backend_for(&uri) else {
            return;
        };
        // FULL sync: each change is the entire new text.
        if let Some(change) = params.content_changes.into_iter().last() {
            backend.set_text(uri.clone(), change.text).await;
        }
        // Background publish (see `did_open`).
        self.spawn_publish_diagnostics(uri, version);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(backend) = self.backend_for(&uri) {
            backend.close(&uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        debug!(%uri, "did_save");
        if let Some(backend) = self.backend_for(&uri) {
            // Zero-debounce reindex — the whole point of
            // handling save is that `Ctrl-s` should force a
            // fresh check now, not 250ms from now.
            backend.save(&uri).await;
        }
        // Same publish path as did_change so the fresh index's
        // diagnostics land in the editor.
        self.spawn_publish_diagnostics(uri, None);
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        let symbols = backend.document_symbols(&uri).await;
        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Nested(symbols)))
        }
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query;
        // Walk every registered backend (today just one) and merge the
        // matches — keeps the same dispatch shape as `backend_for` so
        // adding a second language later doesn't need a refactor.
        let mut symbols = Vec::new();
        for backend in &self.backends {
            symbols.extend(backend.workspace_symbols(&query).await);
        }
        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(symbols))
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // Reads from the cached DocAnalysis — no per-request
        // progress wrapping needed since the answer lands in
        // microseconds after indexing has completed.
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        Ok(backend.hover(&uri, position).await)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Cached — see the note on `hover`.
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let position = params.text_document_position_params.position;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        let locs = backend.goto_definition(&uri, position).await;
        if locs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Array(locs)))
        }
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        // Cached — see the note on `hover`.
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        let items = backend.completion(&uri, position).await;
        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let position = params.text_document_position_params.position;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        Ok(backend.signature_help(&uri, position).await)
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        Ok(backend.rename(&uri, position, &new_name).await)
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let Some(backend) = self.backend_for(&uri) else {
            return Ok(None);
        };
        let locs = backend
            .references(&uri, position, include_declaration)
            .await;
        if locs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locs))
        }
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        // Pull-based single-file diagnostics. Same payload the
        // push path serves via `publishDiagnostics` — the editor
        // may request it explicitly (Helix does when the buffer
        // opens, before any push has fired) as a
        // no-guess-when-they-arrive alternative.
        let uri = params.text_document.uri.clone();
        let items = match self.backend_for(&uri) {
            Some(backend) => backend.diagnostics(&uri).await,
            None => Vec::new(),
        };
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(
                RelatedFullDocumentDiagnosticReport {
                    related_documents: None,
                    full_document_diagnostic_report:
                        FullDocumentDiagnosticReport {
                            result_id: None,
                            items,
                        },
                },
            ),
        ))
    }

    async fn workspace_diagnostic(
        &self,
        _params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        // Collect from every backend. Each returns a set of
        // (uri, diagnostics) tuples pulled from its open docs'
        // workspace-view analyses — files transitively `src`d by
        // an open document surface their errors here even if the
        // user hasn't opened them, which is what makes Helix's
        // `space-D` picker useful for whole-workspace triage.
        let mut items = Vec::new();
        for backend in &self.backends {
            for (uri, diagnostics) in backend.workspace_diagnostics().await {
                items.push(WorkspaceDocumentDiagnosticReport::Full(
                    WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version: None,
                        full_document_diagnostic_report:
                            FullDocumentDiagnosticReport {
                                result_id: None,
                                items: diagnostics,
                            },
                    },
                ));
            }
        }
        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport { items },
        ))
    }
}

/// Extract workspace roots from an `initialize` request as
/// filesystem paths. Prefers `workspaceFolders` (LSP 3.6+, sent
/// by every modern client) and falls back to `rootUri` for older
/// clients. Both may be absent — e.g. when the editor opens a
/// file with no folder context — in which case we return an
/// empty vec and each file's own `vw.toml` is the only source
/// of dep names, matching the pre-fallback behavior.
fn collect_workspace_roots(
    params: &InitializeParams,
) -> Vec<std::path::PathBuf> {
    if let Some(folders) = params.workspace_folders.as_ref() {
        if !folders.is_empty() {
            return folders
                .iter()
                .filter_map(|f| f.uri.to_file_path().ok())
                .collect();
        }
    }
    // `root_uri` is deprecated but still what a lot of clients
    // (including bare-bones LSP integrations) send.
    #[allow(deprecated)]
    if let Some(uri) = params.root_uri.as_ref() {
        if let Ok(p) = uri.to_file_path() {
            return vec![p];
        }
    }
    Vec::new()
}
