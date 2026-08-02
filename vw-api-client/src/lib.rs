//! Progenitor generated clients for the vw service APIs.
//!
//! The user and admin APIs are served by two separate dropshot servers on two
//! separate ports, so each gets its own client in its own module.
//!
//! The OpenAPI documents these are generated from live in `openapi/` at the
//! root of the repository and belong to `vw-openapi-manager`: run
//! `cargo openapi generate` after changing an endpoint. Each is read through
//! the `-latest.json` symlink the manager maintains, so this crate does not
//! have to be edited when the API version changes. `cargo openapi check`
//! (also run as a test) fails if a document is out of date, which keeps these
//! clients from drifting from the service.
//!
//! Both APIs identify the caller by a Github access token in the authorization
//! header of every request. Rather than make each call site remember that,
//! [`user_client`] and [`admin_client`] build clients with the header already
//! attached. The token is optional: a service run with `--no-auth` answers
//! without one.

use reqwest::header::{
    HeaderMap, HeaderValue, InvalidHeaderValue, AUTHORIZATION,
};

/// Client for the vw user API.
pub mod user {
    // Sync types are reused rather than regenerated, for the same reason as in
    // `agent` below: a client and a relay passing structurally identical but
    // incompatible spellings of the same manifest would need a conversion at
    // every hop.
    progenitor::generate_api!(
        spec = "../openapi/vw-user-api/vw-user-api-latest.json",
        replace = {
            Artifact = vw_api_types_versions::latest::Artifact,
            CleanResult = vw_api_types_versions::latest::CleanResult,
            CommitResult = vw_api_types_versions::latest::CommitResult,
            Digest = vw_api_types_versions::latest::Digest,
            FileEntry = vw_api_types_versions::latest::FileEntry,
            SyncPlan = vw_api_types_versions::latest::SyncPlan,
            TargetKind = vw_api_types_versions::latest::TargetKind,
            TreeManifest = vw_api_types_versions::latest::TreeManifest,
        },
    );
}

/// Client for the agent that runs on a build instance.
///
/// Used by `vw-svc` to relay source, not by anything on a developer's machine
/// — the agents are only reachable from inside the rack.
pub mod agent {
    // The shared types are reused rather than regenerated. Progenitor would
    // otherwise mint its own `TreeManifest` and `Digest`, structurally
    // identical to the ones in `vw-api-types` and incompatible with them, and
    // every relayed request would have to be copied field by field between two
    // spellings of the same thing.
    progenitor::generate_api!(
        spec = "../openapi/vw-sync-api/vw-sync-api-latest.json",
        replace = {
            CleanResult = vw_api_types_versions::latest::CleanResult,
            CommitResult = vw_api_types_versions::latest::CommitResult,
            Credentials = vw_api_types_versions::latest::Credentials,
            S3Credentials = vw_api_types_versions::latest::S3Credentials,
            Digest = vw_api_types_versions::latest::Digest,
            FileEntry = vw_api_types_versions::latest::FileEntry,
            SyncPlan = vw_api_types_versions::latest::SyncPlan,
            TargetKind = vw_api_types_versions::latest::TargetKind,
            TreeManifest = vw_api_types_versions::latest::TreeManifest,
        },
    );
}

/// Client for the vw admin API.
pub mod admin {
    progenitor::generate_api!(
        "../openapi/vw-admin-api/vw-admin-api-latest.json"
    );
}

/// Error conditions for constructing a client.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("the access token cannot be sent in an http header: {0}")]
    InvalidToken(#[from] InvalidHeaderValue),
    #[error("building the http client failed: {0}")]
    HttpClient(#[from] reqwest::Error),
}

/// How to reach a vw service.
pub struct ClientConfig<'a> {
    /// Base URL of the service, e.g. `https://vw.example.com:2727`.
    pub base_url: &'a str,

    /// The Github access token to identify the caller with, if there is one.
    ///
    /// Optional because a service run with `--no-auth` answers without one.
    /// Against a service that does require authorization, calls made without a
    /// token come back `401 Unauthorized`.
    pub token: Option<&'a str>,

    /// Accept whatever TLS certificate the service presents, without verifying
    /// it against a trust anchor or checking that it names the host.
    ///
    /// This exists for services fronted by a self-signed certificate, which is
    /// the usual case for a development deployment. It removes the guarantee
    /// that you are talking to the service you think you are, and the access
    /// token is sent to whatever answers, so do not use it against anything
    /// you care about.
    pub insecure: bool,
}

/// A user API client for the service described by `config`.
pub fn user_client(config: &ClientConfig<'_>) -> Result<user::Client, Error> {
    Ok(user::Client::new_with_client(
        config.base_url,
        http_client(config)?,
    ))
}

/// An admin API client for the service described by `config`.
///
/// The token's Github username must have been passed to the service in its
/// `--admin-users` argument for these endpoints to answer.
pub fn admin_client(config: &ClientConfig<'_>) -> Result<admin::Client, Error> {
    Ok(admin::Client::new_with_client(
        config.base_url,
        http_client(config)?,
    ))
}

/// A client for the agent at `base_url`.
///
/// No credentials: the agents sit on the rack's internal network behind
/// `vw-svc`, which has already decided whether the caller owns the environment
/// by the time anything reaches here.
pub fn agent_client(base_url: &str) -> Result<agent::Client, Error> {
    Ok(agent::Client::new_with_client(
        base_url,
        http_client(&ClientConfig {
            base_url,
            token: None,
            insecure: false,
        })?,
    ))
}

/// An http client that presents the configured token, if there is one, on
/// every request.
fn http_client(config: &ClientConfig<'_>) -> Result<reqwest::Client, Error> {
    let mut headers = HeaderMap::new();
    if let Some(token) = config.token {
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {token}"))?;
        // Keep the token out of anything that debug-formats the request
        // headers.
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
    }

    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(config.insecure)
        // A vivado session is a websocket, and a websocket is an HTTP/1.1
        // upgrade — a mechanism HTTP/2 does not have. Over TLS, ALPN otherwise
        // negotiates HTTP/2 and the upgrade is refused before it starts, which
        // is a confusing way to find out. Nothing here benefits from HTTP/2:
        // the requests are small and already sent concurrently over separate
        // connections. The oxide SDK forces the same thing for the same
        // reason.
        .http1_only()
        .build()?)
}
