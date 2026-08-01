//! This module holds common authorization functions. Authorization for vw-svc
//! is centered around Github access tokens.
use dropshot::RequestContext;
use reqwest::{
    header::{ACCEPT, AUTHORIZATION, USER_AGENT},
    StatusCode,
};
use serde::Deserialize;
use slog::{error, info, Logger};
use std::sync::{Arc, OnceLock};

use crate::Context;

/// Access to this repository is what grants access to the service.
const REDHAWK_REPO: &str = "oxidecomputer/redhawk";

/// Github requires a user agent on every API request.
const VW_USER_AGENT: &str = "vw-svc";

/// Header a caller can name themselves with when the service is running with
/// `--no-auth`.
///
/// Deliberately not the authorization header: a real client sends a real
/// Github token there, and under `--no-auth` the caller's name is written to
/// the database as an environment key. Reading the name from its own header
/// keeps tokens out of persistent state on development servers.
const NO_AUTH_USER_HEADER: &str = "x-vw-user";

/// The caller identity used when the service is running with `--no-auth` and
/// the caller did not name themselves.
const ANONYMOUS_USER: &str = "anonymous";

/// Shared client so token checks reuse connections to the Github API.
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub(crate) struct AuthorizedCaller {
    pub(crate) name: String,
    pub(crate) is_admin: bool,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum AuthError {
    #[error("no token is present")]
    NoAuthToken,
    #[error("github rejected the supplied token")]
    TokenRejected,
    #[error("the supplied token does not have access to redhawk")]
    NoRedhawkProjectAccess,
    #[error("an error occured talking to github: {0}")]
    GithubError(String),
}

/// The subset of the Github `/user` response we care about.
#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

pub(crate) async fn authorize_caller(
    rqctx: RequestContext<Arc<Context>>,
) -> Result<AuthorizedCaller, AuthError> {
    let args = &rqctx.context().server_args;

    let name = if args.no_auth {
        // Authorization is off, so there is nothing to ask Github about. Take
        // the caller at their word about who they are so per-user endpoints
        // are still exercisable, falling back to a fixed identity when the
        // caller says nothing at all.
        header(&rqctx, NO_AUTH_USER_HEADER)
            .unwrap_or_else(|| ANONYMOUS_USER.to_owned())
    } else {
        let token = bearer_token(&rqctx).ok_or(AuthError::NoAuthToken)?;
        let client = client();
        let name = github_username(client, &token, &rqctx.log).await?;
        check_redhawk_access(&name, client, &token, &rqctx.log).await?;
        name
    };

    // Github usernames are case insensitive, so the admin list is too.
    let is_admin = args
        .admin_users
        .iter()
        .any(|admin| admin.eq_ignore_ascii_case(&name));

    info!(rqctx.log, "authorized caller";
        "username" => &name,
        "is_admin" => is_admin,
        "req_id" => rqctx.request_id,
    );

    Ok(AuthorizedCaller { name, is_admin })
}

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(reqwest::Client::new)
}

/// The value of `name`, if the request carries it as a non-empty header.
fn header(rqctx: &RequestContext<Arc<Context>>, name: &str) -> Option<String> {
    let value = rqctx.request.headers().get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Pull the Github token out of the request's authorization header.
///
/// Both the `Bearer <token>` and `token <token>` forms Github accepts are
/// understood, as is a bare token with no scheme.
fn bearer_token(rqctx: &RequestContext<Arc<Context>>) -> Option<String> {
    let value = header(rqctx, "authorization")?;
    let token = match value.split_once(' ') {
        Some((scheme, token))
            if scheme.eq_ignore_ascii_case("bearer")
                || scheme.eq_ignore_ascii_case("token") =>
        {
            token
        }
        _ => &value,
    }
    .trim();

    (!token.is_empty()).then(|| token.to_owned())
}

/// Verify the token can see the redhawk repository.
async fn check_redhawk_access(
    username: &str,
    client: &reqwest::Client,
    token: &str,
    log: &Logger,
) -> Result<(), AuthError> {
    let url = format!("https://api.github.com/repos/{REDHAWK_REPO}");
    let response = github_get(client, &url, token).await?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    match status {
        StatusCode::UNAUTHORIZED => {
            info!(log, "github token rejected"; "username" => &username);
            Err(AuthError::TokenRejected)
        }
        // Github reports repositories a token cannot see as absent rather than
        // forbidden, so a 404 here means the same thing as a 403.
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            info!(log, "caller is not part of redhawk project";
                "username" => &username
            );
            Err(AuthError::NoRedhawkProjectAccess)
        }
        other => {
            let e = AuthError::GithubError(format!(
                "unexpected response {other} from {url}"
            ));
            error!(log, "github error checking redhawk access: {e}");
            Err(e)
        }
    }
}

/// Look up the Github username the token belongs to.
async fn github_username(
    client: &reqwest::Client,
    token: &str,
    log: &Logger,
) -> Result<String, AuthError> {
    let url = "https://api.github.com/user";
    let response = github_get(client, url, token).await?;

    let status = response.status();
    if !status.is_success() {
        return Err(match status {
            StatusCode::UNAUTHORIZED => {
                info!(log, "github token rejected");
                AuthError::TokenRejected
            }
            other => {
                let e = AuthError::GithubError(format!(
                    "unexpected response {other} from {url}"
                ));
                error!(log, "github error getting username: {e}");
                e
            }
        });
    }

    let user: GithubUser = response.json().await.map_err(|e| {
        AuthError::GithubError(format!("decoding response from {url}: {e}"))
    })?;

    // Github logins are case insensitive but reported in their original case.
    // Downcasing here keeps one person from owning two sets of environments,
    // and is required anyway to build an Oxide instance name out of it.
    Ok(user.login.to_lowercase())
}

async fn github_get(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<reqwest::Response, AuthError> {
    client
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(USER_AGENT, VW_USER_AGENT)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AuthError::GithubError(format!("GET {url}: {e}")))
}
