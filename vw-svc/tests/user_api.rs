// Integration tests for the vw-svc user API.
//
// Each test spawns the service as a child process against a scratch database
// with `--no-auth`, so these run anywhere without Github credentials. With
// authorization disabled the service takes the `x-vw-user` header at face
// value as the caller's identity, which is what lets these tests exercise the
// per-user behavior of the API without talking to Github.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dropshot::ResultsPage;
use reqwest::StatusCode;
use tempfile::TempDir;
use vw_api_types_versions::latest::{Environment, SshKeyPair};

/// How long to wait for a freshly spawned service to accept requests.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// A running vw-svc backed by a scratch database.
///
/// Dropping the server kills the child process.
struct TestServer {
    child: Child,
    base_url: String,
    client: reqwest::Client,
}

impl TestServer {
    /// Spawn the service against the database at `db_path`, creating it if it
    /// does not exist, and wait for the user API to start answering.
    async fn start(db_path: &Path) -> TestServer {
        let user_port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_vw-svc"))
            .arg("serve")
            .args(["--address", "127.0.0.1"])
            .args(["--user-api-port", &user_port.to_string()])
            .args(["--admin-api-port", &free_port().to_string()])
            .args(["--db-path", db_path.to_str().expect("utf8 database path")])
            .arg("--no-auth")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vw-svc");

        let mut server = TestServer {
            child,
            base_url: format!("http://127.0.0.1:{user_port}"),
            client: versioned_client(),
        };
        server.wait_until_ready().await;
        server
    }

    async fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if self
                .client
                .get(format!("{}/environments", self.base_url))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            if let Some(status) =
                self.child.try_wait().expect("check on vw-svc process")
            {
                panic!("vw-svc exited during startup: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "vw-svc never started listening on {}",
                self.base_url,
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Send a request as `user`. Authorization is disabled, so the service
    /// reads the caller's identity straight out of this header.
    fn request(
        &self,
        method: reqwest::Method,
        user: &str,
        path: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header("x-vw-user", user)
    }

    async fn list(&self, user: &str) -> reqwest::Response {
        self.request(reqwest::Method::GET, user, "/environments")
            .send()
            .await
            .expect("list environments")
    }

    async fn create(&self, user: &str, name: &str) -> reqwest::Response {
        // No image overrides: the service has no Oxide backend in these
        // tests, so it records the environment without resolving any.
        self.request(
            reqwest::Method::PUT,
            user,
            &format!("/environment/{name}"),
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("create environment")
    }

    async fn get(&self, user: &str, name: &str) -> reqwest::Response {
        self.request(
            reqwest::Method::GET,
            user,
            &format!("/environment/{name}"),
        )
        .send()
        .await
        .expect("get environment")
    }

    async fn keys(&self, user: &str, name: &str) -> reqwest::Response {
        self.request(
            reqwest::Method::GET,
            user,
            &format!("/environment/{name}/keys"),
        )
        .send()
        .await
        .expect("get environment keys")
    }

    async fn delete(&self, user: &str, name: &str) -> reqwest::Response {
        self.request(
            reqwest::Method::DELETE,
            user,
            &format!("/environment/{name}"),
        )
        .send()
        .await
        .expect("delete environment")
    }

    /// The names of `user`'s environments, in the order the API returned them.
    ///
    /// The endpoint takes no pagination parameters, so a complete listing is
    /// always one page with no next page token.
    async fn environment_names(&self, user: &str) -> Vec<String> {
        let response = self.list(user).await;
        assert_eq!(response.status(), StatusCode::OK);
        let page: ResultsPage<Environment> =
            response.json().await.expect("decode environments page");
        assert_eq!(
            page.next_page, None,
            "an unpaginated listing should not offer a next page",
        );
        page.items.into_iter().map(|env| env.name).collect()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port nothing is listening on, obtained by binding an ephemeral port and
/// immediately releasing it.
/// A client that names the API version, as every real client does.
///
/// The user API has endpoints that only exist from a given version onwards, so
/// the service routes on this header and refuses a request without it.
fn versioned_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static(vw_api::API_VERSION_HEADER),
        vw_api::latest_version()
            .to_string()
            .parse()
            .expect("a version is a header value"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("build a client")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

#[tokio::test]
async fn environment_lifecycle() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert!(server.environment_names("ferris").await.is_empty());

    let response = server.create("ferris", "alpha").await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = server.get("ferris", "alpha").await;
    assert_eq!(response.status(), StatusCode::OK);
    let env: Environment = response.json().await.expect("decode environment");
    assert_eq!(env.name, "alpha");
    // A new environment has no instances behind it yet.
    assert!(env.vivado_instance.is_none());
    assert!(env.helios_instance.is_none());
    assert!(env.artifact_instance.is_none());

    assert_eq!(server.environment_names("ferris").await, ["alpha"]);

    let response = server.delete("ferris", "alpha").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    assert!(server.environment_names("ferris").await.is_empty());
}

#[tokio::test]
async fn creating_the_same_environment_twice_conflicts() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CONFLICT
    );

    // The conflicting create left the original entry alone.
    assert_eq!(server.environment_names("ferris").await, ["alpha"]);
}

#[tokio::test]
async fn missing_environments_are_not_found() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.get("ferris", "nonesuch").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server.delete("ferris", "nonesuch").await.status(),
        StatusCode::NOT_FOUND
    );

    // Deleting an environment consumes it, so a second delete is a 404 too.
    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        server.delete("ferris", "alpha").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.delete("ferris", "alpha").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn environments_are_scoped_to_their_owner() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    for (user, name) in
        [("ferris", "alpha"), ("ferris", "beta"), ("gorris", "alpha")]
    {
        assert_eq!(
            server.create(user, name).await.status(),
            StatusCode::CREATED
        );
    }

    assert_eq!(server.environment_names("ferris").await, ["alpha", "beta"]);
    assert_eq!(server.environment_names("gorris").await, ["alpha"]);

    // Same environment name, different owners: deleting one leaves the other.
    assert_eq!(
        server.delete("ferris", "alpha").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.environment_names("ferris").await, ["beta"]);
    assert_eq!(server.environment_names("gorris").await, ["alpha"]);

    // One user's name being a prefix of another's must not blur the two
    // listings together, which is the failure mode of the prefix scan that
    // backs this endpoint.
    assert_eq!(
        server.create("f", "solo").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(server.environment_names("f").await, ["solo"]);
    assert_eq!(server.environment_names("ferris").await, ["beta"]);
}

#[tokio::test]
async fn environments_outlive_the_service() {
    let dir = TempDir::new().expect("scratch directory");
    let db_path = dir.path().join("vw-svc.redb");

    {
        let server = TestServer::start(&db_path).await;
        assert_eq!(
            server.create("ferris", "alpha").await.status(),
            StatusCode::CREATED
        );
    }

    let server = TestServer::start(&db_path).await;
    assert_eq!(server.environment_names("ferris").await, ["alpha"]);
}

#[tokio::test]
async fn a_full_listing_is_a_single_page() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    let expected: Vec<String> = (0..25).map(|i| format!("env{i:02}")).collect();
    for name in &expected {
        assert_eq!(
            server.create("ferris", name).await.status(),
            StatusCode::CREATED
        );
    }

    // `environment_names` asserts the page carries no next page token.
    assert_eq!(server.environment_names("ferris").await, expected);
}

#[tokio::test]
async fn names_that_break_the_instance_naming_scheme_are_rejected() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    // Oxide instances are named "vwsvc-{user}-{env}-{kind}" and split back
    // apart on `-`, so an environment name carrying one would not survive the
    // round trip. The rest are what the control plane accepts for a name.
    // An empty name is not in this list because the router rejects
    // `PUT /environment/` as a 404 before a handler ever sees it.
    for rejected in ["my-env", "MyEnv", "my_env", "9lives"] {
        assert_eq!(
            server.create("ferris", rejected).await.status(),
            StatusCode::BAD_REQUEST,
            "expected '{rejected}' to be rejected",
        );
    }

    for accepted in ["alpha", "env2", "a"] {
        assert_eq!(
            server.create("ferris", accepted).await.status(),
            StatusCode::CREATED,
            "expected '{accepted}' to be accepted",
        );
    }
}

#[tokio::test]
async fn environments_have_no_images_without_an_oxide_backend() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );

    // With no rack to resolve against there is nothing to pin, and the
    // environment is a bare record the reconciler will never provision.
    let response = server.get("ferris", "alpha").await;
    let env: Environment = response.json().await.expect("decode environment");
    assert!(env.images.is_none());
}

#[tokio::test]
async fn naming_an_image_without_an_oxide_backend_is_rejected() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    // Accepting an image the service can neither validate nor ever use would
    // look like it took effect.
    let response = server
        .request(reqwest::Method::PUT, "ferris", "/environment/alpha")
        .json(&serde_json::json!({ "vivado_image": "vw-vivado-20260101" }))
        .send()
        .await
        .expect("create environment");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(server.environment_names("ferris").await.is_empty());
}

#[tokio::test]
async fn an_environment_comes_with_a_key_that_opens_it() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    // The create hands the key back directly, so a client can save it without
    // a second round trip.
    let created = server.create("ferris", "alpha").await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let from_create: SshKeyPair =
        created.json().await.expect("decode created keys");

    let response = server.keys("ferris", "alpha").await;
    assert_eq!(response.status(), StatusCode::OK);
    let keys: SshKeyPair = response.json().await.expect("decode keys");

    // And it is the same pair, not a second one generated on the way out.
    assert_eq!(from_create.private_key, keys.private_key);
    assert_eq!(from_create.public_key, keys.public_key);

    // The shapes ssh itself insists on: OpenSSH private key encoding, and a
    // public key line an authorized_keys file would accept.
    assert!(keys
        .private_key
        .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    assert!(keys.public_key.starts_with("ssh-ed25519 "));
    // Named after the environment it opens, so it is recognizable in an agent.
    assert!(keys.public_key.trim_end().ends_with("vw ferris/alpha"));
}

#[tokio::test]
async fn a_private_key_never_rides_along_with_an_environment() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );

    // The key lives in its own table precisely so that fetching or listing an
    // environment cannot carry it out.
    for body in [
        server.get("ferris", "alpha").await.text().await.unwrap(),
        server.list("ferris").await.text().await.unwrap(),
    ] {
        assert!(
            !body.contains("PRIVATE KEY"),
            "an environment response leaked a private key: {body}",
        );
    }
}

#[tokio::test]
async fn one_users_key_is_not_another_users_to_fetch() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );

    // Same environment name, different caller: keys are scoped like every
    // other endpoint here.
    assert_eq!(
        server.keys("gorris", "alpha").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_an_environment_takes_its_key_with_it() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        server.keys("ferris", "alpha").await.status(),
        StatusCode::OK
    );

    assert_eq!(
        server.delete("ferris", "alpha").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.keys("ferris", "alpha").await.status(),
        StatusCode::NOT_FOUND,
        "a key that opens nothing should not outlive its environment",
    );

    // A fresh environment of the same name gets a fresh key rather than
    // inheriting the old one.
    assert_eq!(
        server.create("ferris", "alpha").await.status(),
        StatusCode::CREATED
    );
    let keys: SshKeyPair = server
        .keys("ferris", "alpha")
        .await
        .json()
        .await
        .expect("decode keys");
    assert!(keys.public_key.starts_with("ssh-ed25519 "));
}

/// The flush endpoint exists only from version 3 onwards, so a client that
/// does not say which version it means is turned away rather than answered
/// from one it may not have been built for.
#[tokio::test]
async fn a_request_that_names_no_api_version_is_refused() {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;

    let unversioned = reqwest::Client::new()
        .post(format!("{}/environments", server.base_url))
        .send()
        .await
        .expect("send unversioned request");

    assert_eq!(unversioned.status(), StatusCode::BAD_REQUEST);
}
