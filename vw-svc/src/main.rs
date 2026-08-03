use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use slog::{error, info, warn, Drain, Logger};
use std::{
    io::stdout,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Notify;

mod admin_api;
mod artifacts;
mod auth;
mod db;
mod error;
mod keys;
mod oxide;
mod reconciler;
mod relay;
mod tls;
mod user_api;
mod wiring;

pub struct Context {
    server_args: ServerArgs,
    /// Rung when an environment is created or deleted so the reconciler acts
    /// on it right away instead of waiting out its interval.
    reconcile: Arc<Notify>,
}

#[derive(Parser)]
#[command(name = "vw-svc")]
#[command(about = "vw service")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server
    ///
    /// Boxed because it carries every one of the service's settings while the
    /// other two variants carry nothing, and an enum is as large as its
    /// largest variant — so emitting a spec would otherwise move a few hundred
    /// bytes of server configuration around for no reason.
    Serve(Box<ServerArgs>),
    /// Emit the user OpenAPI spec
    EmitUserSpec,
    /// Emit the admin OpenAPI spec
    EmitAdminSpec,
}

#[derive(Parser, Clone)]
struct ServerArgs {
    /// Server bind address
    #[arg(long, default_value_t = IpAddr::V6(Ipv6Addr::UNSPECIFIED))]
    address: IpAddr,
    #[arg(long, default_value_t = 2727u16)]
    user_api_port: u16,
    #[arg(long, default_value_t = 2728u16)]
    admin_api_port: u16,
    /// Enable TLS
    #[arg(long)]
    tls: bool,
    /// TLS certificate file path
    #[arg(long, default_value = "cert.pem")]
    cert_file: Utf8PathBuf,
    /// TLS private key file path
    #[arg(long, default_value = "key.pem")]
    key_file: Utf8PathBuf,
    /// Do not require a github token for API access
    #[arg(long)]
    no_auth: bool,
    #[arg(long)]
    admin_users: Vec<String>,
    /// Path to the environment database, created if it does not exist
    #[arg(long, default_value = "vw-svc.redb")]
    db_path: Utf8PathBuf,

    /// The Oxide API endpoint to use.
    ///
    /// Together with --oxide-token this is what connects the service to a
    /// rack. With neither set the service keeps environment records but
    /// provisions nothing.
    #[arg(long, requires = "oxide_token")]
    oxide_api_endpoint: Option<String>,

    /// The token to use
    #[arg(long, env = "OXIDE_TOKEN", requires = "oxide_api_endpoint")]
    oxide_token: Option<String>,

    /// The Oxide project instances are created in
    #[arg(long, default_value = "redhawk")]
    oxide_project: String,

    /// Seconds between reconciler passes
    #[arg(long, default_value_t = 30)]
    reconcile_interval: u64,

    /// Send vivado source to this address instead of to a rack instance.
    ///
    /// For running the whole stack on one machine: with no Oxide backend there
    /// are no instances to look up, so there is nowhere for a sync to go. An
    /// address here stands in for the instance the reconciler would otherwise
    /// have recorded.
    #[arg(long, value_name = "HOST:PORT")]
    vivado_agent: Option<String>,

    /// Send helios source to this address instead of to a rack instance.
    #[arg(long, value_name = "HOST:PORT")]
    helios_agent: Option<String>,

    /// Ask this address for the environment's object store, rather than the
    /// artifact instance the reconciler would have recorded.
    #[arg(long, value_name = "HOST:PORT")]
    artifact_agent: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve(args) => serve(*args).await,
        Commands::EmitUserSpec => emit_user_spec(),
        Commands::EmitAdminSpec => emit_admin_spec(),
    };
}

async fn serve(args: ServerArgs) {
    let log = logger();
    db::init(&args.db_path).expect("unable to open environment database");

    if let Err(e) = oxide::init(
        args.oxide_api_endpoint.as_deref(),
        args.oxide_token.as_deref(),
        &args.oxide_project,
    ) {
        error!(log, "oxide configuration error";
            slog_error_chain::InlineErrorChain::new(&e),
        );
        std::process::exit(1);
    }
    if oxide::is_configured() {
        // Not fatal either way: the rack may come back, and the reconciler
        // opens its own session every pass. Say so loudly rather than looking
        // healthy.
        let endpoint = args.oxide_api_endpoint.as_deref().unwrap_or("");
        match oxide::session() {
            Ok(session) => {
                if let Err(e) = session.ping(&log).await {
                    warn!(log, "oxide api probe failed";
                        "endpoint" => endpoint,
                        slog_error_chain::InlineErrorChain::new(&e),
                    );
                }
            }
            Err(e) => {
                warn!(log, "cannot open a session with the oxide api";
                    "endpoint" => endpoint,
                    slog_error_chain::InlineErrorChain::new(&e),
                );
            }
        }
    } else {
        warn!(
            log,
            "no oxide backend configured; environments will be recorded but \
             never provisioned. Pass --oxide-api-endpoint and --oxide-token \
             to reconcile instances."
        );
    }
    //let addr: IpAddr = args.address.parse().expect("unable to parse address");
    let user_sa = SocketAddr::new(args.address, args.user_api_port);
    let admin_sa = SocketAddr::new(args.address, args.admin_api_port);

    // Resolve TLS before either server starts, so a bad certificate path is a
    // startup failure with a message naming it rather than a half-up service.
    // This also starts the watch that notices renewals.
    let tls = match tls::config(&args, &log) {
        Ok(tls) => tls,
        Err(e) => {
            error!(log, "tls configuration error";
                slog_error_chain::InlineErrorChain::new(&e),
            );
            std::process::exit(1);
        }
    };
    if tls.is_some() {
        info!(log, "serving tls";
            "cert_file" => %args.cert_file,
            "key_file" => %args.key_file,
        );
    }

    // Shared between the API servers and the reconciler: creating or deleting
    // an environment rings it so a pass runs immediately.
    let reconcile = Arc::new(Notify::new());

    // The reconciler is only useful with a rack behind it.
    if oxide::is_configured() {
        let reconciler = reconciler::InstanceReconciler::new(
            Duration::from_secs(args.reconcile_interval),
            reconcile.clone(),
        );
        let log = log.new(slog::o!("component" => "reconciler"));
        info!(log, "starting reconciler";
            "interval_secs" => args.reconcile_interval,
        );
        tokio::spawn(async move { reconciler.run(log).await });
    }

    // Environments outlive this service, so some of them may have been created
    // while their instances were still coming up, or have had their object
    // store rebuilt since. Put them right now rather than waiting for someone
    // to synchronize source — an environment nobody has synced since the last
    // restart would otherwise build images that went nowhere.
    //
    // In the background: an instance that is down should delay nothing, and
    // the API can serve while this works through them.
    {
        let args = args.clone();
        let log = log.new(slog::o!("component" => "artifacts"));
        tokio::spawn(async move { wiring::ensure_all(&args, &log).await });
    }

    // Both servers run for the life of the process, so they have to be driven
    // concurrently. Awaiting them in sequence would leave the second one
    // never started. If either stops, the whole service is done.
    let user = user_api::start_server(
        args.clone(),
        log.clone(),
        user_sa,
        tls.clone(),
        reconcile.clone(),
    );
    let admin =
        admin_api::start_server(args, log.clone(), admin_sa, tls, reconcile);

    tokio::try_join!(
        async { user.await.map_err(|e| format!("user api stopped: {e}")) },
        async { admin.await.map_err(|e| format!("admin api stopped: {e}")) },
    )
    .expect("api server stopped");
}

fn emit_user_spec() {
    let api = user_api::api_description();
    let spec = api.openapi("VW user API", vw_api::latest_version());
    let mut out = stdout();
    spec.write(&mut out).expect("write spec to stdout");
}

fn emit_admin_spec() {
    let api = admin_api::api_description();
    let spec = api.openapi("VW admin API", vw_api::latest_version());
    let mut out = stdout();
    spec.write(&mut out).expect("write spec to stdout");
}

fn logger() -> Logger {
    let drain = slog_bunyan::new(std::io::stdout()).build().fuse();
    let drain = slog_async::Async::new(drain)
        .chan_size(0x8000)
        .build()
        .fuse();
    Logger::root(drain, slog::o!())
}
