use dropshot::{
    api_description, HttpError, HttpResponseCreated, HttpResponseDeleted,
    HttpResponseOk, HttpResponseUpdatedNoContent, Path, RequestContext,
    ResultsPage, TypedBody, UntypedBody,
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
