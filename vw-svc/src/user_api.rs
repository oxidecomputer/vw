//! This module implements the user api trait `[vw_api::VwUserApi]`
use crate::{
    auth, db, keys, oxide,
    reconciler::{validate_environment_name, validate_user_name},
    relay,
};
use dropshot::{ApiDescription, BuildError, ConfigDropshot};
use slog::{error, info, o};
use slog_error_chain::InlineErrorChain;
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Notify;
use vw_api::VwUserApi;
use vw_api_types_versions::v1::UserEnvironmentPathParam;

use crate::{Context, ServerArgs};

pub struct UserApi {}
impl VwUserApi for UserApi {
    type Context = Arc<Context>;

    async fn get_environments(
        rqctx: dropshot::RequestContext<Self::Context>,
    ) -> Result<
        dropshot::HttpResponseOk<
            dropshot::ResultsPage<vw_api_types_versions::latest::Environment>,
        >,
        dropshot::HttpError,
    > {
        let caller = auth::authorize_caller(rqctx).await?;
        let environments = db::list_user_environments(&caller.name)?;
        // This endpoint takes no pagination parameters, so the caller's
        // environments are always returned as one complete page. If a limit
        // and page selector are ever added to the endpoint, this becomes a
        // `ResultsPage::new` call with a selector keyed on environment name.
        Ok(dropshot::HttpResponseOk(dropshot::ResultsPage {
            next_page: None,
            items: environments,
        }))
    }

    async fn create_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
        body: dropshot::TypedBody<
            vw_api_types_versions::latest::EnvironmentCreate,
        >,
    ) -> Result<
        dropshot::HttpResponseCreated<
            vw_api_types_versions::latest::SshKeyPair,
        >,
        dropshot::HttpError,
    > {
        let reconcile = rqctx.context().reconcile.clone();
        let log = rqctx.log.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let name = path_params.into_inner().name;
        let requested = body.into_inner();

        // Both halves become part of an Oxide instance name, so both have to
        // survive being parsed back out of one.
        validate_environment_name(&name).map_err(|e| {
            info!(log, "rejecting environment name"; "name" => &name);
            dropshot::HttpError::for_bad_request(None, e)
        })?;
        validate_user_name(&caller.name).map_err(|e| {
            info!(log, "rejecting caller name"; "user" => &caller.name);
            dropshot::HttpError::for_bad_request(None, e)
        })?;

        // Pin the images now rather than at reconcile time, so publishing a
        // newer image never changes what an existing environment boots. This
        // is also where an explicitly named image gets validated.
        let images = if oxide::is_configured() {
            let session = oxide::session()?;
            Some(
                session
                    .resolve_images(
                        requested.vivado_image.as_deref(),
                        requested.helios_image.as_deref(),
                        requested.artifact_image.as_deref(),
                    )
                    .await
                    .inspect_err(|e| {
                        info!(log, "cannot resolve environment images";
                            "environment" => &name,
                            "error" => %e,
                        );
                    })?,
            )
        } else {
            // No rack to resolve against, so the environment is a bare
            // record. Accepting an image the service can neither validate nor
            // ever use would look like it took effect, so say so instead.
            if requested.vivado_image.is_some()
                || requested.helios_image.is_some()
                || requested.artifact_image.is_some()
            {
                info!(log, "image requested with no oxide backend";
                    "environment" => &name,
                );
                return Err(dropshot::HttpError::for_bad_request(
                    None,
                    String::from(
                        "this service has no oxide backend configured, so it \
                         cannot resolve or honor an image",
                    ),
                ));
            }
            None
        };

        let key = UserEnvironmentPathParam {
            user: caller.name.clone(),
            name,
        };
        // Every environment gets its own keypair, generated here and kept
        // alongside it. Without one the instances come up with no way in.
        let ssh_key =
            keys::generate(&key.user, &key.name).inspect_err(|e| {
                error!(log, "cannot generate an ssh key";
                    "user" => &key.user,
                    "environment" => &key.name,
                    InlineErrorChain::new(e),
                );
            })?;

        db::create_environment(key, images, &ssh_key)?;

        // Provision it now rather than on the next tick.
        reconcile.notify_one();

        // Handed back so the caller can save it straight away; the same pair
        // stays available from the keys endpoint.
        Ok(dropshot::HttpResponseCreated(ssh_key))
    }

    async fn sync_plan(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::TargetPathParam,
        >,
        body: dropshot::TypedBody<vw_api_types_versions::latest::TreeManifest>,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::SyncPlan>,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let target = path_params.into_inner();
        let manifest = body.into_inner();

        let agent = relay::Agent::resolve(
            &caller.name,
            &target.name,
            target.kind,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        // Every sync begins here, which makes this the place to hand the
        // instance the credentials its build will fetch dependencies with.
        // Sent each time rather than once: an instance rebuilt underneath us
        // comes back with none, and the failure that causes shows up much
        // later as a build that cannot reach a private repository.
        agent
            .give_credentials(&caller, &log)
            .await
            .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        let plan = agent
            .client
            .sync_plan(&agent.environment, &manifest)
            .await
            .map_err(|e| agent.failed(e))
            .inspect_err(|e| log_relay_failure(&log, &target, e))?
            .into_inner();

        Ok(dropshot::HttpResponseOk(plan))
    }

    async fn sync_blob(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::TargetBlobPathParam,
        >,
        body: dropshot::UntypedBody,
    ) -> Result<dropshot::HttpResponseUpdatedNoContent, dropshot::HttpError>
    {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let params = path_params.into_inner();
        let target = vw_api_types_versions::latest::TargetPathParam {
            name: params.name.clone(),
            kind: params.kind,
        };

        let agent = relay::Agent::resolve(
            &caller.name,
            &params.name,
            params.kind,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        agent
            .client
            .sync_blob(
                &agent.environment,
                params.digest.0.as_str(),
                body.as_bytes().to_vec(),
            )
            .await
            .map_err(|e| agent.failed(e))
            .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        Ok(dropshot::HttpResponseUpdatedNoContent())
    }

    async fn sync_commit(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::TargetPathParam,
        >,
        body: dropshot::TypedBody<vw_api_types_versions::latest::TreeManifest>,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::CommitResult>,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let target = path_params.into_inner();
        let manifest = body.into_inner();

        let agent = relay::Agent::resolve(
            &caller.name,
            &target.name,
            target.kind,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        let result = agent
            .client
            .sync_commit(&agent.environment, &manifest)
            .await
            .map_err(|e| agent.failed(e))
            .inspect_err(|e| log_relay_failure(&log, &target, e))?
            .into_inner();

        info!(log, "relayed a source sync";
            "environment" => &target.name,
            "target" => %target.kind,
            "created" => result.created,
            "updated" => result.updated,
            "deleted" => result.deleted,
            "unchanged" => result.unchanged,
        );

        Ok(dropshot::HttpResponseOk(result))
    }

    async fn sync_clear(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::TargetPathParam,
        >,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::CommitResult>,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let target = path_params.into_inner();

        let agent = relay::Agent::resolve(
            &caller.name,
            &target.name,
            target.kind,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        let result = agent
            .client
            .sync_clear(&agent.environment)
            .await
            .map_err(|e| agent.failed(e))
            .inspect_err(|e| log_relay_failure(&log, &target, e))?
            .into_inner();

        info!(log, "cleared a source tree";
            "environment" => &target.name,
            "target" => %target.kind,
            "deleted" => result.deleted,
        );

        Ok(dropshot::HttpResponseOk(result))
    }

    async fn clean_build_output(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::TargetPathParam,
        >,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::CleanResult>,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let target = path_params.into_inner();

        let agent = relay::Agent::resolve(
            &caller.name,
            &target.name,
            target.kind,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        let cleaned = agent
            .client
            .clean_build_output(&agent.environment)
            .await
            .map_err(|e| agent.failed(e))
            .inspect_err(|e| log_relay_failure(&log, &target, e))?
            .into_inner();

        info!(log, "removed build output";
            "environment" => &target.name,
            "target" => %target.kind,
            "bytes" => cleaned.bytes,
        );

        Ok(dropshot::HttpResponseOk(cleaned))
    }

    async fn bench_session(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
        query: dropshot::Query<vw_api_types_versions::latest::BenchQuery>,
        websock: dropshot::WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let name = path_params.into_inner().name;
        let query = query.into_inner();

        let target = vw_api_types_versions::latest::TargetPathParam {
            name: name.clone(),
            kind: vw_api_types_versions::latest::TargetKind::Vivado,
        };

        let agent = relay::Agent::resolve(
            &caller.name,
            &name,
            vw_api_types_versions::latest::TargetKind::Vivado,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        // A bench build fetches the workspace's dependencies like any other,
        // so the instance needs the caller's credentials before it starts.
        agent
            .give_credentials(&caller, &log)
            .await
            .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        info!(log, "running testbenches";
            "environment" => &name,
            "user" => &caller.name,
            "filter" => query.filter.as_deref().unwrap_or("-"),
        );

        let result = agent.join_bench_session(websock, &query).await;

        match &result {
            Ok(()) => info!(log, "testbench run ended";
                "environment" => &name,
            ),
            Err(e) => log_relay_failure(&log, &target, e),
        }

        result.map_err(Into::into)
    }

    async fn vivado_session(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
        query: dropshot::Query<
            vw_api_types_versions::latest::VivadoSessionQuery,
        >,
        websock: dropshot::WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult {
        let log = rqctx.log.clone();
        let args = rqctx.context().server_args.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let name = path_params.into_inner().name;
        let query = query.into_inner();

        let target = vw_api_types_versions::latest::TargetPathParam {
            name: name.clone(),
            kind: vw_api_types_versions::latest::TargetKind::Vivado,
        };

        let agent = relay::Agent::resolve(
            &caller.name,
            &name,
            vw_api_types_versions::latest::TargetKind::Vivado,
            &args,
        )
        .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        // The worker will want to fetch this build's dependencies, and the
        // credentials for that are the caller's. Sent before the session
        // opens, because once it does this service is only moving frames.
        agent
            .give_credentials(&caller, &log)
            .await
            .inspect_err(|e| log_relay_failure(&log, &target, e))?;

        info!(log, "joining a vivado session";
            "environment" => &name,
            "user" => &caller.name,
            "variant" => query.variant.as_deref().unwrap_or("-"),
        );

        let result = agent.join_vivado_session(websock, &query).await;

        match &result {
            Ok(()) => info!(log, "vivado session ended";
                "environment" => &name,
            ),
            Err(e) => log_relay_failure(&log, &target, e),
        }

        result.map_err(Into::into)
    }

    async fn get_environment_keys(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::SshKeyPair>,
        dropshot::HttpError,
    > {
        // Scoped to the caller like every other endpoint here, so the private
        // key only ever goes back to the environment's owner.
        let caller = auth::authorize_caller(rqctx).await?;
        let key = UserEnvironmentPathParam {
            user: caller.name.clone(),
            name: path_params.into_inner().name,
        };
        Ok(dropshot::HttpResponseOk(db::get_environment_keys(key)?))
    }

    async fn get_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
    ) -> Result<
        dropshot::HttpResponseOk<vw_api_types_versions::latest::Environment>,
        dropshot::HttpError,
    > {
        let caller = auth::authorize_caller(rqctx).await?;
        let key = UserEnvironmentPathParam {
            user: caller.name.clone(),
            name: path_params.into_inner().name,
        };
        let env = db::get_environment_status(key)?;
        Ok(dropshot::HttpResponseOk(env))
    }

    async fn delete_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::EnvironmentPathParam,
        >,
    ) -> Result<dropshot::HttpResponseDeleted, dropshot::HttpError> {
        let reconcile = rqctx.context().reconcile.clone();
        let caller = auth::authorize_caller(rqctx).await?;
        let key = UserEnvironmentPathParam {
            user: caller.name.clone(),
            name: path_params.into_inner().name,
        };
        db::delete_environment(key)?;

        // Tear the instances down now rather than on the next tick.
        reconcile.notify_one();

        Ok(dropshot::HttpResponseDeleted())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartServerError {
    #[error("Server build error {0}")]
    ServerBuildError(#[from] BuildError),
    #[error("Unexpected server exit {0}")]
    ServerExit(String),
}

pub async fn start_server(
    server_args: ServerArgs,
    log: slog::Logger,
    bind_address: SocketAddr,
    tls: Option<dropshot::ConfigTls>,
    reconcile: Arc<Notify>,
) -> Result<(), StartServerError> {
    let scheme = crate::tls::scheme(&server_args);
    let context = Arc::new(Context {
        server_args,
        reconcile,
    });
    let cfg = ConfigDropshot {
        bind_address,
        default_request_body_max_bytes: usize::MAX,
        ..Default::default()
    };
    let lg = log.new(o!("component" => "user_api"));
    let api = api_description();

    let server = dropshot::ServerBuilder::new(api, context, lg.clone())
        .config(cfg)
        .tls(tls)
        .start()?;

    info!(lg, "listening on {scheme}://{}", server.local_addr());

    Ok(server.await.map_err(|e| StartServerError::ServerExit(e))?)
}

pub fn api_description() -> ApiDescription<Arc<Context>> {
    vw_api::vw_user_api_mod::api_description::<UserApi>().unwrap()
}

/// Say why a sync could not be passed on.
///
/// Every one of these is worth a line: an instance that is not up yet, an
/// address that has not appeared, an agent that refused. None of it is visible
/// from the response alone, which only says the sync did not happen.
fn log_relay_failure(
    log: &slog::Logger,
    target: &vw_api_types_versions::latest::TargetPathParam,
    error: &relay::RelayError,
) {
    slog::warn!(log, "cannot relay a source sync";
        "environment" => &target.name,
        "target" => %target.kind,
        InlineErrorChain::new(error),
    );
}
