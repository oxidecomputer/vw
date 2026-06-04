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
        _params: InitializeParams,
    ) -> Result<InitializeResult> {
        info!("vw-analyzer initializing");
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
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("vw-analyzer initialized");
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
}
