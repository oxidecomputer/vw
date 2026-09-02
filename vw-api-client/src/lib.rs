//! Progenitor generated clients for the vw service APIs.
//!
//! The user and admin APIs are served by two separate dropshot servers on two
//! separate ports, so each gets its own client in its own module.
//!
//! The OpenAPI documents these are generated from live in `openapi/` at the
//! root of the repository and belong to `vw-openapi-manager`: run
//! `cargo xtask openapi generate` after changing an endpoint. Each is read through
//! the `-latest.json` symlink the manager maintains, so this crate does not
//! have to be edited when the API version changes. `cargo xtask openapi check`
//! (also run as a test) fails if a document is out of date, which keeps these
//! clients from drifting from the service.
//!
//! Both APIs identify the caller by a Github access token in the authorization
//! header of every request. Rather than make each call site remember that,
//! [`user_client`] and [`admin_client`] build clients with the header already
//! attached. The token is optional: a service run with `--no-auth` answers
//! without one.

use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, InvalidHeaderValue, AUTHORIZATION,
};
use std::time::Duration;

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

/// How many times a call is attempted before its failure is the answer.
///
/// The service is across a network from almost everybody who uses it, and a
/// single dropped connection should not end a build that has been running for
/// half an hour.
pub const ATTEMPTS: usize = 10;

/// How long to wait between attempts.
///
/// Flat rather than backing off. What this is riding out is a connection that
/// failed to establish or a service that is restarting, both of which resolve
/// in seconds — and ten evenly spaced tries covers that window while still
/// giving up inside a time somebody will wait.
pub const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Whether trying again could plausibly produce a different answer.
///
/// Anything that never reached a server, or reached one that was in no
/// position to answer, is worth repeating. A request the service understood
/// and refused is not: `401` will still be `401` ten seconds from now, and
/// retrying it only delays telling somebody their token is wrong by ten
/// seconds.
pub fn retryable<E>(error: &progenitor_client::Error<E>) -> bool {
    match error.status() {
        // Never got an answer: connection refused, DNS, TLS, timeout.
        None => true,
        // The service is there and having a bad time. `429` is included
        // because being told to slow down is a reason to wait, not to stop.
        Some(status) => {
            status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        }
    }
}

/// Call `attempt` until it succeeds, its failure turns out not to be worth
/// repeating, or [`ATTEMPTS`] tries have been made.
///
/// Wraps the request only. Callers that stream a response body do so after
/// this returns, which is deliberate: restarting a request whose body is
/// already partly written to a file would corrupt what it had, and the retry
/// belongs where it is still safe.
pub async fn retrying<T, E, F, Fut>(
    mut attempt: F,
) -> Result<T, progenitor_client::Error<E>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, progenitor_client::Error<E>>>,
{
    // One short of the total, so the last try below is the tenth and its
    // failure is returned rather than slept on.
    for _ in 1..ATTEMPTS {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error) => {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    attempt().await
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
        http_client(config, vw_api::latest_version())?,
    ))
}

/// An admin API client for the service described by `config`.
///
/// The token's Github username must have been passed to the service in its
/// `--admin-users` argument for these endpoints to answer.
pub fn admin_client(config: &ClientConfig<'_>) -> Result<admin::Client, Error> {
    Ok(admin::Client::new_with_client(
        config.base_url,
        http_client(config, vw_api::latest_version())?,
    ))
}

/// A client for the agent at `base_url`.
///
/// No credentials: the agents sit on the rack's internal network behind
/// `vw-svc`, which has already decided whether the caller owns the environment
/// by the time anything reaches here.
///
/// Versioned against the agent API rather than the user API. They are separate
/// documents with separate version histories that happen to share a header
/// name, and sending one's version to the other is only harmless for as long
/// as the numbers coincide — at which point it stops being harmless without
/// anything having changed.
pub fn agent_client(base_url: &str) -> Result<agent::Client, Error> {
    Ok(agent::Client::new_with_client(
        base_url,
        http_client(
            &ClientConfig {
                base_url,
                token: None,
                insecure: false,
            },
            vw_sync_api::latest_version(),
        )?,
    ))
}

/// An http client that presents the configured token, if there is one, on
/// every request, and names `version` as the API version it speaks.
fn http_client(
    config: &ClientConfig<'_>,
    version: impl std::fmt::Display,
) -> Result<reqwest::Client, Error> {
    let mut headers = HeaderMap::new();

    // What this client was generated against. The service routes on it, so a
    // request without it is refused rather than answered from a version this
    // client may not understand.
    headers.insert(
        // Both APIs spell it the same way; only the value differs.
        HeaderName::from_static(vw_api::API_VERSION_HEADER),
        HeaderValue::from_str(&version.to_string())?,
    );

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

#[cfg(test)]
mod test {
    use super::*;
    use std::cell::Cell;

    /// An error that never reached a server, so it has no status.
    fn transport() -> progenitor_client::Error<()> {
        progenitor_client::Error::InvalidRequest("no connection".to_owned())
    }

    /// An error carrying `status`, built the only way a status-bearing one can
    /// be made outside the generated code.
    fn answered(status: u16) -> progenitor_client::Error<()> {
        let response = http::Response::builder()
            .status(status)
            .body("")
            .expect("a response");
        progenitor_client::Error::UnexpectedResponse(response.into())
    }

    #[test]
    fn what_the_service_refused_is_not_tried_again() {
        // The whole point of not blindly retrying: a token that is wrong stays
        // wrong, and ten seconds of hoping helps nobody.
        for status in [400, 401, 403, 404, 409] {
            assert!(
                !retryable(&answered(status)),
                "{status} should be reported, not retried",
            );
        }
    }

    #[test]
    fn what_never_got_an_answer_is_tried_again() {
        assert!(retryable(&transport()));
        for status in [500, 502, 503, 504] {
            assert!(retryable(&answered(status)), "{status} is worth a retry");
        }
        // Being told to slow down is a reason to wait, not to stop.
        assert!(retryable(&answered(429)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_comes_good_stops_being_retried() {
        let calls = Cell::new(0);

        let got: Result<&str, _> = retrying(|| async {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(transport())
            } else {
                Ok("answered")
            }
        })
        .await;

        assert_eq!(got.expect("should succeed"), "answered");
        assert_eq!(calls.get(), 3, "should stop as soon as it works");
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_never_comes_good_is_tried_exactly_ten_times() {
        let calls = Cell::new(0);

        let got: Result<(), _> = retrying(|| async {
            calls.set(calls.get() + 1);
            Err(transport())
        })
        .await;

        assert!(got.is_err());
        assert_eq!(calls.get(), ATTEMPTS, "should try ATTEMPTS times, no more");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_ends_it_on_the_first_try() {
        let calls = Cell::new(0);

        let got: Result<(), _> = retrying(|| async {
            calls.set(calls.get() + 1);
            Err(answered(401))
        })
        .await;

        assert!(got.is_err());
        assert_eq!(calls.get(), 1, "no point asking again");
    }
}
