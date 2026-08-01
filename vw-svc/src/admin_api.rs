//! This module implements the user api trait `[vw_api::VwAdminApi]`
use crate::{Context, ServerArgs};
use dropshot::{ApiDescription, BuildError, ConfigDropshot};
use slog::{info, o};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Notify;
use vw_api::VwAdminApi;

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
        todo!()
    }

    async fn delete_environment(
        rqctx: dropshot::RequestContext<Self::Context>,
        path_params: dropshot::Path<
            vw_api_types_versions::latest::UserEnvironmentPathParam,
        >,
    ) -> Result<dropshot::HttpResponseDeleted, dropshot::HttpError> {
        todo!()
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

    Ok(server.await.map_err(|e| StartServerError::ServerExit(e))?)
}

pub fn api_description() -> ApiDescription<Arc<Context>> {
    vw_api::vw_admin_api_mod::api_description::<AdminApi>().unwrap()
}
