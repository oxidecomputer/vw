//! Version `DRIVER_CARGO` of the vw APIs.
//!
//! The helios instance could build the driver and do nothing else with it. A
//! driver is developed like any other cargo workspace — linted, checked,
//! tested, held to a format — and none of that can happen on a developer's
//! machine for the same reasons the build cannot: the kernel module targets
//! illumos, the workspace pins its own toolchain, and it includes anodizer
//! output that only exists in the environment. So the instance now runs any
//! of the cargo commands a driver is developed with, and building is one of
//! them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{v1, v6};

/// Which cargo command to run over the driver.
///
/// A fixed set rather than any cargo subcommand. Each one differs in what it
/// needs from the environment and what it leaves behind — whether it compiles
/// against anodizer output, whether it produces anything worth storing,
/// whether a member built for its own target can take part — and a name the
/// instance does not know is one it cannot get those right for.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DriverCommand {
    /// `cargo build`, storing what it produces.
    #[default]
    Build,
    /// `cargo check`.
    Check,
    /// `cargo clippy`.
    Clippy,
    /// `cargo test`, for the members that run where they are built.
    Test,
    /// `cargo fmt`, sending back every file it changed for the client to
    /// write into the developer's own tree. The instance's copy of the
    /// workspace is a mirror, so formatting only it would change nothing
    /// anyone keeps.
    Fmt,
    /// `cargo fmt --check`, which reports and rewrites nothing.
    FmtCheck,
}

impl std::fmt::Display for DriverCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Build => "build",
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Fmt => "fmt",
            Self::FmtCheck => "fmt --check",
        })
    }
}

/// What to run over the driver, as a developer asks for it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct DriverQuery {
    /// The cargo command.
    pub command: DriverCommand,
    /// Compile with optimizations. Ignored by the `fmt` commands.
    #[serde(default)]
    pub release: bool,
    /// Anything else to put on cargo's command line, separated by spaces.
    ///
    /// Split on whitespace at the far end, for the reason given on
    /// [`v1::DriverBuildQuery::args`].
    #[serde(default)]
    pub args: Option<String>,
}

impl From<v1::DriverBuildQuery> for DriverQuery {
    /// What a client from before this version asked for, which was always a
    /// build.
    fn from(old: v1::DriverBuildQuery) -> Self {
        DriverQuery {
            command: DriverCommand::Build,
            release: old.release,
            args: old.args,
        }
    }
}

/// How the service asks a helios instance to run a cargo command over the
/// driver.
///
/// The developer's [`DriverQuery`], plus what only the service knows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentDriverQuery {
    /// The cargo command.
    pub command: DriverCommand,
    /// Compile with optimizations.
    #[serde(default)]
    pub release: bool,
    /// Anything else to put on cargo's command line, separated by spaces.
    #[serde(default)]
    pub args: Option<String>,
    /// The bucket the vivado instance publishes this workspace's anodizer
    /// output to. See [`v6::AgentDriverBuildQuery::anodized_bucket`].
    #[serde(default)]
    pub anodized_bucket: Option<String>,
}

impl AgentDriverQuery {
    /// The extra arguments, as cargo will receive them.
    pub fn arguments(&self) -> Vec<String> {
        v1::DriverBuildQuery {
            release: self.release,
            args: self.args.clone(),
        }
        .arguments()
    }
}

impl From<v6::AgentDriverBuildQuery> for AgentDriverQuery {
    fn from(old: v6::AgentDriverBuildQuery) -> Self {
        AgentDriverQuery {
            command: DriverCommand::Build,
            release: old.release,
            args: old.args,
            anodized_bucket: old.anodized_bucket,
        }
    }
}

impl From<v1::DriverBuildQuery> for AgentDriverQuery {
    fn from(old: v1::DriverBuildQuery) -> Self {
        v6::AgentDriverBuildQuery::from(old).into()
    }
}
