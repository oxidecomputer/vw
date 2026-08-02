//! This module implements the admin api trait `[vw_api::VwAdminApi]`
//!
//! Everything here reaches across users, which is the whole reason it is a
//! separate API on a separate port: the user API can only ever see the caller's
//! own environments, and that property is easier to keep when the endpoints
//! that break it do not sit beside it.

use crate::{auth, db, Context, ServerArgs};
use dropshot::{ApiDescription, BuildError, ConfigDropshot};
use slog::{info, o};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Notify;
use vw_api::VwAdminApi;
use vw_api_types_versions::latest::UserEnvironmentPathParam;

pub struct AdminApi {}
impl VwAdminApi for AdminApi {
    type Context = Arc<Context>;

    async fn get_environments(
        rqctx: dropshot::RequestContext<Self::Context>,
    ) -> Result<
        dropshot::HttpResponseOk<
            dropshot::ResultsPage<
                vw_api_types_versions::latest::UserEnvironment,
            >,
        >,
        dropshot::HttpError,
    > {
        let log = rqctx.log.clone();
        let caller = auth::authorize_administrator(rqctx).await?;

        let environments = db::list_all_environments().inspect_err(|e| {
            slog::error!(log, "cannot list environments";
                slog_error_chain::InlineErrorChain::new(e),
            );
        })?;

        info!(log, "listed every environment";
            "administrator" => &caller.name,
            "environments" => environments.len(),
        );

        // One complete page: this endpoint takes no pagination parameters, and
        // the number of environments on a rack is bounded by the number of
        // developers using it. If that ever stops being true this becomes a
        // `ResultsPage::new` with a selector keyed on user and name.
        Ok(dropshot::HttpResponseOk(dropshot::ResultsPage {
            next_page: None,
            items: environments,
        }))
    }

    async fn delete_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::UserEnvironmentPathParam,
        >,
    ) -> Result<dropshot::HttpResponseDeleted, dropshot::HttpError> {
        let log = rqctx.log.clone();
        let reconcile = rqctx.context().reconcile.clone();
        let caller = auth::authorize_administrator(rqctx).await?;
        let key: UserEnvironmentPathParam = path_params.into_inner();

        // Named in the log before it happens as well as after: this is one
        // person removing another person's work, and the record of who did it
        // should survive whatever the deletion does next.
        info!(log, "deleting an environment on behalf of the service";
            "administrator" => &caller.name,
            "user" => &key.user,
            "environment" => &key.name,
        );

        db::delete_environment(key.clone()).inspect_err(|e| {
            slog::error!(log, "cannot delete an environment";
                "user" => &key.user,
                "environment" => &key.name,
                slog_error_chain::InlineErrorChain::new(e),
            );
        })?;

        // Tear the instances down now rather than on the next tick, so a rack
        // an administrator is reclaiming starts emptying immediately.
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
    let lg = log.new(o!("component" => "admin_api"));
    let api = api_description();

    let server = dropshot::ServerBuilder::new(api, context, lg.clone())
        .config(cfg)
        .tls(tls)
        .start()?;

    info!(lg, "listening on {scheme}://{}", server.local_addr());

    server.await.map_err(StartServerError::ServerExit)
}

pub fn api_description() -> ApiDescription<Arc<Context>> {
    vw_api::vw_admin_api_mod::api_description::<AdminApi>().unwrap()
}
