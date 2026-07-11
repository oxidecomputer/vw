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
    SignatureHelp, SymbolInformation, Url, WorkspaceEdit,
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

    /// Trigger an immediate re-index of `uri` — no debounce, no
    /// wait. Called from `did_save` so `Ctrl-s` in the editor
    /// forces a fresh diagnostic sweep even after a small edit
    /// that hadn't yet reached the `set_text` debounce window.
    /// Backends without a debounce can leave this as the no-op
    /// default: their `set_text` already committed synchronously.
    async fn save(&self, _uri: &Url) {}

    /// Block until the next full re-index / re-analysis of `uri`
    /// commits. Called by the server AFTER `set_text` from a
    /// detached task so it can wrap the wait in an LSP
    /// `window/workDoneProgress` notification — that's how the
    /// editor's "indexing…" spinner comes back on.
    ///
    /// Default implementation returns immediately (backends without
    /// a background reindex signal don't have anything to wait on;
    /// the progress spinner just flashes briefly and disappears).
    /// Backends that do have a background rebuild (like
    /// [`crate::HtclBackend`]) override this to await their
    /// indexer's commit notification.
    async fn wait_for_reindex(&self, _uri: &Url) {}

    /// Editor-supplied workspace roots (from LSP `rootUri` /
    /// `workspaceFolders`, plus updates via
    /// `didChangeWorkspaceFolders`). Backends may use them as
    /// fallback dep-lookup sources when a file being analyzed sits
    /// outside the nearest `vw.toml` — e.g. a goto-def landed the
    /// user in a dep cache dir whose own workspace doesn't declare
    /// the same deps as the editor's root. Default impl is a no-op
    /// because most backends won't care.
    async fn set_workspace_roots(&self, _roots: Vec<std::path::PathBuf>) {}

    /// Forget any state for `uri`.
    async fn close(&self, uri: &Url);

    /// Diagnostics for the current text of `uri`. The server pushes
    /// these to the editor via `textDocument/publishDiagnostics` after
    /// every text update.
    async fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic>;

    /// Workspace-wide diagnostics, keyed by file URI. The server
    /// serves these to the editor via `workspace/diagnostic` (LSP
    /// 3.17 pull-based workspace diagnostics — Helix's `space-D`
    /// picker consumes them).
    ///
    /// Backends compute these from the workspace view attached to
    /// every open document — a validator diagnostic that landed in
    /// an imported file's region gets routed back to that file's
    /// URI. Files that no open document transitively `src`s stay
    /// silent; that's a soft edge of the model but a good default
    /// (an isolated file with no reachable entry is unlikely to be
    /// what the user is looking for).
    ///
    /// Default impl returns empty so backends without workspace-
    /// diagnostic support don't have to stub it out.
    async fn workspace_diagnostics(&self) -> Vec<(Url, Vec<Diagnostic>)> {
        Vec::new()
    }

    /// Document symbols ("outline view") for `uri`.
    async fn document_symbols(&self, uri: &Url) -> Vec<DocumentSymbol>;

    /// Workspace-wide symbol search for the LSP `workspace/symbol`
    /// request. `query` is the user's filter text; the backend should
    /// return at minimum every symbol whose name matches `query` (case-
    /// insensitive substring is fine — the editor applies its own fuzzy
    /// scoring on top). An empty `query` should return every symbol the
    /// backend knows about (capped at a sensible upper bound to keep
    /// the response small enough to render).
    async fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation>;

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

    /// Compute the workspace edit that renames the identifier at
    /// `position` to `new_name`. `None` when the cursor isn't on a
    /// renamable symbol, when `new_name` is invalid for the target
    /// language, or when the rename would need to touch symbols the
    /// backend can't safely reach (e.g. cross-file references).
    ///
    /// Default `None` so language backends without rename support
    /// (or where rename hasn't landed yet) don't have to stub the
    /// method out; the server treats the response as "not
    /// supported here" and the editor shows an unobtrusive error.
    async fn rename(
        &self,
        _uri: &Url,
        _position: Position,
        _new_name: &str,
    ) -> Option<WorkspaceEdit> {
        None
    }

    /// All locations that reference the symbol at `position`,
    /// including `position`'s own location. `include_declaration`
    /// mirrors the LSP `ReferenceContext` flag — when `false` the
    /// backend should omit the decl span from the response.
    ///
    /// Returns an empty vec when the cursor isn't on a known
    /// symbol. Default impl returns empty so backends without a
    /// references implementation don't have to stub it out.
    async fn references(
        &self,
        _uri: &Url,
        _position: Position,
        _include_declaration: bool,
    ) -> Vec<Location> {
        Vec::new()
    }
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
