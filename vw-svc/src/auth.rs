//! This module holds common authorization functions. Authorization for vw-svc
//! is centered around Github access tokens.
use dropshot::RequestContext;
use reqwest::{
    header::{ACCEPT, AUTHORIZATION, USER_AGENT},
    StatusCode,
};
use serde::Deserialize;
use slog::{error, info, Logger};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

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

/// How long a Github answer about a token is trusted for.
///
/// Deciding whether a token is good costs two round trips to Github, and
/// without this every request pays them. That is barely noticeable for a
/// person clicking around and ruinous for a source sync, which sends one
/// request per file: a first sync of a few hundred files spent two minutes
/// waiting on Github and burned five hundred API calls doing it.
///
/// A minute is short enough that revoking someone's access takes effect while
/// they are still reading the email about it, and long enough that a whole
/// sync costs one check.
const AUTH_CACHE_TTL: Duration = Duration::from_secs(60);

/// What Github said about a token, and when it stops being worth believing.
struct CachedAuth {
    name: String,
    expires_at: Instant,
}

/// Answers about tokens, keyed by a digest of the token rather than the token.
///
/// Hashed because the raw value is a live credential and a map key is a poor
/// place to leave one lying: this way a dump of the process, or a stray
/// `Debug`, does not hand one over.
static AUTH_CACHE: OnceLock<Mutex<HashMap<[u8; 32], CachedAuth>>> =
    OnceLock::new();

fn auth_cache() -> &'static Mutex<HashMap<[u8; 32], CachedAuth>> {
    AUTH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

/// The name Github last gave for this token, if that was recently enough.
fn cached_name(token: &str) -> Option<String> {
    let mut cache = auth_cache().lock().expect("the auth cache lock");
    let key = cache_key(token);

    match cache.get(&key) {
        Some(entry) if entry.expires_at > Instant::now() => {
            Some(entry.name.clone())
        }
        Some(_) => {
            cache.remove(&key);
            None
        }
        None => None,
    }
}

fn remember(token: &str, name: &str) {
    let mut cache = auth_cache().lock().expect("the auth cache lock");

    // Expired entries are only noticed when their own token comes back, so a
    // token used once and never again would otherwise sit here forever. This
    // is the only place the map grows, so it is the right place to sweep.
    let now = Instant::now();
    cache.retain(|_, entry| entry.expires_at > now);

    cache.insert(
        cache_key(token),
        CachedAuth {
            name: name.to_owned(),
            expires_at: now + AUTH_CACHE_TTL,
        },
    );
}

pub(crate) struct AuthorizedCaller {
    pub(crate) name: String,
    pub(crate) is_admin: bool,
    /// The token the caller authorized themselves with.
    ///
    /// Kept because an instance needs credentials of its own to fetch a
    /// build's dependencies, and this is already the caller's answer to
    /// "prove you may reach these repositories". Nothing stores it — it is
    /// relayed to the instance and dropped when the request ends.
    ///
    /// Absent when the service runs with `--no-auth`, where there was no
    /// token to begin with.
    pub(crate) token: Option<String>,
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

    let supplied = bearer_token(&rqctx);

    let name = if args.no_auth {
        // Authorization is off, so there is nothing to ask Github about. Take
        // the caller at their word about who they are so per-user endpoints
        // are still exercisable, falling back to a fixed identity when the
        // caller says nothing at all.
        header(&rqctx, NO_AUTH_USER_HEADER)
            .unwrap_or_else(|| ANONYMOUS_USER.to_owned())
    } else {
        let token = supplied.clone().ok_or(AuthError::NoAuthToken)?;

        match cached_name(&token) {
            Some(name) => name,
            None => {
                let client = client();
                let name = github_username(client, &token, &rqctx.log).await?;
                check_redhawk_access(&name, client, &token, &rqctx.log).await?;
                remember(&token, &name);
                name
            }
        }
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

    Ok(AuthorizedCaller {
        name,
        is_admin,
        token: supplied,
    })
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_token_is_only_checked_with_github_once_per_ttl() {
        let token = "ghp_a_token_used_for_a_whole_sync";
        assert!(cached_name(token).is_none(), "nothing is known yet");

        remember(token, "rcgoodfellow");
        assert_eq!(cached_name(token).as_deref(), Some("rcgoodfellow"));
        // Repeated lookups keep hitting: this is the whole point, since a
        // sync asks once per file.
        assert_eq!(cached_name(token).as_deref(), Some("rcgoodfellow"));
    }

    #[test]
    fn one_tokens_answer_is_not_anothers() {
        remember("ghp_ferris", "ferris");
        remember("ghp_gorris", "gorris");

        assert_eq!(cached_name("ghp_ferris").as_deref(), Some("ferris"));
        assert_eq!(cached_name("ghp_gorris").as_deref(), Some("gorris"));
        assert!(cached_name("ghp_never_seen").is_none());
    }

    #[test]
    fn an_answer_stops_being_believed_once_it_is_old() {
        let token = "ghp_a_token_since_revoked";

        // Reach past `remember` to place an entry that has already expired,
        // which is the state a revoked token's entry reaches on its own after
        // a minute.
        auth_cache().lock().expect("lock").insert(
            cache_key(token),
            CachedAuth {
                name: "rcgoodfellow".to_owned(),
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );

        assert!(
            cached_name(token).is_none(),
            "a stale answer must send the next request back to github",
        );
    }

    #[test]
    fn the_cache_is_not_keyed_by_the_credential_itself() {
        // The key reaches a map that outlives the request. Keying it by the
        // raw token would leave live credentials sitting in process memory in
        // a form anything walking the map could read straight off.
        let token = "ghp_a_real_looking_token";
        remember(token, "ferris");

        let cache = auth_cache().lock().expect("lock");
        assert!(
            !cache.keys().any(|key| key.as_slice() == token.as_bytes()),
            "the token itself should not be a key",
        );
        assert!(cache.contains_key(&cache_key(token)));
    }
}
