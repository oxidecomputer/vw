use dropshot::{
    api_description, FreeformBody, HttpError, HttpResponseCreated,
    HttpResponseDeleted, HttpResponseOk, HttpResponseUpdatedNoContent, Path,
    Query, RequestContext, ResultsPage, TypedBody, UntypedBody,
    WebsocketChannelResult, WebsocketConnection,
};
use dropshot_api_manager_types::api_versions;
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

// WHEN CHANGING THE API (part 2 of 2):
//
// The call to `api_versions!` above defines constants of type
// `semver::Version` that you can use in your Dropshot API definition to specify
// the version when a particular endpoint was added or removed.  For example, if
// you used:
//
//     (1, INITIAL)
//
// Then you could use `VERSION_INITIAL` as the version in which endpoints were
// added or removed.

/// User API. For all endpoints, the caller is identified by a Github access
/// token in the authorization header of the request.
#[api_description]
pub trait VwUserApi {
    type Context;

    //
    // Environment CRUD
    //

    /// Return a list of all environments for the calling user.
    #[endpoint {
        method = GET,
        path = "/environments",
    }]
    async fn get_environments(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<ResultsPage<latest::Environment>>, HttpError>;

    /// Create an environment with the specified name.
    ///
    /// The images the environment's instances boot from are chosen here and
    /// pinned for the life of the environment. Any image named in the body
    /// must already exist.
    ///
    /// Returns the ssh keypair generated for the new environment, so a caller
    /// can save it without a second round trip. The same pair is available
    /// afterwards from `get_environment_keys`.
    #[endpoint {
        method = PUT,
        path = "/environment/{name}"
    }]
    async fn create_environment(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        body: TypedBody<latest::EnvironmentCreate>,
    ) -> Result<HttpResponseCreated<latest::SshKeyPair>, HttpError>;

    /// Get an environment with the specified name.
    #[endpoint {
        method = GET,
        path = "/environment/{name}"
    }]
    async fn get_environment(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::Environment>, HttpError>;

    /// Delete an environment with the specified name.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}"
    }]
    async fn delete_environment(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseDeleted, HttpError>;

    //
    // Source synchronization
    //
    // Relayed to the instance that serves the named half of the environment,
    // over the rack's internal network. The client never reaches an instance
    // directly, so this is the only route source takes.
    //

    /// Report what source content an environment's instance still needs.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/target/{kind}/sync/plan",
    }]
    async fn sync_plan(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::SyncPlan>, HttpError>;

    /// Deliver one piece of source content.
    #[endpoint {
        method = PUT,
        path = "/environment/{name}/target/{kind}/sync/blob/{digest}",
    }]
    async fn sync_blob(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetBlobPathParam>,
        body: UntypedBody,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    /// Make the instance's source tree match the manifest.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/target/{kind}/sync/commit",
    }]
    async fn sync_commit(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::CommitResult>, HttpError>;

    /// Discard an environment's source tree, so the next sync sends all of it.
    ///
    /// An ordinary sync does not need this: the instance is told the whole
    /// desired state and replaces whatever differs from it. This is for when
    /// what the instance says it has is itself in question — with the tree and
    /// the delivered content both gone there is nothing left to be wrong
    /// about, and the sync that follows sends every file.
    ///
    /// Build output on the instance is not touched.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/target/{kind}/sync",
    }]
    async fn sync_clear(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
    ) -> Result<HttpResponseOk<latest::CommitResult>, HttpError>;

    /// Remove everything a build wrote on one of an environment's instances.
    ///
    /// `target/` is the one directory synchronization will never touch, in
    /// either direction, so it outlives every push and has to be removed on
    /// purpose. Source on the instance is left alone.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/target/{kind}/build-output",
    }]
    async fn clean_build_output(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
    ) -> Result<HttpResponseOk<latest::CleanResult>, HttpError>;

    /// Build the driver on an environment's helios instance.
    ///
    /// Relayed frame for frame. The driver's target is native there and its
    /// pinned toolchain is installed there, which is the whole reason the
    /// build does not happen on a developer's machine.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/driver/build",
    }]
    async fn driver_build(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        query: Query<latest::DriverBuildQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Run an environment's testbenches on its vivado instance.
    ///
    /// Relayed frame for frame. What comes back is the same stream of events a
    /// local run produces, so the display on a developer's terminal is driven
    /// by exactly what would have driven it here.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/bench/session",
    }]
    async fn bench_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        query: Query<latest::BenchQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Drive a vivado worker on an environment's vivado instance.
    ///
    /// Relayed frame for frame to the instance, which spawns the worker when
    /// this opens and tears it down when it closes. A build is a conversation
    /// that runs for a long time and produces output throughout, so it is a
    /// websocket rather than a request and a reply — the developer sees each
    /// message as vivado emits it, exactly as they would running it locally.
    ///
    /// The source being built is whatever the last synchronization put on the
    /// instance. Nothing is shipped over this socket.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/vivado/session",
    }]
    async fn vivado_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        query: Query<latest::VivadoSessionQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// The VHDL vivado generated for this environment's IP.
    ///
    /// A developer's static analysis needs these to resolve the design, and
    /// they only exist where vivado ran. Relayed from the vivado instance.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/generated",
    }]
    async fn generated_manifest(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::TreeManifest>, HttpError>;

    /// One generated file's contents.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/generated/file",
    }]
    async fn generated_file(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        query: Query<latest::GeneratedFileQuery>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError>;

    /// List the artifacts an environment's builds have produced.
    ///
    /// Read from the environment's own object store, which lives on its
    /// artifact instance.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/artifacts",
    }]
    async fn get_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<Vec<latest::Artifact>>, HttpError>;

    /// Remove every artifact an environment has stored.
    ///
    /// Irreversible: the object store keeps no versions, so what goes is gone.
    /// The instances themselves are untouched — a build's output is still on
    /// the machine that made it until that machine is cleaned or replaced.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/artifacts",
    }]
    async fn clear_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::ArtifactsCleared>, HttpError>;

    /// Download one artifact.
    ///
    /// Streamed through this service rather than handed out as a link to the
    /// store. The store sits on the rack's internal network, and its instance's
    /// external address is often only reachable over a VPN — needing one to
    /// collect a build's output would make this useless from anywhere else.
    /// The body is passed through as it arrives, so an image of any size costs
    /// this service no more memory than a small one.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/artifacts/{kind}/{artifact}",
    }]
    async fn get_artifact(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::ArtifactPathParam>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError>;

    /// Fetch the ssh keypair that opens an environment's instances.
    ///
    /// The private key is only ever handed to the environment's owner.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/keys"
    }]
    async fn get_environment_keys(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<latest::SshKeyPair>, HttpError>;
}

/// Administrator API. For all endpoints, the caller is identified by a Github
/// access token in the authorization header of the request. The caller's Github
/// username must provided to the server at startup in the --admin_users
/// arguments for authorization to be granted.
#[api_description]
pub trait VwAdminApi {
    type Context;

    /// Return a list of all environments.
    #[endpoint {
        method = GET,
        path = "/environments",
    }]
    async fn get_environments(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<ResultsPage<latest::UserEnvironment>>, HttpError>;

    /// Delete an environment with the specified name for the specified user.
    #[endpoint {
        method = DELETE,
        path = "/environment/{user}/{name}"
    }]
    async fn delete_environment(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::UserEnvironmentPathParam>,
    ) -> Result<HttpResponseDeleted, HttpError>;
}
