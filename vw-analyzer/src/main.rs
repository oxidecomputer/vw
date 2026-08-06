// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw-analyzer` binary entry point.
//!
//! Spawns the LSP server on stdio. The editor (or `vw analyzer`
//! subcommand) exec's this binary directly.

#[tokio::main]
async fn main() {
    // Silent by default — Helix and most LSP clients flag any stderr
    // output from a language server as an error. Opt in with
    // `VW_ANALYZER_LOG=info` (or `debug`/`trace`) for development.
    // ANSI off so colors don't show up as escape codes in the
    // client's log viewer.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VW_ANALYZER_LOG")
                .unwrap_or_else(|_| "vw_analyzer=off".into()),
        )
        .init();

    vw_analyzer::run_stdio().await;
}
