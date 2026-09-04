// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! VHDL [`LanguageBackend`] backed by an embedded
//! [`vhdl_ls::VHDLServer`] per workspace.
//!
//! `vhdl_ls::SharedRpcChannel` wraps `Rc<dyn RpcChannel>` and is
//! therefore `!Send`, so the server can't live in an
//! `Arc<dyn LanguageBackend>` shared across tokio worker threads.
//! We pin each workspace's server to a dedicated `std::thread` and
//! communicate over a synchronous mpsc: the LSP-facing async methods
//! send a typed request + oneshot reply, the worker thread receives
//! it, calls the corresponding `VHDLServer::text_document_*` method,
//! and returns the response.
//!
//! Outbound notifications (`publishDiagnostics`, `logMessage`, etc.)
//! emitted by the wrapped server hit a `TowerLspRpc` bridge that
//! forwards them to the `tower_lsp::Client` on a captured runtime
//! handle. Unknown notification methods are logged and dropped —
//! `vhdl_ls` only emits a small set today (diagnostics + log/show
//! messages) and adding a new one is a rare event.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{oneshot, Mutex as TokioMutex};
use tower_lsp::lsp_types::{
    ClientCapabilities, CompletionItem, Diagnostic,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbol,
    DocumentSymbolParams, DocumentSymbolResponse, FileChangeType,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, Location, LogMessageParams, MessageType,
    PartialResultParams, Position, PublishDiagnosticsParams, ReferenceContext,
    ReferenceParams, RenameParams, ShowMessageParams, SignatureHelp,
    SymbolInformation, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Url, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceEdit,
};
use tower_lsp::Client;
use tracing::warn;

use crate::backend::LanguageBackend;

/// Per-workspace worker owning a `VHDLServer`. Communication happens
/// over a synchronous mpsc; async LSP handlers await responses via
/// `tokio::sync::oneshot`.
struct WorkspaceHandle {
    tx: std::sync::mpsc::Sender<Message>,
    /// Workspace root (the dir holding `vw.toml`) — kept so the
    /// config can be re-rendered when the file set shifts.
    root: Utf8PathBuf,
    /// Every file the *currently installed* config maps into a
    /// library, normalized the same way `vhdl_lang` normalizes
    /// source paths. A `.vhd` that isn't in here is invisible to
    /// the wrapped server's library mapping — it gets analyzed as
    /// a standalone `Source::inline` with no `work` visibility, so
    /// `use work.<pkg>` fails to resolve.
    project_files: StdMutex<HashSet<PathBuf>>,
    /// Paths we already re-rendered for and *still* didn't find —
    /// a scratch `.vhd` outside `hdl/`, or a buffer not yet written
    /// to disk. Keeps every keystroke in such a file from walking
    /// the workspace again. `did_save` re-checks regardless.
    absent: StdMutex<HashSet<PathBuf>>,
    /// Held only for cleanup at drop; joining is best-effort.
    _thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for WorkspaceHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Shutdown);
        if let Some(handle) = self._thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl WorkspaceHandle {
    /// Make sure `path` is part of the wrapped server's project
    /// before its text arrives.
    ///
    /// The config is a *concrete file list* rendered from the
    /// workspace's on-disk enumeration, so a `.vhd` created after
    /// the worker spawned isn't in it. Re-render on first sight of
    /// an unknown path and push the result down, which is what
    /// puts a newly added `hdl/*.vhd` into `defaultlib` and lets
    /// `use work.…` resolve.
    ///
    /// `force` bypasses the [`absent`](Self::absent) memo — used
    /// from `did_save`, where the very event that matters is a
    /// buffer becoming a file on disk (`file_names` only reports
    /// files that exist).
    fn ensure_in_project(&self, path: &Path, force: bool) {
        let path = normalize(path);
        if self.project_files.lock().unwrap().contains(&path) {
            return;
        }
        if !force && !self.absent.lock().unwrap().insert(path.clone()) {
            return;
        }
        let cfg = match vw_lib::render_vhdl_lang_config(&self.root, None) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "vhdl_backend: config re-render failed for {}: {e}",
                    self.root
                );
                return;
            }
        };
        let files = config_file_set(&cfg);
        let found = files.contains(&path);
        if found {
            self.absent.lock().unwrap().remove(&path);
        } else {
            self.absent.lock().unwrap().insert(path);
        }
        // Installing a config rebuilds the whole design root, so
        // only do it when the enumeration actually moved.
        if self.install_files(files) {
            let _ = self.tx.send(Message::UpdateConfig(cfg));
        }
    }

    /// Replace the cached project-file set. Returns true when it
    /// differed from what was already cached — i.e. when the
    /// wrapped server needs the new config.
    fn install_files(&self, files: HashSet<PathBuf>) -> bool {
        let mut cached = self.project_files.lock().unwrap();
        if *cached == files {
            return false;
        }
        // Anything previously written off as absent gets another
        // chance against the new enumeration.
        self.absent.lock().unwrap().retain(|p| !files.contains(p));
        *cached = files;
        true
    }
}

/// Every file the config maps into a library, normalized for
/// comparison against a URI-derived path.
fn config_file_set(cfg: &vhdl_lang::Config) -> HashSet<PathBuf> {
    let mut messages = vhdl_lang::NullMessages;
    cfg.iter_libraries()
        .flat_map(|lib| lib.file_names(&mut messages))
        .map(|p| normalize(&p))
        .collect()
}

fn is_vhdl(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("vhd") | Some("vhdl")
    )
}

/// Absolute-but-not-canonical, matching `vhdl_lang::FilePath`:
/// symlinks are deliberately left unresolved there, so resolving
/// them here would produce paths that never compare equal.
fn normalize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[allow(dead_code)]
enum Message {
    DidOpen(DidOpenTextDocumentParams),
    /// Populated once we wire delta-mode text sync; today the
    /// full-buffer replace path routes through `DidOpen` because
    /// the wrapped server treats duplicates as no-ops.
    DidChange(DidChangeTextDocumentParams),
    DidClose(DidCloseTextDocumentParams),
    Hover(HoverParams, oneshot::Sender<Option<Hover>>),
    GotoDefinition(
        GotoDefinitionParams,
        oneshot::Sender<Option<GotoDefinitionResponse>>,
    ),
    DocumentSymbols(
        DocumentSymbolParams,
        oneshot::Sender<Option<DocumentSymbolResponse>>,
    ),
    Rename(RenameParams, oneshot::Sender<Option<WorkspaceEdit>>),
    References(ReferenceParams, oneshot::Sender<Vec<Location>>),
    /// Pull-based diagnostics for a single file. Since `vhdl_ls`
    /// pushes diagnostics asynchronously through
    /// `publishDiagnostics`, this returns whatever the cache holds
    /// for `uri` at the moment of the request. Callers wanting
    /// fresher results should nudge `did_change` first.
    Diagnostics(Url, oneshot::Sender<Vec<Diagnostic>>),
    /// Ask the worker to swap the wrapped server's config in
    /// place. Triggered by `did_change_watched_files` after the
    /// backend re-renders from live workspace state.
    UpdateConfig(vhdl_lang::Config),
    Shutdown,
}

pub struct VhdlBackend {
    client: Client,
    workspaces: TokioMutex<HashMap<Utf8PathBuf, Arc<WorkspaceHandle>>>,
    /// `Some(handle)` in production; captured at construction so
    /// worker threads can hand outbound notifications back to
    /// tower_lsp via `handle.spawn`. Tests skip this by leaving it
    /// `None` and reading messages out of the RpcMock instead.
    runtime: tokio::runtime::Handle,
}

impl VhdlBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspaces: TokioMutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// Resolve `uri` to a workspace root and (create + bootstrap)
    /// the per-workspace worker if it isn't running yet. Returns
    /// `None` when the URI doesn't sit under a `vw.toml` — the
    /// caller should skip the request rather than fabricate an
    /// answer.
    async fn ensure_workspace(
        &self,
        uri: &Url,
    ) -> Option<Arc<WorkspaceHandle>> {
        let path = uri.to_file_path().ok()?;
        let ws = crate::workspace::find_workspace_dir(&path)?;
        let mut map = self.workspaces.lock().await;
        if let Some(existing) = map.get(&ws) {
            return Some(existing.clone());
        }
        let cfg = match vw_lib::render_vhdl_lang_config(&ws, None) {
            Ok(c) => c,
            Err(e) => {
                warn!("vhdl_backend: config render failed for {ws}: {e}");
                return None;
            }
        };
        // Make sure the VHDL standard library is available — fetched
        // into the dep cache on first use — and point vhdl_ls at it, so
        // a machine without a system rust_hdl install still resolves
        // `ieee` / `std`. `None` (fetch failed, offline + uncached)
        // falls back to vhdl_ls's built-in search of installed
        // locations.
        let stdlib = vw_lib::ensure_vhdl_stdlib()
            .await
            .ok()
            .map(|p| p.to_string());
        let handle = spawn_workspace_worker(
            ws.clone(),
            self.client.clone(),
            self.runtime.clone(),
            cfg,
            stdlib,
        );
        map.insert(ws, handle.clone());
        Some(handle)
    }

    /// Resolve `uri` to its workspace and make sure the file is in
    /// that workspace's project before anything else touches it.
    /// See [`WorkspaceHandle::ensure_in_project`] for why.
    async fn workspace_for_file(
        &self,
        uri: &Url,
        force_refresh: bool,
    ) -> Option<Arc<WorkspaceHandle>> {
        let handle = self.ensure_workspace(uri).await?;
        if let Ok(path) = uri.to_file_path() {
            handle.ensure_in_project(&path, force_refresh);
        }
        Some(handle)
    }
}

fn spawn_workspace_worker(
    root: Utf8PathBuf,
    client: Client,
    runtime: tokio::runtime::Handle,
    initial_config: vhdl_lang::Config,
    stdlib_libraries_path: Option<String>,
) -> Arc<WorkspaceHandle> {
    let (tx, rx) = std::sync::mpsc::channel::<Message>();
    let project_files = config_file_set(&initial_config);
    let worker_root = root.clone();
    let thread = std::thread::spawn(move || {
        workspace_thread(
            worker_root,
            client,
            runtime,
            initial_config,
            stdlib_libraries_path,
            rx,
        );
    });
    Arc::new(WorkspaceHandle {
        tx,
        root,
        project_files: StdMutex::new(project_files),
        absent: StdMutex::new(HashSet::new()),
        _thread: StdMutex::new(Some(thread)),
    })
}

fn workspace_thread(
    root: Utf8PathBuf,
    client: Client,
    runtime: tokio::runtime::Handle,
    initial_config: vhdl_lang::Config,
    stdlib_libraries_path: Option<String>,
    rx: std::sync::mpsc::Receiver<Message>,
) {
    let bridge = TowerLspRpc { client, runtime };
    let rpc = vhdl_ls::SharedRpcChannel::new(Rc::new(bridge));
    let mut server = vhdl_ls::VHDLServer::new_with_config(
        rpc,
        vhdl_ls::VHDLServerSettings {
            non_project_file_handling: vhdl_ls::NonProjectFileHandling::Analyze,
            // Point vhdl_ls at the stdlib vw fetched into the dep cache
            // (`None` → its built-in search of installed locations).
            libraries_path: stdlib_libraries_path,
            ..Default::default()
        },
        initial_config,
    );
    // Synthesize the LSP `initialize` handshake — root_uri anchors
    // vhdl_ls's workspace-scoped file resolution. Client
    // capabilities left at default: minimal is enough since we're
    // not proxying advanced features (semantic tokens, semantic
    // highlighting) through the outer analyzer today.
    #[allow(deprecated)]
    let init_params = InitializeParams {
        process_id: None,
        root_path: None,
        root_uri: Url::from_directory_path(root.as_std_path()).ok(),
        initialization_options: None,
        capabilities: ClientCapabilities::default(),
        trace: None,
        workspace_folders: None,
        client_info: None,
        locale: None,
    };
    server.initialize_request(init_params);
    server.initialized_notification();

    while let Ok(msg) = rx.recv() {
        match msg {
            Message::DidOpen(p) => {
                server.text_document_did_open_notification(&p);
            }
            Message::DidChange(p) => {
                server.text_document_did_change_notification(&p);
            }
            Message::DidClose(_) => {
                // `vhdl_ls::VHDLServer` doesn't expose a
                // `did_close` — the file stays part of the
                // project until an `update_config` drops it.
                // That matches vhdl_ls's stdio behavior.
            }
            Message::Hover(p, reply) => {
                let r = server
                    .text_document_hover(&p.text_document_position_params);
                let _ = reply.send(r);
            }
            Message::GotoDefinition(p, reply) => {
                let r = server
                    .text_document_definition(&p.text_document_position_params)
                    .map(GotoDefinitionResponse::Scalar);
                let _ = reply.send(r);
            }
            Message::DocumentSymbols(p, reply) => {
                let r = server.document_symbol(&p);
                let _ = reply.send(r);
            }
            Message::Rename(p, reply) => {
                let r = server.rename(&p);
                let _ = reply.send(r);
            }
            Message::References(p, reply) => {
                let r = server.text_document_references(&p);
                let _ = reply.send(r);
            }
            Message::UpdateConfig(cfg) => {
                server.set_config(cfg);
            }
            Message::Diagnostics(uri, reply) => {
                // The wrapped server has already emitted
                // `publishDiagnostics` for this file — the pull-
                // based `textDocument/diagnostic` handler in
                // vhdl_ls delegates to the same analyze pipeline.
                // For now we return `[]` and rely on the push
                // path; a follow-up phase can wire the pull-based
                // `text_document_diagnostic` if any editor we
                // support prefers pull.
                let _ = (uri, reply.send(Vec::new()));
            }
            Message::Shutdown => break,
        }
    }
}

/// `vhdl_ls::RpcChannel` implementation that forwards notifications
/// and requests to the outer tower_lsp `Client`.
///
/// Only the notifications `vhdl_ls` actually emits today are wired
/// through — new ones surface as warnings so we notice.
struct TowerLspRpc {
    client: Client,
    runtime: tokio::runtime::Handle,
}

impl vhdl_ls::RpcChannel for TowerLspRpc {
    fn send_notification(&self, method: String, params: serde_json::Value) {
        let client = self.client.clone();
        self.runtime.spawn(async move {
            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    match serde_json::from_value::<PublishDiagnosticsParams>(
                        params,
                    ) {
                        Ok(p) => {
                            client
                                .publish_diagnostics(
                                    p.uri,
                                    p.diagnostics,
                                    p.version,
                                )
                                .await
                        }
                        Err(e) => {
                            warn!("vhdl_backend: bad publishDiagnostics: {e}")
                        }
                    }
                }
                "window/logMessage" => {
                    match serde_json::from_value::<LogMessageParams>(params) {
                        Ok(p) => client.log_message(p.typ, p.message).await,
                        Err(e) => {
                            warn!("vhdl_backend: bad logMessage: {e}")
                        }
                    }
                }
                "window/showMessage" => {
                    match serde_json::from_value::<ShowMessageParams>(params) {
                        Ok(p) => client.show_message(p.typ, p.message).await,
                        Err(e) => {
                            warn!("vhdl_backend: bad showMessage: {e}")
                        }
                    }
                }
                other => warn!(
                    "vhdl_backend: dropping unhandled notification {other}"
                ),
            }
        });
    }

    fn send_request(&self, method: String, _params: serde_json::Value) {
        // `vhdl_ls` sends server→client requests rarely (e.g.
        // `client/registerCapability`). tower_lsp doesn't expose a
        // generic method+Value send, so drop with a warning until
        // one of these actually becomes load-bearing.
        warn!(
            "vhdl_backend: dropping server→client request {method} — \
             not yet bridged"
        );
    }
}

fn text_document_position(
    uri: Url,
    position: Position,
) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri },
        position,
    }
}

#[async_trait]
impl LanguageBackend for VhdlBackend {
    fn language_id(&self) -> &str {
        "vhdl"
    }

    fn handles(&self, uri: &Url) -> bool {
        let path = uri.path();
        path.ends_with(".vhd") || path.ends_with(".vhdl")
    }

    fn pushes_diagnostics(&self) -> bool {
        // vhdl_ls calls `publish_diagnostics` synchronously in
        // `text_document_did_{open,change}_notification`; the
        // outbound `TowerLspRpc` bridges those through the
        // tower_lsp `Client`. The outer server's pull path would
        // race and (with our current empty `diagnostics()` stub)
        // clobber those real diagnostics with an empty vec — so
        // opt out of it entirely.
        true
    }

    async fn set_text(&self, uri: Url, text: String) {
        // `force = false`: this runs on every keystroke, and the
        // memo in `ensure_in_project` keeps all but the first sight
        // of an unknown path free.
        let Some(ws) = self.workspace_for_file(&uri, false).await else {
            return;
        };
        // vhdl_ls doesn't distinguish "first open" from
        // "subsequent change" at the API level — sending
        // `did_open` every set_text call is safe (the server
        // ignores duplicates) and simpler than tracking per-URI
        // state ourselves. The alternative — sending `did_change`
        // with a full-buffer replace — would produce an
        // equivalent effect but leaves us on the hook for
        // synthesizing valid version numbers.
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "vhdl".into(),
                version: 0,
                text,
            },
        };
        let _ = ws.tx.send(Message::DidOpen(params));
    }

    async fn save(&self, uri: &Url) {
        // A brand-new buffer only becomes a project file once it
        // exists on disk — `LibraryConfig::file_names` skips paths
        // that don't. Force past the absent-memo so the first write
        // of `hdl/new.vhd` pulls it into `defaultlib`.
        self.workspace_for_file(uri, true).await;
    }

    async fn did_change_watched_files(
        &self,
        params: &DidChangeWatchedFilesParams,
    ) {
        // Which workspaces are affected? Walk each event's URI up
        // to its nearest `vw.toml` and dedup — one config re-render
        // per workspace, not per file. Deleting a `vw.toml` shows
        // up as `Deleted` events whose containing dir *may* still
        // be a workspace (parent `vw.toml`) or *may* not — either
        // way, re-render is safe: it either produces a valid
        // config for the surviving workspace or a `render_...`
        // error we log and ignore.
        let mut affected: std::collections::HashSet<Utf8PathBuf> =
            std::collections::HashSet::new();
        for change in &params.changes {
            let Ok(path) = change.uri.to_file_path() else {
                continue;
            };
            // A VHDL source only moves the config when it appears
            // or disappears — its *contents* reach the server
            // through `did_change`, and re-rendering on every save
            // would rebuild the whole design root for nothing.
            // Vivado writing a few thousand generated `.vhd`s
            // under `target/` is the case that makes this matter.
            if is_vhdl(&path) && change.typ == FileChangeType::CHANGED {
                continue;
            }
            if let Some(ws) = crate::workspace::find_workspace_dir(&path) {
                affected.insert(ws);
            }
        }
        if affected.is_empty() {
            return;
        }
        let map = self.workspaces.lock().await;
        for ws in affected {
            let Some(handle) = map.get(&ws) else {
                // Workspace has no live server (no `.vhd` opened
                // yet). Nothing to reload — next `.vhd` open will
                // build a fresh server from the current state.
                continue;
            };
            match vw_lib::render_vhdl_lang_config(&ws, None) {
                Ok(cfg) => {
                    // Keep the membership cache in step with what
                    // the server is about to be told, so a file
                    // this event just added doesn't trigger a
                    // second re-render when it's opened.
                    handle.install_files(config_file_set(&cfg));
                    let _ = handle.tx.send(Message::UpdateConfig(cfg));
                }
                Err(e) => {
                    warn!(
                        "vhdl_backend: config re-render failed for \
                         {ws}: {e}"
                    );
                }
            }
        }
    }

    async fn close(&self, uri: &Url) {
        let Some(ws) = self.ensure_workspace(uri).await else {
            return;
        };
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
        };
        let _ = ws.tx.send(Message::DidClose(params));
    }

    async fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        let Some(ws) = self.ensure_workspace(uri).await else {
            return Vec::new();
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::Diagnostics(uri.clone(), tx)).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    async fn document_symbols(&self, uri: &Url) -> Vec<DocumentSymbol> {
        let Some(ws) = self.ensure_workspace(uri).await else {
            return Vec::new();
        };
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::DocumentSymbols(params, tx)).is_err() {
            return Vec::new();
        }
        match rx.await.unwrap_or(None) {
            Some(DocumentSymbolResponse::Nested(v)) => v,
            // vhdl_ls always returns Nested; the Flat branch is
            // only reached if the wrapper starts negotiating with
            // the client. Convert defensively.
            Some(DocumentSymbolResponse::Flat(_)) => Vec::new(),
            None => Vec::new(),
        }
    }

    async fn workspace_symbols(&self, _query: &str) -> Vec<SymbolInformation> {
        // Not yet wired — `VHDLServer::workspace_symbol` needs the
        // query threaded through; add in a follow-up.
        Vec::new()
    }

    async fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let ws = self.ensure_workspace(uri).await?;
        let params = HoverParams {
            text_document_position_params: text_document_position(
                uri.clone(),
                position,
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::Hover(params, tx)).is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    async fn goto_definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<Location> {
        let Some(ws) = self.ensure_workspace(uri).await else {
            return Vec::new();
        };
        let params = GotoDefinitionParams {
            text_document_position_params: text_document_position(
                uri.clone(),
                position,
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::GotoDefinition(params, tx)).is_err() {
            return Vec::new();
        }
        match rx.await.unwrap_or(None) {
            Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(GotoDefinitionResponse::Array(v)) => v,
            Some(GotoDefinitionResponse::Link(_)) | None => Vec::new(),
        }
    }

    async fn completion(
        &self,
        _uri: &Url,
        _position: Position,
    ) -> Vec<CompletionItem> {
        // Wire in Phase 3b — `VHDLServer::request_completion` takes
        // `CompletionParams` and we need to bridge the returned
        // `CompletionList`.
        Vec::new()
    }

    async fn signature_help(
        &self,
        _uri: &Url,
        _position: Position,
    ) -> Option<SignatureHelp> {
        // vhdl_ls's signature help is limited; wire in a follow-up.
        None
    }

    async fn rename(
        &self,
        uri: &Url,
        position: Position,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let ws = self.ensure_workspace(uri).await?;
        let params = RenameParams {
            text_document_position: text_document_position(
                uri.clone(),
                position,
            ),
            new_name: new_name.to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::Rename(params, tx)).is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    async fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some(ws) = self.ensure_workspace(uri).await else {
            return Vec::new();
        };
        // `include_declaration` is passed through verbatim rather
        // than post-filtered: `vhdl_ls` builds its answer from
        // `Project::find_all_references`, which always includes the
        // declaration's own span, and the embedding API gives us no
        // way to tell which returned `Location` is the decl. Vanilla
        // vhdl_ls has the same behavior — matching it keeps the
        // proxy honest instead of guessing at a span to drop.
        let params = ReferenceParams {
            text_document_position: text_document_position(
                uri.clone(),
                position,
            ),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };
        let (tx, rx) = oneshot::channel();
        if ws.tx.send(Message::References(params, tx)).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }
}

/// Suppress a false-positive dead-code warning: `VersionedTextDocumentIdentifier`
/// is currently only reachable if we ever fill out did_change params
/// with version numbers; keep the import to signal the surface.
#[allow(dead_code)]
fn _placeholder(v: VersionedTextDocumentIdentifier) {
    let _ = v;
}

/// Marker type: outbound `window/showMessage`s coming in as
/// `MessageType::WARNING` etc. are what we surface at the top of
/// `TowerLspRpc::send_notification`. Kept here so the `use` stays
/// referenced across builds.
#[allow(dead_code)]
const _MSG_TYPES: &[MessageType] = &[
    MessageType::ERROR,
    MessageType::WARNING,
    MessageType::INFO,
    MessageType::LOG,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    const VW_TOML: &str = "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n\
                           [dependencies]\n[test-dependencies]\n";

    /// A workspace handle with no worker thread behind it —
    /// `Message`s pile up in the returned receiver instead, which
    /// is exactly what these tests want to inspect.
    fn handle_for(ws: &Utf8PathBuf) -> (WorkspaceHandle, Receiver<Message>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg = vw_lib::render_vhdl_lang_config(ws, None).unwrap();
        let handle = WorkspaceHandle {
            tx,
            root: ws.clone(),
            project_files: StdMutex::new(config_file_set(&cfg)),
            absent: StdMutex::new(HashSet::new()),
            _thread: StdMutex::new(None),
        };
        (handle, rx)
    }

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), VW_TOML).unwrap();
        std::fs::create_dir_all(ws.join("hdl")).unwrap();
        std::fs::write(ws.join("hdl/existing.vhd"), "").unwrap();
        (tmp, ws)
    }

    fn took_update_config(rx: &Receiver<Message>) -> bool {
        matches!(rx.try_recv(), Ok(Message::UpdateConfig(_)))
    }

    /// The bug: a `.vhd` created after the workspace's server
    /// started isn't in its rendered config, so it lands outside
    /// every library and `use work.<pkg>` can't resolve. Opening
    /// it has to re-render first.
    #[test]
    fn new_file_pulls_in_a_fresh_config() {
        let (_tmp, ws) = workspace();
        let (handle, rx) = handle_for(&ws);

        let added = ws.join("hdl/added.vhd");
        std::fs::write(&added, "").unwrap();
        handle.ensure_in_project(added.as_std_path(), false);

        assert!(
            took_update_config(&rx),
            "opening a newly created source must push a re-rendered config"
        );
        assert!(handle
            .project_files
            .lock()
            .unwrap()
            .contains(&normalize(added.as_std_path())));
    }

    /// A file already in the config is the common case — every
    /// keystroke goes through here, so it must not re-render.
    #[test]
    fn known_file_does_not_re_render() {
        let (_tmp, ws) = workspace();
        let (handle, rx) = handle_for(&ws);

        handle.ensure_in_project(
            ws.join("hdl/existing.vhd").as_std_path(),
            false,
        );

        assert!(rx.try_recv().is_err(), "no config churn for a known file");
    }

    /// An unsaved buffer isn't on disk, so no re-render can find
    /// it (`file_names` only reports files that exist). The memo
    /// keeps the miss cheap; `did_save`'s forced re-check is what
    /// picks the file up once it lands.
    #[test]
    fn absent_file_is_memoized_until_saved() {
        let (_tmp, ws) = workspace();
        let (handle, rx) = handle_for(&ws);
        let pending = ws.join("hdl/pending.vhd");

        handle.ensure_in_project(pending.as_std_path(), false);
        assert!(
            rx.try_recv().is_err(),
            "nothing on disk, nothing to install"
        );

        // File appears — but an unforced check is memoized away.
        std::fs::write(&pending, "").unwrap();
        handle.ensure_in_project(pending.as_std_path(), false);
        assert!(
            rx.try_recv().is_err(),
            "unforced re-check must stay memoized"
        );

        // `did_save`'s forced path is the one that notices.
        handle.ensure_in_project(pending.as_std_path(), true);
        assert!(took_update_config(&rx), "save must re-check and install");
        assert!(handle
            .project_files
            .lock()
            .unwrap()
            .contains(&normalize(pending.as_std_path())));
    }

    /// `did_change_watched_files` installs the new enumeration
    /// directly; a file it just added must not trigger a second
    /// re-render when the editor opens it.
    #[test]
    fn install_files_clears_the_absent_memo() {
        let (_tmp, ws) = workspace();
        let (handle, rx) = handle_for(&ws);
        let late = ws.join("hdl/late.vhd");

        handle.ensure_in_project(late.as_std_path(), false);
        assert!(handle
            .absent
            .lock()
            .unwrap()
            .contains(&normalize(late.as_std_path())));

        std::fs::write(&late, "").unwrap();
        let cfg = vw_lib::render_vhdl_lang_config(&ws, None).unwrap();
        assert!(handle.install_files(config_file_set(&cfg)));

        handle.ensure_in_project(late.as_std_path(), false);
        assert!(rx.try_recv().is_err(), "already installed — no re-render");
        assert!(handle.absent.lock().unwrap().is_empty());
    }
}
