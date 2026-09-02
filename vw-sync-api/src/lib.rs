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
    api_description, FreeformBody, HttpError, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, Query, RequestContext, TypedBody,
    UntypedBody, WebsocketChannelResult, WebsocketConnection,
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
    (2, ARTIFACT_FLUSH),
    (1, INITIAL),
]);

/// Header a client names the API version it is written against in.
///
/// Spelled the same as the user API's, and deliberately a separate constant:
/// the two are different documents with different version histories, and a
/// client that sent one's version to the other would be understood right up
/// until the numbers diverged.
pub const API_VERSION_HEADER: &str = "api-version";

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

    /// Put the credentials a build needs to fetch its dependencies in place.
    ///
    /// Written as a `.netrc`, which is what git, cargo and the rest already
    /// know how to read, so nothing on the instance needs teaching about where
    /// its credentials come from.
    ///
    /// These belong to whoever is synchronizing, and `vw-svc` sends them with
    /// every sync rather than once: an instance rebuilt underneath us comes
    /// back with no credentials at all, and the alternative is builds that
    /// fail to fetch until something notices.
    #[endpoint {
        method = PUT,
        path = "/environment/{environment}/credentials",
    }]
    async fn put_credentials(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        body: TypedBody<latest::Credentials>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    /// Remove everything a build wrote on this instance.
    ///
    /// The opposite of what synchronization does: `target/` is the one thing a
    /// sync will never send and never delete, which is exactly why removing it
    /// needs saying explicitly. Source is untouched, so the next build starts
    /// over without anything having to be pushed again.
    #[endpoint {
        method = DELETE,
        path = "/environment/{environment}/build-output",
    }]
    async fn clean_build_output(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::CleanResult>, HttpError>;

    /// Where this instance currently believes its artifacts should go.
    ///
    /// Answers `404` when it has never been told. Lets the service notice an
    /// instance that needs configuring — one created before there was a store,
    /// or whose store has since been rebuilt with a new key — without pushing
    /// credentials at every instance on every restart.
    #[endpoint {
        method = GET,
        path = "/environment/{environment}/artifact-target",
    }]
    async fn get_artifact_target(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::S3Credentials>, HttpError>;

    /// The VHDL vivado generated for this environment's IP.
    ///
    /// A `POST` because it finishes the job first: vivado writes an
    /// instantiation template per standalone IP, and turning those into
    /// black-box entities is a mechanical step that happens in Rust after the
    /// vivado pass. On a local run that happens on the developer's machine; on
    /// a remote one there is nobody there to do it, so it happens here, where
    /// the templates are.
    ///
    /// Answers with paths relative to the workspace, so the far end can put
    /// each file exactly where its own tools will look for it.
    #[endpoint {
        method = POST,
        path = "/environment/{environment}/generated",
    }]
    async fn generated_manifest(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::TreeManifest>, HttpError>;

    /// One generated file's contents.
    #[endpoint {
        method = GET,
        path = "/environment/{environment}/generated/file",
    }]
    async fn generated_file(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        query: Query<latest::GeneratedFileQuery>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError>;

    /// The key that opens this instance's object store.
    ///
    /// Answered only by the instance that runs the store. The admin credential
    /// that minted this key never leaves that machine — this is the one thing
    /// it hands out, and `vw-svc` passes it to the instance that has artifacts
    /// to upload.
    #[endpoint {
        method = GET,
        path = "/environment/{environment}/object-store",
    }]
    async fn get_object_store(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        query: Query<latest::ObjectStoreQuery>,
    ) -> Result<HttpResponseOk<latest::S3Credentials>, HttpError>;

    /// Tell this instance where to put the artifacts it builds.
    ///
    /// Sent by `vw-svc`, which got it from the instance that runs the store —
    /// this one cannot ask directly, since it has no way to know which of its
    /// neighbours holds it. Remembered on disk, so a reboot between two builds
    /// does not lose the answer.
    #[endpoint {
        method = PUT,
        path = "/environment/{environment}/artifact-target",
    }]
    async fn put_artifact_target(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        body: TypedBody<latest::S3Credentials>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    /// Send anything a build has left behind that has not gone yet, and
    /// answer once there is nothing left to send.
    ///
    /// The uploader this drives is a poller: it notices a finished artifact a
    /// second or two after the last byte is written, and deliberately waits to
    /// see a file stop changing before sending it. That is right for the case
    /// it was built for and wrong for the one where a build finishes and
    /// something collects immediately — the collection lists the store before
    /// the last artifact has reached it, and gets a set that looks complete.
    ///
    /// So this is a barrier rather than a command: it names nothing and knows
    /// nothing about what a build produces, it just runs the same walk the
    /// uploader already runs until a pass finds nothing left to do. A stage
    /// added tomorrow is covered without anything here changing.
    ///
    /// Answers `404` when this instance has not been told where its artifacts
    /// go, or when it is not the kind of instance that produces any.
    #[endpoint {
        method = POST,
        path = "/environment/{environment}/artifact-flush",
        versions = VERSION_ARTIFACT_FLUSH..,
    }]
    async fn flush_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::ArtifactFlush>, HttpError>;

    /// Build the driver on this instance.
    ///
    /// A websocket because a build takes minutes and produces output the whole
    /// time, and because a developer who interrupts one should not leave cargo
    /// running on a machine nobody is watching.
    ///
    /// Cargo is spawned rather than linked: the driver pins its toolchain in
    /// `rust-toolchain.toml`, which the rustup shim honours and a linked cargo
    /// would not, so linking it would quietly build a kernel module with the
    /// wrong compiler.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{environment}/driver/build",
    }]
    async fn driver_build(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        query: Query<latest::DriverBuildQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Run this workspace's testbenches on this instance.
    ///
    /// A websocket for the same reason a vivado session is one: a batch takes
    /// minutes and finishes one bench at a time, and a developer watching it
    /// should see each result land rather than a verdict at the end.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{environment}/bench/session",
    }]
    async fn bench_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        query: Query<latest::BenchQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Drive a vivado worker on this instance.
    ///
    /// A websocket rather than a request and a reply because a build is not
    /// one of those. It is a conversation that runs for minutes, produces
    /// output the whole time, and has to show that output as it happens —
    /// waiting for a synthesis run to finish before saying anything would make
    /// the remote flow useless for the thing people actually do with it.
    ///
    /// What crosses the socket is the protocol `vw-eda` already uses to talk
    /// to a local worker: commands in, output chunks and results back. The
    /// worker is spawned when the socket opens and torn down when it closes,
    /// so no state survives between runs — the same guarantee running vivado
    /// locally gives, and what the checkpoint machinery in the htcl library
    /// already relies on for speed.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{environment}/vivado/session",
    }]
    async fn vivado_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<EnvironmentPathParam>,
        query: Query<latest::VivadoSessionQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;
}
