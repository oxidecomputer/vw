// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! IP-XACT → htcl wrapper generation.
//!
//! Reads an IP-XACT component description (via the `ipxact` crate) and
//! emits an htcl instantiation proc for it — the "configuration
//! interface" layer described in the project plan: one top-level proc
//! per IP, with sub-procs for parameter groups when an IP's surface is
//! too large for a single proc to be tractable (CPM5 has ~8700
//! parameters).
//!
//! Group recovery: IP-XACT itself carries no grouping metadata for
//! large Xilinx IPs (no `<configGroup>` etc., and the `xgui/*.tcl`
//! files that *do* carry the UI grouping are encrypted). Instead, we
//! derive groups from the convention Xilinx uses in parameter naming —
//! `CPM_PCIE0_*`, `CPM_PCIE1_*`, `PS_PMC_*` and so on are clear
//! prefix clusters. See [`group_parameters`].

pub mod cips_dict;
pub mod family;
pub mod generate;
pub mod group;
pub mod overrides;
pub mod paired_list;
pub mod presets;
pub mod summary;
pub mod targets;
pub mod tree;

pub use cips_dict::{
    load_schemas as load_cips_dict_schemas, DictField, DictSchema,
};
pub use family::{detect_families, DetectOptions, IndexedFamily};
pub use generate::{generate, GenerateOptions};
pub use group::{group_parameters, ParameterGroup};
pub use presets::{
    discover_for as discover_presets, load_files as load_presets, PresetMap,
};
pub use summary::Summary;
pub use tree::{build_tree, Node, TreeOptions};

use std::path::Path;

use ipxact::Component;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("loading IP-XACT component: {0}")]
    Ipxact(#[from] ipxact::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Load an IP-XACT component from disk.
pub fn load(path: impl AsRef<Path>) -> Result<Component> {
    Ok(Component::from_file(path)?)
}
