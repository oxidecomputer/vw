// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! LSP server entry point. Owns the per-language backends and
//! dispatches `textDocument/*` requests by URI.

use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{debug, info};

use crate::backend::LanguageBackend;
use crate::htcl_backend::HtclBackend;

pub struct Analyzer {
    client: Client,
    backends: Vec<Arc<dyn LanguageBackend>>,
}

impl Analyzer {
    pub fn new(client: Client) -> Self {
        let backends: Vec<Arc<dyn LanguageBackend>> =
            vec![Arc::new(HtclBackend::new())];
        Self { client, backends }
    }

    fn backend_for(&self, uri: &Url) -> Option<Arc<dyn LanguageBackend>> {
        self.backends.iter().find(|b| b.handles(uri)).cloned()
    }

    async fn publish_diagnostics(&self, uri: Url, version: Option<i32>) {
        let Some(backend) = self.backend_for(&uri) else {
            return;
        };
        let diags = backend.diagnostics(&uri).await;
        self.client.publish_diagnostics(uri, diags, version).await;
    }
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
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("vw-analyzer initialized");
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
        self.publish_diagnostics(uri, version).await;
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
        self.publish_diagnostics(uri, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(backend) = self.backend_for(&uri) {
            backend.close(&uri).await;
        }
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
