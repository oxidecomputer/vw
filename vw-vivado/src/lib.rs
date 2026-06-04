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

mod worker;

pub use worker::{VivadoBackend, VivadoConfig};
