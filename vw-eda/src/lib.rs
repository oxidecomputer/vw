// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! EDA backend abstraction.
//!
//! Defines the trait that every vendor-specific TCL worker implements
//! and the wire protocol used to talk to it. `vw-vivado` is the first
//! implementation; future `vw-quartus` / `vw-synopsys` crates will
//! implement the same trait, and consumers (`vw run`, `vw repl`, the
//! analyzer) talk only to this abstraction.
//!
//! The protocol is intentionally small: newline-delimited JSON
//! requests, one response per request, monotonic IDs. See the project
//! plan's "Wire protocol" section for the design rationale.

pub mod protocol;
pub mod stream;

use async_trait::async_trait;
use thiserror::Error;

pub use protocol::{
    ErrorPayload, Request, RequestOp, Response, ResponseResult, StreamMessage,
    WireMessage,
};
pub use stream::{StdoutSink, StreamKind};

/// Errors returned by an [`EdaBackend`].
#[derive(Debug, Error)]
pub enum BackendError {
    /// The worker process exited or could not be started.
    #[error("worker process error: {0}")]
    Worker(String),

    /// I/O error while reading or writing the wire protocol.
    #[error("wire I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Wire protocol message could not be serialized or parsed.
    #[error("wire protocol error: {0}")]
    Protocol(#[from] serde_json::Error),

    /// The backend reported a TCL-level error in response to a command.
    /// `stdout` carries any output the command produced before erroring,
    /// so callers can show context.
    #[error("TCL error: {message}")]
    Tcl {
        message: String,
        code: Option<String>,
        info: Option<String>,
        stdout: String,
    },

    /// Catch-all for backend-specific failures.
    #[error("{0}")]
    Other(String),
}

/// Result of an [`EdaBackend::eval`] call.
#[derive(Clone, Debug, Default)]
pub struct EvalOutput {
    /// The TCL expression's return value, as a string.
    pub value: String,
    /// stdout captured during this eval (puts to stdout from the user
    /// TCL while the shim's capturing flag was set). Always present
    /// and may be empty; trailing newlines are preserved as written.
    pub stdout: String,
}

/// A long-lived TCL worker driven by `vw`.
///
/// Implementations spawn the vendor process (Vivado, Quartus, ...),
/// inject a small shim that speaks the wire protocol, and translate
/// [`Request`]s into [`Response`]s. The trait is intentionally narrow:
/// callers issue commands, the backend runs them, and the protocol is
/// the contract.
#[async_trait]
pub trait EdaBackend: Send {
    /// Human-readable backend name, e.g. `"vivado"`.
    fn name(&self) -> &str;

    /// Evaluate a TCL command string and return its result plus any
    /// stdout the command produced.
    ///
    /// Equivalent to issuing a [`RequestOp::Eval`] request and
    /// extracting both the return value and the captured-puts payload.
    /// Most callers should use this in preference to
    /// [`EdaBackend::send`] until the structured-eval machinery
    /// (phase 4) lands.
    async fn eval(&mut self, tcl: &str) -> Result<EvalOutput, BackendError>;

    /// Issue an arbitrary request and return the raw response.
    ///
    /// The default implementation in concrete backends is the
    /// preferred place to add `eval_structured` and future ops without
    /// changing the trait surface.
    async fn send(
        &mut self,
        request: Request,
    ) -> Result<Response, BackendError>;

    /// Install a sink called once per chunk of output as it is produced,
    /// rather than after the command finishes.
    ///
    /// On the trait rather than on one backend because it is the only way a
    /// caller can show a long command's progress, and a caller should not have
    /// to know which backend it is driving to get that. With a sink set,
    /// chunks are not also accumulated into [`EvalOutput::stdout`] — the sink
    /// owns them.
    fn set_stdout_sink(&mut self, sink: StdoutSink);

    /// Cleanly shut the worker down. Backends should make this
    /// idempotent so that `Drop` can fall back to it without
    /// double-shutdown errors.
    async fn shutdown(&mut self) -> Result<(), BackendError>;
}
