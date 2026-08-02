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
use vw_api_types_versions::latest::{
    CommitResult, Credentials, SyncPlan, TreeManifest,
};
use vw_sync::Store;
use vw_sync_api::{BlobPathParam, EnvironmentPathParam, VwSyncApi};

mod artifacts;
mod error;
mod garage;
mod netrc;

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
    /// Run exactly one testbench into an isolated build directory.
    ///
    /// Hidden because it is not for people: the batch runner fans one of
    /// these out per bench, the same way `vw bench` does on a developer's
    /// machine. A child per bench is what makes each one's output separable
    /// and each one's build directory its own — `nvc` inherits stdio, so
    /// several in one process would interleave beyond recovery.
    #[command(hide = true)]
    BenchOne(BenchOneArgs),
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

    /// Where to remember the object store artifacts are uploaded to.
    ///
    /// Kept so an instance that reboots between builds still knows where its
    /// output goes without being told again.
    #[arg(long, default_value = "/var/lib/vw-agent/artifact-target.json")]
    artifact_target: Utf8PathBuf,

    /// Directory the object store keeps its config, metadata and data in.
    ///
    /// Only consulted on an artifact instance, which is the only kind that
    /// runs a store.
    #[arg(long, default_value = "/var/lib/vw-agent/garage")]
    garage_dir: Utf8PathBuf,

    /// Port the object store serves S3 on.
    ///
    /// Garage's own default, and what every uploader is told to expect.
    #[arg(long, default_value_t = 3900u16)]
    s3_port: u16,

    /// Port the object store's admin API listens on, bound to localhost.
    #[arg(long, default_value_t = 3903u16)]
    garage_admin_port: u16,

    /// Port the object store uses to talk to itself.
    #[arg(long, default_value_t = 3901u16)]
    garage_rpc_port: u16,

    /// How much of this instance's disk the object store may use.
    #[arg(long, default_value = "100G")]
    garage_capacity: String,

    /// Where to write the credentials a build fetches its dependencies with.
    ///
    /// Defaults to the `.netrc` of whoever the agent runs as. Provisioning
    /// should point this at the home directory of the user builds run as when
    /// that is somebody else — `/home/ubuntu/.netrc` on the vivado and
    /// artifact instances, where the agent is root and the build is not.
    #[arg(long)]
    netrc: Option<Utf8PathBuf>,
}

#[derive(Parser, Clone)]
struct BenchOneArgs {
    /// Workspace to run in.
    #[arg(long)]
    root: Utf8PathBuf,
    /// The testbench entity to run.
    #[arg(long)]
    name: String,
    /// Where nvc should build, relative to the workspace.
    #[arg(long)]
    build_dir: String,
    /// The VHDL standard, as `nvc` spells it.
    #[arg(long)]
    std: String,
}

pub struct Context {
    environment: String,
    /// Where finished artifacts are uploaded, once anyone has said.
    artifact_target: tokio::sync::watch::Sender<
        Option<vw_api_types_versions::latest::S3Credentials>,
    >,
    /// Where that answer is kept across restarts.
    artifact_target_path: Utf8PathBuf,
    /// The object store this instance runs, if it is the one that runs it.
    ///
    /// Held so garage lives as long as the agent does, and so the key can be
    /// handed to whoever asks for it. Distinct from `store` below, which is
    /// where delivered source waits — this one is where finished artifacts go.
    object_store: Option<garage::Store>,
    root: Utf8PathBuf,
    store: Store,
    /// Where the credentials for fetching dependencies are kept.
    netrc: Utf8PathBuf,
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

    async fn get_artifact_target(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
    ) -> Result<
        HttpResponseOk<vw_api_types_versions::latest::S3Credentials>,
        HttpError,
    > {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        let current = ctx.artifact_target.borrow().clone();
        let Some(current) = current else {
            slog::debug!(rqctx.log, "no artifact target has been set");
            return Err(HttpError::for_not_found(
                None,
                String::from(
                    "this instance has not been told where its \
                              artifacts go",
                ),
            ));
        };

        Ok(HttpResponseOk(current))
    }

    async fn put_artifact_target(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        body: dropshot::TypedBody<vw_api_types_versions::latest::S3Credentials>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;
        let credentials = body.into_inner();

        artifacts::remember(&ctx.artifact_target_path, &credentials)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot remember the artifact target";
                    "path" => %ctx.artifact_target_path,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(|e| HttpError::for_internal_error(e.to_string()))?;

        info!(rqctx.log, "artifacts now go to a store";
            "bucket" => &credentials.bucket,
            "endpoint" => &credentials.endpoint,
        );

        // Waking the uploader, which will send anything already built.
        let _ = ctx.artifact_target.send(Some(credentials));

        Ok(HttpResponseUpdatedNoContent())
    }

    async fn get_object_store(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        query: dropshot::Query<vw_api_types_versions::latest::ObjectStoreQuery>,
    ) -> Result<
        HttpResponseOk<vw_api_types_versions::latest::S3Credentials>,
        HttpError,
    > {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        let Some(store) = ctx.object_store.as_ref() else {
            slog::warn!(
                rqctx.log,
                "asked for an object store this instance \
                                    does not run"
            );
            return Err(HttpError::for_not_found(
                None,
                String::from(
                    "this instance does not run an object store; the artifact \
                     instance does",
                ),
            ));
        };

        let kind = query
            .into_inner()
            .kind
            .unwrap_or(vw_api_types_versions::latest::TargetKind::Vivado)
            .to_string();

        let Some(bucket) = store.buckets.get(&kind) else {
            slog::warn!(rqctx.log, "asked for a bucket that does not exist";
                "kind" => &kind,
            );
            return Err(HttpError::for_not_found(
                None,
                format!("this store has no bucket for '{kind}'"),
            ));
        };

        info!(rqctx.log, "handing out the object store key";
            "bucket" => bucket,
        );

        let mut credentials = store.credentials.clone();
        credentials.bucket = bucket.clone();

        Ok(HttpResponseOk(credentials))
    }

    async fn clean_build_output(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
    ) -> Result<
        HttpResponseOk<vw_api_types_versions::latest::CleanResult>,
        HttpError,
    > {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        // The same lock a commit takes. Removing the build output while a
        // tree is being written would not corrupt either — they are disjoint
        // — but a build starting in between would see half a world.
        let _guard = ctx.materializing.lock().await;
        let cleaned = vw_sync::clean(&ctx.root)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot remove the build output";
                    "root" => %ctx.root,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::apply_error)?;

        info!(rqctx.log, "build output removed";
            "existed" => cleaned.existed,
            "bytes" => cleaned.bytes,
        );

        Ok(HttpResponseOk(vw_api_types_versions::latest::CleanResult {
            existed: cleaned.existed,
            bytes: cleaned.bytes,
        }))
    }

    async fn bench_session(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        query: dropshot::Query<vw_api_types_versions::latest::BenchQuery>,
        websock: dropshot::WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        let query = query.into_inner();
        // The instance decides how many run at once when the client does not
        // say, because the instance is the machine doing the work and knows
        // what it has.
        let concurrency =
            query.concurrency.map(|n| n as usize).unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
        let request = vw_bench::Request {
            filter: query.filter.clone(),
            standard: query
                .standard
                .clone()
                .unwrap_or_else(|| "2019".to_owned()),
            concurrency,
            ignore: query.ignored(),
        };

        info!(rqctx.log, "running testbenches";
            "root" => %ctx.root,
            "filter" => query.filter.as_deref().unwrap_or("-"),
            "concurrency" => concurrency,
        );

        // One child per bench, and the child is this same binary. It already
        // knows how to run exactly one bench into its own directory, which is
        // what the hidden `bench-one` mode is for.
        let exe = std::env::current_exe()?;
        let root = ctx.root.clone();
        let standard = request.standard.clone();
        let launch: vw_bench::Launch =
            std::sync::Arc::new(move |name: &str, build_dir: &str| {
                let mut command = tokio::process::Command::new(&exe);
                command.args([
                    "bench-one",
                    "--root",
                    root.as_str(),
                    "--name",
                    name,
                    "--build-dir",
                    build_dir,
                    "--std",
                    &standard,
                ]);
                command
            });

        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            websock.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        let result =
            vw_remote::bench::serve(socket, &ctx.root, request, launch).await;

        match &result {
            Ok(()) => info!(rqctx.log, "testbenches finished"),
            Err(e) => slog::error!(rqctx.log, "testbench run failed";
                InlineErrorChain::new(e),
            ),
        }

        result.map_err(Into::into)
    }

    async fn vivado_session(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        query: dropshot::Query<
            vw_api_types_versions::latest::VivadoSessionQuery,
        >,
        websock: dropshot::WebsocketConnection,
    ) -> dropshot::WebsocketChannelResult {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;

        let query = query.into_inner();
        let params = vw_remote::SessionParams {
            part: query.part.clone(),
            variant: query.variant.clone(),
            info_with_stack: query.info_with_stack,
            verbose: query.verbose,
        };

        info!(rqctx.log, "starting a vivado session";
            "root" => %ctx.root,
            "part" => query.part.as_deref().unwrap_or("-"),
            "variant" => query.variant.as_deref().unwrap_or("-"),
        );

        let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            websock.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        let result = vw_remote::serve(socket, &ctx.root, params).await;

        match &result {
            Ok(()) => info!(rqctx.log, "vivado session finished"),
            Err(e) => slog::error!(rqctx.log, "vivado session failed";
                InlineErrorChain::new(e),
            ),
        }

        result.map_err(Into::into)
    }

    async fn put_credentials(
        rqctx: RequestContext<Self::Context>,
        path_params: dropshot::Path<EnvironmentPathParam>,
        body: dropshot::TypedBody<Credentials>,
    ) -> Result<HttpResponseUpdatedNoContent, HttpError> {
        let ctx = rqctx.context();
        ctx.check_environment(
            &path_params.into_inner().environment,
            &rqctx.log,
        )?;
        let credentials = body.into_inner();

        netrc::write(&ctx.netrc, &credentials)
            .inspect_err(|e| {
                slog::error!(rqctx.log, "cannot write the credentials file";
                    "path" => %ctx.netrc,
                    InlineErrorChain::new(e),
                );
            })
            .map_err(error::netrc_error)?;

        // The login and the path, never the token.
        info!(rqctx.log, "credentials in place";
            "user" => &credentials.user,
            "path" => %ctx.netrc,
        );

        Ok(HttpResponseUpdatedNoContent())
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
        Commands::BenchOne(args) => bench_one(args).await,
        Commands::EmitSpec => emit_spec(),
    }
}

/// Run one testbench and exit with whether it passed.
///
/// Output goes to this process's own stdout and stderr, where the parent
/// captures it — that is the whole point of being a separate process.
async fn bench_one(args: BenchOneArgs) {
    let standard = match args.std.parse::<vw_lib::VhdlStandard>() {
        Ok(standard) => standard,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let result = vw_lib::run_testbench(
        &args.root,
        args.name,
        standard,
        true,
        &[],
        false,
        false,
        &args.build_dir,
    )
    .await;

    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
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

    // Resolved at startup rather than when the first credentials arrive, so
    // an agent with nowhere to put them says so now instead of halfway
    // through somebody's first sync.
    let netrc = match netrc_path(&args) {
        Some(path) => path,
        None => {
            slog::error!(log, "cannot work out where to write credentials";
                "detail" => "HOME is not set and --netrc was not given",
            );
            std::process::exit(1);
        }
    };

    // The artifact instance is the one that holds the object store, so it is
    // the one that brings it up. Nothing is pre-shared: the admin credential
    // is generated here and stays here, and only an S3 key ever leaves.
    let store = if args.kind == "artifact" {
        match garage::start(
            &args.environment,
            &garage::Settings {
                dir: args.garage_dir.clone(),
                s3_port: args.s3_port,
                admin_port: args.garage_admin_port,
                rpc_port: args.garage_rpc_port,
                capacity: args.garage_capacity.clone(),
            },
            &log,
        )
        .await
        {
            Ok(store) => Some(store),
            Err(e) => {
                slog::error!(log, "cannot bring up the object store";
                    InlineErrorChain::new(&e),
                );
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // Where artifacts go, recovered from the last time anyone said. An agent
    // that has never been told simply uploads nothing.
    let remembered = artifacts::recall(&args.artifact_target)
        .inspect_err(|e| {
            slog::warn!(log, "cannot read the remembered artifact target";
                InlineErrorChain::new(e),
            );
        })
        .unwrap_or(None);
    if let Some(target) = &remembered {
        info!(log, "artifacts go to a store this instance was told about";
            "bucket" => &target.bucket,
        );
    }
    let (artifact_target, artifact_changes) =
        tokio::sync::watch::channel(remembered);

    // Only where builds happen. An artifact instance holds the store rather
    // than filling it, and helios does not produce images.
    if args.kind == "vivado" {
        tokio::spawn(artifacts::synchronize(
            args.root.clone(),
            artifact_changes,
            log.new(o!("task" => "artifacts")),
        ));
    }

    let context = Arc::new(Context {
        environment: args.environment.clone(),
        artifact_target,
        artifact_target_path: args.artifact_target.clone(),
        object_store: store,
        root: args.root.clone(),
        store: Store::new(args.store.clone()),
        netrc: netrc.clone(),
        materializing: tokio::sync::Mutex::new(()),
    });

    info!(log, "serving source synchronization";
        "root" => %args.root,
        "store" => %args.store,
        "netrc" => %netrc,
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

/// Where credentials go, if it can be worked out at all.
fn netrc_path(args: &ServerArgs) -> Option<Utf8PathBuf> {
    if let Some(path) = &args.netrc {
        return Some(path.clone());
    }
    std::env::var("HOME")
        .ok()
        .map(|home| Utf8PathBuf::from(home).join(".netrc"))
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
