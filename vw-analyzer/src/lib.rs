// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw analyzer` — multi-language LSP for the vw HDL workflow.
//!
//! The server is built around a [`LanguageBackend`] abstraction even
//! while only [`HtclBackend`] is wired up. This keeps the architectural
//! slot for VHDL (initially a `vhdl_ls` proxy, later a direct Oxide
//! VHDL frontend integration) open from day one — see the project
//! plan's "LSP design" section.

mod backend;
mod htcl_backend;
mod server;
mod src_complete;
mod vhdl_backend;
mod workspace;

pub use backend::{LanguageBackend, SymbolInfo};
pub use htcl_backend::HtclBackend;
pub use server::Analyzer;
pub use vhdl_backend::VhdlBackend;

use tower_lsp::{LspService, Server};

/// Run the LSP server on stdio. Returns when the editor disconnects.
///
/// Both the standalone `vw-analyzer` binary and the `vw analyzer`
/// subcommand call this so the editor sees identical behavior either
/// way.
pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Analyzer::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
