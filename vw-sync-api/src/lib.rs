// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The API a build instance exposes so that `vw-svc` can put source on it.
//!
//! Three calls make a synchronization: say what the tree should look like and
//! find out what is missing, deliver exactly that, then ask for the tree to be
//! made to match. Each is idempotent — a blob is named by its own content, and
//! a commit describes a destination rather than a change — so a retry after a
//! dropped connection costs at worst a repeated upload.
//!
//! This is not reachable from a developer's machine. `vw-svc` holds the only
//! route to it, over the rack's internal network, and relays the client's calls
//! after deciding whether the caller owns the environment in question.

use dropshot::{
    api_description, HttpError, HttpResponseOk, HttpResponseUpdatedNoContent,
    Path, RequestContext, TypedBody, UntypedBody,
};
use dropshot_api_manager_types::api_versions;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use vw_api_types_versions::latest;

api_versions!([
    // WHEN CHANGING THE API (part 1 of 2):
    //
    // +- Pick a new semver and define it in the list below.  The list MUST
    // |  remain sorted, which generally means that your version should go at
    // |  the very top.
    // |
    // |  Duplicate this line, uncomment the *second* copy, update that copy for
    // |  your new API version, and leave the first copy commented out as an
    // |  example for the next person.
    // v
    // (next_int, IDENT),
    (1, INITIAL),
]);

/// Which environment a request is for.
///
/// An agent serves exactly one, and checks this against the one it was
/// started with. The name never becomes part of a filesystem path — the tree
/// and content store are fixed at startup — so a request for the wrong
/// environment is answered rather than acted on.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EnvironmentPathParam {
    pub environment: String,
}

/// Which piece of content is being delivered.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlobPathParam {
    pub environment: String,
    /// The digest of the content in the body, which is verified on arrival
    /// rather than taken at face value.
    pub digest: latest::Digest,
}

/// Source synchronization for one build instance.
#[api_description]
pub trait VwSyncApi {
    type Context;

    /// Report what content is missing before a tree can be made to match.
    ///
    /// Content already held anywhere in the instance's tree is not asked for,
    /// whatever path it currently sits under, so a rename or a directory move
    /// costs nothing over the wire.
    #[endpoint {
        method = POST,
        path = "/environment/{environment}/sync/plan",
    }]
    async fn sync_plan(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::SyncPlan>, HttpError>;

    /// Deliver one piece of content.
    ///
    /// Rejected if the body does not hash to the digest in the path: the
    /// digest is how every later lookup finds this content, so storing it
    /// under a name it does not have would be worse than not storing it.
    #[endpoint {
        method = PUT,
        path = "/environment/{environment}/sync/blob/{digest}",
    }]
    async fn sync_blob(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<BlobPathParam>,
        body: UntypedBody,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    /// Make the instance's tree match the manifest.
    ///
    /// The manifest is the complete desired state, so this adds, replaces and
    /// removes as needed. Anything a build produced is invisible to it.
    #[endpoint {
        method = POST,
        path = "/environment/{environment}/sync/commit",
    }]
    async fn sync_commit(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::CommitResult>, HttpError>;

    /// Discard the source tree and everything delivered towards it.
    ///
    /// The instance is left as though it had never been synchronized: no
    /// source, and no record of what content it holds. Build output is
    /// untouched, as it is for a commit.
    ///
    /// This is not part of an ordinary sync, which needs no help — a commit
    /// replaces whatever differs from the manifest. It is what a sender uses
    /// when it does not believe the instance's account of what it has, so that
    /// the sync that follows sends everything rather than asking first.
    ///
    /// Answers with the result of committing an empty manifest, so the count
    /// of what was removed is the `deleted` field.
    #[endpoint {
        method = DELETE,
        path = "/environment/{environment}/sync",
    }]
    async fn sync_clear(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::CommitResult>, HttpError>;
}
