//! Version `ARTIFACT_FLUSH` of the vw APIs.
//!
//! Adds waiting for a build's finished artifacts to reach the object store, so
//! that collecting them straight after a build does not race the instance that
//! is still uploading them. No type from a prior version changed, so there is
//! nothing to convert.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a flush sent, and whether it got to the end of the job.
///
/// `settled` is the half that matters. A flush that ran out of time has still
/// uploaded whatever it managed, and a listing taken afterwards will look
/// entirely normal — just short. Saying so is the only thing standing between
/// a caller and the silent partial collection this call exists to prevent.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactFlush {
    /// Artifacts that went during this flush.
    ///
    /// Zero is the ordinary answer: it means everything a build produced had
    /// already been uploaded by the time anybody asked.
    pub uploaded: usize,
    /// Whether the build output came to rest.
    ///
    /// True when a whole pass found nothing left to send, nothing still being
    /// written, and nothing that failed to go. False when the deadline arrived
    /// first, which means the store may still be missing something.
    pub settled: bool,
}
