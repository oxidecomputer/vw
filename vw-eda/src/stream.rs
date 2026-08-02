// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! How a backend hands output to its caller while a command is still
//! running.
//!
//! Separate from the request/response protocol because it is not a reply to
//! anything: a synthesis run produces output for minutes before it produces a
//! result, and a caller that only saw the result would have nothing to show
//! for the wait. Every backend streams the same way, so a caller written
//! against one works against another — including one that is not on this
//! machine.

/// Tag attached to each chunk a [`StdoutSink`] receives, so the
/// caller can route it to the right UI lane. The shim's
/// `puts`-interception path always produces [`StreamKind::Stdout`]
/// — user TCL has no way to "label" a write. The PTY-line filter
/// classifies Vivado's standard message format
/// (`ERROR:`/`WARNING:`/`CRITICAL WARNING:`/`INFO:`) into the
/// corresponding kind.
///
/// A consumer that doesn't care (e.g. `vw run` capturing for
/// stdout pass-through) can ignore the kind and treat every chunk
/// identically; the REPL uses it to colour error/warning lines.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// User TCL `puts` output, or any other chunk we don't have a
    /// reason to label otherwise. Default.
    Stdout,
    /// Vivado `INFO:` line — usually low-importance chatter from
    /// the message system.
    Info,
    /// Vivado `WARNING:` line.
    Warning,
    /// Vivado `CRITICAL WARNING:` line. Semantically means "your
    /// run may fail because of this" — Vivado nests this severity
    /// between WARNING and ERROR. Distinct from
    /// [`StreamKind::Error`] so log-level filtering can treat them
    /// separately (`--log-level=error` hides critical warnings;
    /// `--log-level=critical` keeps them).
    CriticalWarning,
    /// Vivado `ERROR:` line. Distinct from the final
    /// [`BackendError::Tcl`] returned by `eval` — these are emitted
    /// *during* an eval and the final error often refers back to
    /// them ("failed due to earlier errors").
    Error,
}

/// Sink for streamed output during an eval. Called once per chunk
/// the worker observes — from the shim's `puts` interception (Tcl
/// user output) or from the PTY-line filter (Vivado's own message
/// system). The [`StreamKind`] tags the chunk so the caller can
/// route warnings and errors to a more attention-grabbing UI
/// surface than ordinary stdout.
pub type StdoutSink = Box<dyn FnMut(StreamKind, &str) + Send>;
