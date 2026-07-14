// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Vivado [`EdaBackend`](vw_eda::EdaBackend) implementation.
//!
//! Spawns `vivado -mode tcl` as a long-lived worker, sources the
//! embedded shim TCL file at startup, and exchanges newline-delimited
//! JSON with it over stdio. Resolution order for the `vivado`
//! executable is: `VW_VIVADO` env var, then `PATH` lookup. v0 supports
//! the `eval` op only; structured ops land in phase 4.

mod handlers;
mod raw_log;
mod rpc;
pub mod stream;
mod worker;

pub use handlers::{make_handler, make_handler_with_variant};
pub use raw_log::raw_log_path_for_workspace;
pub use rpc::{FnHandler, RpcHandler};
pub use stream::{
    severity_of, stream_kind_for, Block, BlockAccumulator, LogLevel, Severity,
};
pub use worker::{AutoProject, StreamKind, VivadoBackend, VivadoConfig};
