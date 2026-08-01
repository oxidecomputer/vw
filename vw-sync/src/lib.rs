// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Content-addressed synchronization of a source tree.
//!
//! The same engine runs at both ends. A sender scans a directory into a
//! [`TreeManifest`] describing every file it wants to exist; a receiver
//! answers with the content it does not already hold, takes delivery of that
//! into a [`Store`], and then makes its own directory match the manifest.
//!
//! Three properties are worth stating up front, because the rest follows from
//! them.
//!
//! **Whole files, not diffs.** Build sources are small, and content addressing
//! already collapses the unchanged ones. Computing and applying deltas would
//! cost more round trips than it saves bytes — the same conclusion Bazel and
//! Buck2 reached for their remote execution protocols.
//!
//! **Complete manifests, not changesets.** An environment may be synchronized
//! from a different machine tomorrow, and a changeset computed against one
//! machine's state means nothing against another's. A complete manifest makes
//! "make it look like this" the literal semantics, and deletions and renames
//! fall out of it.
//!
//! **Generated files are invisible.** The scan honours `.gitignore`
//! hierarchically and excludes build output outright, so neither end sends,
//! receives, or deletes anything a build produced. Both ends apply the same
//! rules — the receiver reads them from the tree it was handed, so they cannot
//! drift apart.
//!
//! [`TreeManifest`]: vw_api_types_versions::latest::TreeManifest

mod scan;
mod store;
mod tree;

pub use scan::{scan, ScanError, ALWAYS_IGNORED};
pub use store::{Store, StoreError};
pub use tree::{apply, clear, missing, ApplyError};

use vw_api_types_versions::latest::Digest;

/// The digest of some bytes.
pub fn digest_bytes(bytes: &[u8]) -> Digest {
    Digest(blake3::hash(bytes).to_hex().to_string())
}
