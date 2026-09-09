use dropshot::{
    api_description, FreeformBody, HttpError, HttpResponseCreated,
    HttpResponseDeleted, HttpResponseOk, HttpResponseUpdatedNoContent, Path,
    Query, RequestContext, ResultsPage, TypedBody, UntypedBody,
    WebsocketChannelResult, WebsocketConnection,
};
use dropshot_api_manager_types::api_versions;
use vw_api_types_versions::{latest, v1, v3, v4};

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
    (5, WORKSPACES),
    (4, ANODIZE),
    (3, ARTIFACT_FLUSH),
    (2, IMAGE_RECYCLE),
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

/// Header a client names the API version it is written against in.
///
/// Required even though only one version is served. A client that omits it is
/// turned away rather than assumed to mean the version that happens to be
/// current, so the day a second one exists nothing has to change about what a
/// well-behaved client already sends.
pub const API_VERSION_HEADER: &str = "api-version";

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
        path = "/environment/{name}/workspace/{workspace}/target/{kind}/sync/plan",
        versions = VERSION_WORKSPACES..,
    }]
    async fn sync_plan(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::SyncPlan>, HttpError>;

    /// Report what source content an environment's instance still needs.
    #[endpoint {
        operation_id = "sync_plan",
        method = POST,
        path = "/environment/{name}/target/{kind}/sync/plan",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn sync_plan_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::TargetPathParam>,
        body: TypedBody<v1::TreeManifest>,
    ) -> Result<HttpResponseOk<v1::SyncPlan>, HttpError> {
        Self::sync_plan(
            rqctx,
            path_params.map(|params| {
                latest::TargetPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            body,
        )
        .await
    }

    /// Deliver one piece of source content.
    #[endpoint {
        method = PUT,
        path = "/environment/{name}/workspace/{workspace}/target/{kind}/sync/blob/{digest}",
        versions = VERSION_WORKSPACES..,
    }]
    async fn sync_blob(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetBlobPathParam>,
        body: UntypedBody,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError>;

    /// Deliver one piece of source content.
    #[endpoint {
        operation_id = "sync_blob",
        method = PUT,
        path = "/environment/{name}/target/{kind}/sync/blob/{digest}",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn sync_blob_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::TargetBlobPathParam>,
        body: UntypedBody,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        Self::sync_blob(
            rqctx,
            path_params.map(|params| {
                latest::TargetBlobPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            body,
        )
        .await
    }

    /// Make the instance's source tree match the manifest.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/workspace/{workspace}/target/{kind}/sync/commit",
        versions = VERSION_WORKSPACES..,
    }]
    async fn sync_commit(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
        body: TypedBody<latest::TreeManifest>,
    ) -> Result<HttpResponseOk<latest::CommitResult>, HttpError>;

    /// Make the instance's source tree match the manifest.
    #[endpoint {
        operation_id = "sync_commit",
        method = POST,
        path = "/environment/{name}/target/{kind}/sync/commit",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn sync_commit_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::TargetPathParam>,
        body: TypedBody<v1::TreeManifest>,
    ) -> Result<HttpResponseOk<v1::CommitResult>, HttpError> {
        Self::sync_commit(
            rqctx,
            path_params.map(|params| {
                latest::TargetPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            body,
        )
        .await
    }

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
        path = "/environment/{name}/workspace/{workspace}/target/{kind}/sync",
        versions = VERSION_WORKSPACES..,
    }]
    async fn sync_clear(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
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
        operation_id = "sync_clear",
        method = DELETE,
        path = "/environment/{name}/target/{kind}/sync",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn sync_clear_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::TargetPathParam>,
    ) -> Result<HttpResponseOk<v1::CommitResult>, HttpError> {
        Self::sync_clear(
            rqctx,
            path_params.map(|params| {
                latest::TargetPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

    /// Remove everything a build wrote on one of an environment's instances.
    ///
    /// `target/` is the one directory synchronization will never touch, in
    /// either direction, so it outlives every push and has to be removed on
    /// purpose. Source on the instance is left alone.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/workspace/{workspace}/target/{kind}/build-output",
        versions = VERSION_WORKSPACES..,
    }]
    async fn clean_build_output(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::TargetPathParam>,
    ) -> Result<HttpResponseOk<latest::CleanResult>, HttpError>;

    /// Remove everything a build wrote on one of an environment's instances.
    ///
    /// `target/` is the one directory synchronization will never touch, in
    /// either direction, so it outlives every push and has to be removed on
    /// purpose. Source on the instance is left alone.
    #[endpoint {
        operation_id = "clean_build_output",
        method = DELETE,
        path = "/environment/{name}/target/{kind}/build-output",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn clean_build_output_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::TargetPathParam>,
    ) -> Result<HttpResponseOk<v1::CleanResult>, HttpError> {
        Self::clean_build_output(
            rqctx,
            path_params.map(|params| {
                latest::TargetPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

    /// Build the driver on an environment's helios instance.
    ///
    /// Relayed frame for frame. The driver's target is native there and its
    /// pinned toolchain is installed there, which is the whole reason the
    /// build does not happen on a developer's machine.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/workspace/{workspace}/driver/build",
        versions = VERSION_WORKSPACES..,
    }]
    async fn driver_build(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
        query: Query<latest::DriverBuildQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Build the driver on an environment's helios instance.
    ///
    /// Relayed frame for frame. The driver's target is native there and its
    /// pinned toolchain is installed there, which is the whole reason the
    /// build does not happen on a developer's machine.
    #[channel {
        operation_id = "driver_build",
        protocol = WEBSOCKETS,
        path = "/environment/{name}/driver/build",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn driver_build_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
        query: Query<v1::DriverBuildQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult {
        Self::driver_build(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            query,
            websock,
        )
        .await
    }

    /// Run the anodizer on an environment's vivado instance.
    ///
    /// Relayed frame for frame. Anodization already happens on its own before
    /// every bench run, cached and silent; this is the same generator with
    /// the cache off and the answers reported, for working on the generator
    /// itself. It runs there rather than here because it is an nvc pass over
    /// the workspace's VHDL, and the workspace is there.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/workspace/{workspace}/anodize",
        versions = VERSION_WORKSPACES..,
    }]
    async fn anodize(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
        query: Query<latest::AnodizeQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Run the anodizer on an environment's vivado instance.
    ///
    /// Relayed frame for frame. Anodization already happens on its own before
    /// every bench run, cached and silent; this is the same generator with
    /// the cache off and the answers reported, for working on the generator
    /// itself. It runs there rather than here because it is an nvc pass over
    /// the workspace's VHDL, and the workspace is there.
    #[channel {
        operation_id = "anodize",
        protocol = WEBSOCKETS,
        path = "/environment/{name}/anodize",
        versions = VERSION_ANODIZE..VERSION_WORKSPACES,
    }]
    async fn anodize_v4(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
        query: Query<v4::AnodizeQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult {
        Self::anodize(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            query,
            websock,
        )
        .await
    }

    /// Run an environment's testbenches on its vivado instance.
    ///
    /// Relayed frame for frame. What comes back is the same stream of events a
    /// local run produces, so the display on a developer's terminal is driven
    /// by exactly what would have driven it here.
    #[channel {
        protocol = WEBSOCKETS,
        path = "/environment/{name}/workspace/{workspace}/bench/session",
        versions = VERSION_WORKSPACES..,
    }]
    async fn bench_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
        query: Query<latest::BenchQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult;

    /// Run an environment's testbenches on its vivado instance.
    ///
    /// Relayed frame for frame. What comes back is the same stream of events a
    /// local run produces, so the display on a developer's terminal is driven
    /// by exactly what would have driven it here.
    #[channel {
        operation_id = "bench_session",
        protocol = WEBSOCKETS,
        path = "/environment/{name}/bench/session",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn bench_session_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
        query: Query<v1::BenchQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult {
        Self::bench_session(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            query,
            websock,
        )
        .await
    }

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
        path = "/environment/{name}/workspace/{workspace}/vivado/session",
        versions = VERSION_WORKSPACES..,
    }]
    async fn vivado_session(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
        query: Query<latest::VivadoSessionQuery>,
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
        operation_id = "vivado_session",
        protocol = WEBSOCKETS,
        path = "/environment/{name}/vivado/session",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn vivado_session_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
        query: Query<v1::VivadoSessionQuery>,
        websock: WebsocketConnection,
    ) -> WebsocketChannelResult {
        Self::vivado_session(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            query,
            websock,
        )
        .await
    }

    /// The VHDL vivado generated for this environment's IP.
    ///
    /// A developer's static analysis needs these to resolve the design, and
    /// they only exist where vivado ran. Relayed from the vivado instance.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/workspace/{workspace}/generated",
        versions = VERSION_WORKSPACES..,
    }]
    async fn generated_manifest(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
    ) -> Result<HttpResponseOk<latest::TreeManifest>, HttpError>;

    /// The VHDL vivado generated for this environment's IP.
    ///
    /// A developer's static analysis needs these to resolve the design, and
    /// they only exist where vivado ran. Relayed from the vivado instance.
    #[endpoint {
        operation_id = "generated_manifest",
        method = POST,
        path = "/environment/{name}/generated",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn generated_manifest_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<v1::TreeManifest>, HttpError> {
        Self::generated_manifest(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

    /// One generated file's contents.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/workspace/{workspace}/generated/file",
        versions = VERSION_WORKSPACES..,
    }]
    async fn generated_file(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
        query: Query<latest::GeneratedFileQuery>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError>;

    /// One generated file's contents.
    #[endpoint {
        operation_id = "generated_file",
        method = GET,
        path = "/environment/{name}/generated/file",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn generated_file_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
        query: Query<v1::GeneratedFileQuery>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError> {
        Self::generated_file(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
            query,
        )
        .await
    }

    /// List the artifacts an environment's builds have produced.
    ///
    /// Read from the environment's own object store, which lives on its
    /// artifact instance.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/workspace/{workspace}/artifacts",
        versions = VERSION_WORKSPACES..,
    }]
    async fn get_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
    ) -> Result<HttpResponseOk<Vec<latest::Artifact>>, HttpError>;

    /// List the artifacts an environment's builds have produced.
    ///
    /// Read from the environment's own object store, which lives on its
    /// artifact instance.
    #[endpoint {
        operation_id = "get_artifacts",
        method = GET,
        path = "/environment/{name}/artifacts",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn get_artifacts_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<Vec<v1::Artifact>>, HttpError> {
        Self::get_artifacts(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

    /// Wait for this environment's finished artifacts to reach its store.
    ///
    /// The instance uploads what a build produces on a timer, noticing a
    /// finished artifact a second or two after the last byte is written. That
    /// gap is invisible to a person, who takes minutes to think about
    /// collecting, and is exactly the wrong size for a script, which collects
    /// in under a second and gets a listing that is short by however many
    /// artifacts were still on their way. Nothing about the result says so.
    ///
    /// So this exists to be called before [`Self::get_artifacts`], by the
    /// callers that would otherwise lose the race. It is not called on the
    /// caller's behalf, because it costs the seconds the uploader takes to
    /// come to rest and most listings are somebody looking rather than a
    /// build collecting.
    ///
    /// Names nothing: the instance runs the walk it already runs until a pass
    /// finds nothing left to do, so a build stage added later is covered
    /// without this changing.
    #[endpoint {
        method = POST,
        path = "/environment/{name}/workspace/{workspace}/artifact-flush",
        versions = VERSION_WORKSPACES..,
    }]
    async fn flush_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
    ) -> Result<HttpResponseOk<latest::ArtifactFlush>, HttpError>;

    /// Wait for this environment's finished artifacts to reach its store.
    ///
    /// The instance uploads what a build produces on a timer, noticing a
    /// finished artifact a second or two after the last byte is written. That
    /// gap is invisible to a person, who takes minutes to think about
    /// collecting, and is exactly the wrong size for a script, which collects
    /// in under a second and gets a listing that is short by however many
    /// artifacts were still on their way. Nothing about the result says so.
    ///
    /// So this exists to be called before [`Self::get_artifacts`], by the
    /// callers that would otherwise lose the race. It is not called on the
    /// caller's behalf, because it costs the seconds the uploader takes to
    /// come to rest and most listings are somebody looking rather than a
    /// build collecting.
    ///
    /// Names nothing: the instance runs the walk it already runs until a pass
    /// finds nothing left to do, so a build stage added later is covered
    /// without this changing.
    #[endpoint {
        operation_id = "flush_artifacts",
        method = POST,
        path = "/environment/{name}/artifact-flush",
        versions = VERSION_ARTIFACT_FLUSH..VERSION_WORKSPACES,
    }]
    async fn flush_artifacts_v3(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<v3::ArtifactFlush>, HttpError> {
        Self::flush_artifacts(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

    /// Remove every artifact an environment has stored.
    ///
    /// Irreversible: the object store keeps no versions, so what goes is gone.
    /// The instances themselves are untouched — a build's output is still on
    /// the machine that made it until that machine is cleaned or replaced.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/workspace/{workspace}/artifacts",
        versions = VERSION_WORKSPACES..,
    }]
    async fn clear_artifacts(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
    ) -> Result<HttpResponseOk<latest::ArtifactsCleared>, HttpError>;

    /// Remove every artifact an environment has stored.
    ///
    /// Irreversible: the object store keeps no versions, so what goes is gone.
    /// The instances themselves are untouched — a build's output is still on
    /// the machine that made it until that machine is cleaned or replaced.
    #[endpoint {
        operation_id = "clear_artifacts",
        method = DELETE,
        path = "/environment/{name}/artifacts",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn clear_artifacts_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<v1::ArtifactsCleared>, HttpError> {
        Self::clear_artifacts(
            rqctx,
            path_params.map(|params| {
                latest::WorkspacePathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

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
        path = "/environment/{name}/workspace/{workspace}/artifacts/{kind}/{artifact}",
        versions = VERSION_WORKSPACES..,
    }]
    async fn get_artifact(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::ArtifactPathParam>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError>;

    /// Download one artifact.
    ///
    /// Streamed through this service rather than handed out as a link to the
    /// store. The store sits on the rack's internal network, and its instance's
    /// external address is often only reachable over a VPN — needing one to
    /// collect a build's output would make this useless from anywhere else.
    /// The body is passed through as it arrives, so an image of any size costs
    /// this service no more memory than a small one.
    #[endpoint {
        operation_id = "get_artifact",
        method = GET,
        path = "/environment/{name}/artifacts/{kind}/{artifact}",
        versions = ..VERSION_WORKSPACES,
    }]
    async fn get_artifact_v1(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<v1::ArtifactPathParam>,
    ) -> Result<HttpResponseOk<FreeformBody>, HttpError> {
        Self::get_artifact(
            rqctx,
            path_params.map(|params| {
                latest::ArtifactPathParam::from_v1(
                    params,
                    latest::LEGACY_WORKSPACE,
                )
            }),
        )
        .await
    }

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

    //
    // Workspaces
    //
    // An environment holds a source tree per workspace, keyed by whatever
    // `vw-cloud.toml` — or failing that `vw.toml` — calls it. Which ones
    // exist is not something this service records: an instance has a
    // workspace because somebody synchronized one to it, so the instance is
    // asked.
    //

    /// List the workspaces synchronized to an environment.
    ///
    /// Read from the vivado instance, which every sync reaches. What comes
    /// back is what is taking up room and when each was last pushed to, which
    /// together are how somebody works out which slots they are finished
    /// with.
    #[endpoint {
        method = GET,
        path = "/environment/{name}/workspaces",
        versions = VERSION_WORKSPACES..,
    }]
    async fn get_workspaces(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::EnvironmentPathParam>,
        query: Query<latest::WorkspaceListQuery>,
    ) -> Result<HttpResponseOk<Vec<latest::Workspace>>, HttpError>;

    /// Remove a workspace from an environment entirely.
    ///
    /// Its tree on both instances that take source, everything a build wrote
    /// under it, and its artifacts. Distinct from clearing a sync, which
    /// leaves the slot behind ready to be filled again: this is for a
    /// workspace that was renamed, or stood for a branch that is finished, and
    /// would otherwise sit there forever because nothing else ever removes
    /// one.
    ///
    /// Irreversible on the artifact side — the store keeps no versions — and
    /// costs nothing on the source side, since the tree came from a
    /// developer's machine and can be pushed again.
    #[endpoint {
        method = DELETE,
        path = "/environment/{name}/workspace/{workspace}",
        versions = VERSION_WORKSPACES..,
    }]
    async fn forget_workspace(
        rqctx: RequestContext<Self::Context>,
        path_params: Path<latest::WorkspacePathParam>,
    ) -> Result<HttpResponseOk<latest::WorkspaceForgotten>, HttpError>;
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

    /// Delete the service's images that nothing is using and nothing would
    /// use.
    ///
    /// Each image kind keeps its newest — what an environment created now
    /// would boot — and every image any environment is booting, however old.
    /// Only the service's own images in its project are ever candidates, so
    /// the base images the rack publishes are not at risk.
    #[endpoint {
        method = POST,
        path = "/images/recycle",
        versions = VERSION_IMAGE_RECYCLE..,
    }]
    async fn recycle_images(
        rqctx: RequestContext<Self::Context>,
        query: Query<latest::ImageRecycleQuery>,
    ) -> Result<HttpResponseOk<latest::ImageRecycleReport>, HttpError>;
}
