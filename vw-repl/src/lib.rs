// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Interactive REPL for htcl scripts.
//!
//! A ratatui-driven shell that talks to a long-lived Vivado worker
//! via [`vw_vivado::VivadoBackend`]. The session document model
//! (every successful eval appended to an in-memory script + the
//! current input as its tail) lets the analyzer power the same
//! features the LSP gives editors — completion, hover, signature
//! help — without any REPL-specific machinery.
//!
//! v1 (this slice) ships the foundation: screen layout, multi-line
//! input with Readline-quality editing, persistent history with
//! Ctrl-R search, a long-lived Vivado worker, and `:load <file>`.
//! Tab completion, signature help, hover overlay, command palette,
//! and structured-result rendering layer on top in subsequent
//! slices.

mod app;
mod highlight;
mod highlight_htcl;
mod history;
pub mod lower;
mod popup;
mod render;
mod session;
mod symbol_index;
mod symbol_search;
pub mod trace;
mod ui;

use camino::Utf8PathBuf;
use thiserror::Error;

pub use app::App;
pub use lower::{build_proc_locations, Origin, OriginFrame, ProcLocation};
pub use session::Session;
pub use trace::{
    display_path, resolve_stack_frames_with, rewrite_stack_line, RewrittenFrame,
};

#[derive(Debug, Error)]
pub enum ReplError {
    #[error("terminal I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(#[from] vw_eda::BackendError),
}

/// Tunable knobs supplied by the CLI invocation.
#[derive(Clone, Debug, Default)]
pub struct ReplOptions {
    /// Forward Vivado's banner / info messages to the scrollback
    /// rather than swallowing them. Useful for diagnosing a slow or
    /// misbehaving worker; off by default to keep the log focused
    /// on the user's evals.
    pub verbose: bool,
    /// If set, source this file into the session immediately after
    /// the Vivado worker comes up. Equivalent to typing `:load
    /// <path>` as the first input.
    pub initial_load: Option<Utf8PathBuf>,
    /// If true, INFO-severity Vivado messages carry their full Tcl
    /// stack frames into the scrollback. Off by default — INFO is
    /// noisy enough without stack traces — but useful when diagnosing
    /// where a particular INFO is emitted from.
    pub info_with_stack: bool,
}

/// Run the REPL until the user exits. Owns the terminal alternate
/// screen for the duration; restores it on every exit path.
pub async fn run(opts: ReplOptions) -> Result<(), ReplError> {
    app::run(opts).await
}
