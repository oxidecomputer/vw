// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! [`LanguageBackend`] — per-language analysis surface consumed by the
//! LSP server.
//!
//! Even though only [`HtclBackend`](crate::HtclBackend) exists today,
//! defining the trait from day one is the architectural commitment
//! described in the project plan: VHDL via a `vhdl_ls` proxy (phase 5)
//! and a future direct Oxide-VHDL-frontend integration both slot in as
//! additional implementations without changing the server or
//! cross-language htcl code.

use async_trait::async_trait;
use tower_lsp::lsp_types::{
    CompletionItem, Diagnostic, DocumentSymbol, Hover, Location, Position,
    SignatureHelp, Url,
};

#[async_trait]
pub trait LanguageBackend: Send + Sync {
    /// Language id (`"htcl"`, `"vhdl"`, ...) — used for tracing and
    /// dispatch.
    fn language_id(&self) -> &str;

    /// Whether this backend handles the given file. Default: match by
    /// extension.
    fn handles(&self, uri: &Url) -> bool;

    /// Update the backend's view of `uri`'s contents. Called on
    /// `did_open` and every `did_change`. The backend should treat
    /// this as the new authoritative source and may eagerly compute
    /// (and cache) analysis results.
    async fn set_text(&self, uri: Url, text: String);

    /// Forget any state for `uri`.
    async fn close(&self, uri: &Url);

    /// Diagnostics for the current text of `uri`. The server pushes
    /// these to the editor via `textDocument/publishDiagnostics` after
    /// every text update.
    async fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic>;

    /// Document symbols ("outline view") for `uri`.
    async fn document_symbols(&self, uri: &Url) -> Vec<DocumentSymbol>;

    /// Hover content for the construct at `position`. Returns `None`
    /// if the cursor isn't on anything the backend has something to
    /// say about.
    async fn hover(&self, uri: &Url, position: Position) -> Option<Hover>;

    /// Definition site for the reference at `position`. Returns
    /// `None` if the cursor isn't on a known reference. Returns
    /// possibly multiple locations because, in general, a name may
    /// have several defining sites (overloads, conditional
    /// definitions); the Phase 2 htcl backend only returns one.
    async fn goto_definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<Location>;

    /// Completion items for the cursor at `position`. Empty when the
    /// backend has nothing to offer in that context.
    async fn completion(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<CompletionItem>;

    /// Signature help for the call enclosing `position`. `None` when
    /// the cursor isn't inside a call the backend recognizes.
    async fn signature_help(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<SignatureHelp>;
}

/// A symbol surfaced from a backend, language-neutral. Backends that
/// need richer fields can build [`DocumentSymbol`] directly; this is
/// a convenience for the common cases that fit a flat name+kind+span.
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: tower_lsp::lsp_types::SymbolKind,
    pub detail: Option<String>,
    pub range: tower_lsp::lsp_types::Range,
    pub selection_range: tower_lsp::lsp_types::Range,
}
