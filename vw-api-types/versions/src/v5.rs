//! Version `WORKSPACES` of the vw APIs.
//!
//! One environment used to hold one workspace, so an environment name
//! was enough to say which tree a request meant. Three instances is a
//! lot of rack to spend on one checkout, and the developer with a
//! design and a driver and a scratch copy of each was paying for it
//! three times over — so an environment now holds as many workspaces
//! as are synchronized to it, and every request that means a tree has
//! to say which one.
//!
//! What that key namespaces is exactly three things: the source tree
//! on each instance, the content store standing behind it, and the
//! bucket its artifacts go to. Nothing else about an environment is
//! divided up — it has one owner, one ssh key, and three instances,
//! the same as it ever did.
//!
//! The name is emphatically **not** the design's identity. That is
//! `[workspace] name` in `vw.toml`, which the htcl resolver uses for
//! a workspace's own imports (`src @foo/bar`) and which
//! `vw::project_name` hands to a design at build time. This is a slot
//! on an environment, which is why `vw-cloud.toml` can override it
//! per checkout without any of that moving underneath the build.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1::{Digest, TargetKind};

/// Which environment, and which workspace on it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspacePathParam {
    /// The name of the environment.
    pub name: String,
    /// The workspace within it.
    pub workspace: String,
}

/// Which workspace, and which half of the environment it is on.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceTargetPathParam {
    pub name: String,
    pub workspace: String,
    /// Which half of the environment.
    pub kind: TargetKind,
}

/// Which piece of content is being delivered, and to which tree.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceBlobPathParam {
    pub name: String,
    pub workspace: String,
    pub kind: TargetKind,
    /// The digest of the content in the body, verified on arrival.
    pub digest: Digest,
}

/// Which artifact is being fetched, and whose.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceArtifactPathParam {
    pub name: String,
    pub workspace: String,
    pub kind: TargetKind,
    /// The artifact's file name.
    pub artifact: String,
}

/// One workspace an environment is holding.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
    /// What it is called: the key it was synchronized under.
    pub name: String,
    /// When its tree was last made to match a manifest, in seconds
    /// since the Unix epoch.
    ///
    /// A number rather than a formatted time because what a reader
    /// wants is how long ago, and an age is worked out from an
    /// instant rather than parsed back out of a rendering of one.
    ///
    /// Absent when the instance cannot say — a tree cleared since it
    /// was last committed to, or one put there by something other
    /// than a sync. The point of reporting it at all is that
    /// staleness is what tells a developer which of several slots
    /// they are finished with, and a name on its own never does.
    pub last_synced: Option<u64>,
    /// What the workspace occupies on the instance, source and build
    /// output together.
    ///
    /// Zero when it was not measured — see
    /// [`WorkspaceListQuery::measure`], which is off by default
    /// because working this out means walking a build tree that runs
    /// to gigabytes.
    pub bytes: u64,
}

/// How much trouble to go to listing an environment's workspaces.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceListQuery {
    /// Measure what each workspace occupies.
    ///
    /// Off by default. The names and their sync times come from a
    /// `readdir` and a `stat`; the size comes from walking every file
    /// a build has ever written, which on a vivado instance is a
    /// great many of them. Somebody deciding what to forget wants the
    /// number and can wait for it. The service checking at startup
    /// which workspaces exist does not.
    #[serde(default)]
    pub measure: bool,
}

/// What forgetting a workspace came to.
///
/// Reported rather than silent because the three pieces go separately
/// — a tree on each instance that takes source, and a bucket per kind
/// on the one that holds artifacts — and "it is gone" is a claim
/// about all of them.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
pub struct WorkspaceForgotten {
    /// Instances whose copy of the tree was removed.
    pub trees: Vec<TargetKind>,
    /// Artifacts deleted from the environment's store.
    pub artifacts: usize,
    /// What those artifacts were taking up.
    pub bytes: u64,
}
