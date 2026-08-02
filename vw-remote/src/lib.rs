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
pub mod protocol;
mod session;

pub use backend::{NoteSink, RemoteBackend};
pub use protocol::{SessionEvent, SessionParams, SessionRequest};
pub use session::{serve, workspace_root, SessionError};
