//! Version `IMAGE_RECYCLE` of the vw APIs.
//!
//! Adds reclaiming the service's unused images to the admin API. No type
//! from a prior version changed, so there is nothing to convert.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Query parameters for a recycle pass over the service's images.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ImageRecycleQuery {
    /// Work out what would be deleted and report it, without deleting
    /// anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// What a recycle pass did, and what it left alone.
///
/// Both halves are reported because the interesting question after running
/// this is usually not "what went" but "why is that one still here".
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ImageRecycleReport {
    /// Images deleted, or that would have been on a real pass.
    pub deleted: Vec<RecycledImage>,
    /// Images considered and spared, each with what spared it.
    pub kept: Vec<KeptImage>,
    /// Whether this was a dry run, echoed back so a report cannot be
    /// mistaken for the other kind.
    pub dry_run: bool,
}

/// An image a recycle pass removed.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RecycledImage {
    pub id: Uuid,
    pub name: String,
}

/// An image a recycle pass considered and left alone.
///
/// At least one of `latest` and `used_by` says why. Both can be true at once
/// — the newest image of a kind is usually also the one environments are
/// booting — and neither is reported in preference to the other, because an
/// administrator asking why an image survived is owed the whole answer.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct KeptImage {
    pub id: Uuid,
    pub name: String,
    /// The newest image of its kind: what an environment created now would
    /// boot if it did not name an image.
    pub latest: bool,
    /// Environments booting this image, as `user/name`.
    pub used_by: Vec<String>,
}
