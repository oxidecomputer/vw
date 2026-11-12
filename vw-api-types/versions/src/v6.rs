//! Version `ANODIZE_PHASE` of the vw APIs.
//!
//! Anodization used to be something that happened on the way to running the
//! benches, and the generated Rust stayed on the instance that ran it. The
//! driver now builds against it too, on a different instance — so
//! anodization became a phase of its own: skipped when its output is already
//! current, and published to the environment's artifact store for the helios
//! instance to build with.
//!
//! The output is one for the workspace, not one per variant: a workspace has
//! many HDL variants but one driver. Anodizing now says whether to regenerate
//! output that is already current. The service tells the helios instance
//! where the vivado instance's output is kept, which no client gets a say in,
//! so the instance's own driver query differs from the developer's, which is
//! unchanged.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{v1, v4};

/// What to anodize, and what to check the result against.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct AnodizeQuery {
    /// A bench crate to compile against what was generated.
    ///
    /// Absent runs the generator and stops there.
    #[serde(default)]
    pub bench: Option<String>,
    /// The VHDL standard, as `nvc` spells it. Absent lets the instance use
    /// its own default.
    #[serde(default)]
    pub standard: Option<String>,
    /// Regenerate even when the output is already current.
    ///
    /// Off for a build, which only needs the output to be current. On for
    /// working on the anodizer, where watching it run is the point.
    #[serde(default)]
    pub force: bool,
}

impl From<v4::AnodizeQuery> for AnodizeQuery {
    /// What a client from before this version asked for: `vw cosim anodize`,
    /// which always regenerated.
    fn from(old: v4::AnodizeQuery) -> Self {
        AnodizeQuery {
            bench: old.bench,
            standard: old.standard,
            force: true,
        }
    }
}

/// How the service asks a helios instance to build the driver.
///
/// The developer's [`v1::DriverBuildQuery`], plus what only the service knows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentDriverBuildQuery {
    /// Build with optimizations.
    #[serde(default)]
    pub release: bool,
    /// Anything else to put on cargo's command line, separated by spaces.
    #[serde(default)]
    pub args: Option<String>,
    /// The bucket the vivado instance publishes this workspace's anodizer
    /// output to.
    ///
    /// In the same store as the instance's own artifact target and opened by
    /// the same key — the store has one key per environment, allowed on each
    /// of its buckets — so a name is all the instance needs. Absent when the
    /// service could not find one; a driver that needs anodizer output then
    /// fails saying so, rather than building against nothing.
    #[serde(default)]
    pub anodized_bucket: Option<String>,
}

impl AgentDriverBuildQuery {
    /// The extra arguments, as cargo will receive them.
    pub fn arguments(&self) -> Vec<String> {
        v1::DriverBuildQuery {
            release: self.release,
            args: self.args.clone(),
        }
        .arguments()
    }
}

impl From<v1::DriverBuildQuery> for AgentDriverBuildQuery {
    fn from(old: v1::DriverBuildQuery) -> Self {
        AgentDriverBuildQuery {
            release: old.release,
            args: old.args,
            anodized_bucket: None,
        }
    }
}
