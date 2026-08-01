// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Receives source on a vw build instance.
//!
//! One agent serves one environment on one instance. It takes delivery of
//! content from `vw-svc` over the rack's internal network and keeps a
//! directory matching whatever the developer's machine last said it should
//! look like — so vivado, nvc and cargo find ordinary files where they expect
//! them, with no knowledge that any of this happened.
//!
//! It is deliberately incurious. It does not know which developer it is
//! working for, whether they are allowed to be, or what is going to be built:
//! `vw-svc` settles all of that before relaying anything here.

use camino::Utf8PathBuf;
use clap::Parser;
use dropshot::{
    ApiDescription, ConfigDropshot, HttpError, HttpResponseOk,
    HttpResponseUpdatedNoContent, RequestContext,
};
use slog::{info, o, Drain, Logger};
use slog_error_chain::InlineErrorChain;
use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
};
use vw_api_types_versions::latest::{CommitResult, SyncPlan, TreeManifest};
use vw_sync::Store;
use vw_sync_api::{BlobPathParam, EnvironmentPathParam, VwSyncApi};

mod error;

#[derive(Parser)]
#[command(name = "vw-agent")]
#[command(about = "receives source on a vw build instance")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Run the agent
    Serve(ServerArgs),
    /// Emit the OpenAPI spec
    EmitSpec,
}

#[derive(Parser, Clone)]
struct ServerArgs {
    /// Address to listen on
    #[arg(long, default_value_t = IpAddr::V6(Ipv6Addr::UNSPECIFIED))]
    address: IpAddr,

    #[arg(long, default_value_t = 2729u16)]
    port: u16,

    /// The environment this agent belongs to.
    ///
    /// Requests naming any other environment are refused. An agent is brought
    /// up for one environment and stays that way, so a request for a different
    /// one is a routing mistake rather than something to serve.
    #[arg(long)]
    environment: String,

    /// Which half of the environment this is.
    ///
    /// Only used to say so in the logs today. It is what the two roles will
    /// diverge on once the agent starts running builds rather than only
    /// receiving the source for them.
    #[arg(long, default_value = "vivado")]
    kind: String,

    /// Directory the source tree is kept in.
    ///
    /// Fixed at startup and never derived from a request, so nothing a caller
    /// sends can place a file outside it.
    #[arg(long)]
    root: Utf8PathBuf,

    /// Directory delivered content waits in before being put in place.
    #[arg(long, default_value = "/var/lib/vw-agent/store")]
    store: Utf8PathBuf,
}

pub struct Context {
    environment: String,
    root: Utf8PathBuf,
    store: Store,
    /// Held while a tree is being made to match a manifest.
    ///
    /// Two machines synchronizing the same environment is not a conflict worth
    /// reporting — the later one wins — but it is worth making sure the loser
    /// does not leave half of its tree interleaved with half of the winner's.
    materializing: tokio::sync::Mutex<()>,
}

pub struct Agent {}

impl VwSyncApi for Agent {
    type Context = Arc<Context>;

    async fn sync_plan(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        body: dropshot::TypedBody<TreeManifest>,
    ) -> Result<HttpResponseOk<SyncPlan>, HttpError> {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        let manifest = body.into_inner();
        let plan = vw_sync::missing(&ctx.root, &ctx.store, &manifest)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot work out what is missing";
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::apply_error)?;

        info!(rqctx.log, "planned a sync";
            "wanted" => manifest.entries.len(),
            "missing" => plan.missing.len(),
        );

        Ok(HttpResponseOk(plan))
    }

    async fn sync_blob(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<BlobPathParam>,
        body: dropshot::UntypedBody,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        let ctx = rqctx.context();
        let params = path_params.into_inner();
        ctx.check_environment(&params.environment, &rqctx.log)?;

        ctx.store
            .put(&params.digest, body.as_bytes())
            .inspect_err(|e| {
                slog::error!(rqctx.log, "rejected delivered content";
                    "digest" => %params.digest,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::store_error)?;

        Ok(HttpResponseUpdatedNoContent())
    }

    async fn sync_commit(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        body: dropshot::TypedBody<TreeManifest>,
    ) -> Result<HttpResponseOk<CommitResult>, HttpError> {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;
        let manifest = body.into_inner();

        let _guard = ctx.materializing.lock().await;
        let result = vw_sync::apply(&ctx.root, &ctx.store, &manifest)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot make the tree match";
                    "root" => %ctx.root,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::apply_error)?;

        info!(rqctx.log, "tree synchronized";
            "created" => result.created,
            "updated" => result.updated,
            "deleted" => result.deleted,
            "unchanged" => result.unchanged,
        );

        Ok(HttpResponseOk(result))
    }

    async fn sync_clear(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
    ) -> Result<HttpResponseOk<CommitResult>, HttpError> {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        // The same lock a commit takes: this is a commit, of an empty
        // manifest, and it would be no better to interleave with one than two
        // commits would be with each other.
        let _guard = ctx.materializing.lock().await;
        let result = vw_sync::clear(&ctx.root, &ctx.store)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot clear the tree";
                    "root" => %ctx.root,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::apply_error)?;

        info!(rqctx.log, "tree cleared";
            "deleted" => result.deleted,
        );

        Ok(HttpResponseOk(result))
    }
}

impl Context {
    /// Refuse a request meant for a different environment.
    fn check_environment(
        &self,
        environment: &str,
        log: &Logger,
    ) -> Result<(), HttpError> {
        if environment == self.environment {
            return Ok(());
        }

        slog::warn!(log, "request for an environment this agent does not serve";
            "wanted" => environment,
            "serving" => &self.environment,
        );
        Err(HttpError::for_not_found(
            None,
            format!(
                "this agent serves '{}', not '{environment}'",
                self.environment
            ),
        ))
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve(args) => serve(args).await,
        Commands::EmitSpec => emit_spec(),
    }
}

async fn serve(args: ServerArgs) {
    let log = logger().new(o!(
        "environment" => args.environment.clone(),
        "kind" => args.kind.clone(),
    ));

    // Both are made now rather than on the first request, so a directory that
    // cannot be created is a startup failure naming it rather than a confusing
    // error in the middle of somebody's first sync.
    for directory in [&args.root, &args.store] {
        if let Err(e) = std::fs::create_dir_all(directory) {
            slog::error!(log, "cannot create a directory the agent needs";
                "path" => %directory,
                InlineErrorChain::new(&e),
            );
            std::process::exit(1);
        }
    }

    let context = Arc::new(Context {
        environment: args.environment.clone(),
        root: args.root.clone(),
        store: Store::new(args.store.clone()),
        materializing: tokio::sync::Mutex::new(()),
    });

    info!(log, "serving source synchronization";
        "root" => %args.root,
        "store" => %args.store,
    );

    let server =
        dropshot::ServerBuilder::new(api_description(), context, log.clone())
            .config(ConfigDropshot {
                bind_address: SocketAddr::new(args.address, args.port),
                // A manifest is one JSON document listing every file in the tree, and
                // a blob is one whole source file. Neither is large, but the default
                // of a few kilobytes is smaller than either.
                default_request_body_max_bytes: 128 * 1024 * 1024,
                ..Default::default()
            })
            .start();

    let server = match server {
        Ok(server) => server,
        Err(e) => {
            slog::error!(log, "cannot start the agent";
                InlineErrorChain::new(&e),
            );
            std::process::exit(1);
        }
    };

    info!(log, "listening on http://{}", server.local_addr());

    if let Err(e) = server.await {
        slog::error!(log, "agent stopped"; "error" => e);
        std::process::exit(1);
    }
}

fn emit_spec() {
    let api = api_description();
    let spec = api.openapi("VW agent API", vw_sync_api::latest_version());
    spec.write(&mut std::io::stdout())
        .expect("write spec to stdout");
}

pub fn api_description() -> ApiDescription<Arc<Context>> {
    vw_sync_api::vw_sync_api_mod::api_description::<Agent>()
        .expect("the api description is built from a trait that compiles")
}

fn logger() -> Logger {
    let drain = slog_bunyan::new(std::io::stdout()).build().fuse();
    let drain = slog_async::Async::new(drain)
        .chan_size(0x8000)
        .build()
        .fuse();
    Logger::root(drain, o!())
}
