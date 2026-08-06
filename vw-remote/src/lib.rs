// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Running a build on a machine that is not this one.
//!
//! The split is between what depends on the source you are editing and what
//! depends on the tree being built. Parsing `design.htcl`, lowering it to Tcl
//! and attributing a diagnostic back to a line number all belong to the first
//! and stay on the developer's machine, which is why an error still points at
//! a file they can open. Spawning Vivado, answering `vw::vhdl_design_sources`
//! and deciding whether a checkpoint is still good all belong to the second
//! and happen on the instance, because that is where the files and the
//! `target/` directory are.
//!
//! In between is the protocol `vw-eda` already defined for talking to a local
//! worker, carried over a websocket instead of a pipe. It streams because it
//! always streamed.

mod backend;
pub mod bench;
pub mod driver;
pub mod protocol;
mod session;

pub use backend::{InterruptHandle, NoteSink, RemoteBackend};
pub use bench::BenchEvent;
pub use driver::{BuildParams, DriverEvent};
pub use protocol::{SessionEvent, SessionParams, SessionRequest};
pub use session::{serve, workspace_root, SessionError};

/// An error and everything under it, as one line.
///
/// Every failure on an instance reaches the developer as a `String` in an
/// event — there is a process and a network between the two, and a `dyn Error`
/// does not cross either. `to_string()` alone keeps only the outermost
/// message, which for a wrapper like "generating anodizer structs" is the one
/// part that says nothing about what went wrong.
///
/// So the chain is flattened here, before it is sent, because after that there
/// is nothing left to walk.
pub fn causes(error: &dyn std::error::Error) -> String {
    slog_error_chain::InlineErrorChain::new(error).to_string()
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("the disk is full")]
    struct Bottom;

    #[derive(Debug, thiserror::Error)]
    #[error("writing the scratch library")]
    struct Middle(#[source] Bottom);

    #[derive(Debug, thiserror::Error)]
    #[error("generating anodizer structs")]
    struct Top(#[source] Middle);

    /// The reported message has to carry what went wrong, not just which step
    /// it happened in. `generating anodizer structs` on its own is the exact
    /// error this exists to stop being the whole story.
    #[test]
    fn a_wrapper_reports_what_actually_failed() {
        assert_eq!(
            causes(&Top(Middle(Bottom))),
            "generating anodizer structs: writing the scratch library: \
             the disk is full",
        );
    }

    /// An error with nothing under it reads exactly as it always did.
    #[test]
    fn an_error_with_no_cause_is_unchanged() {
        assert_eq!(causes(&Bottom), "the disk is full");
    }
}
