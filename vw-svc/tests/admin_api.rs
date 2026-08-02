// The admin API, which exists to do the two things the user API deliberately
// cannot: see every environment on the rack, and delete one that belongs to
// somebody else.
//
// That reach is the whole point and also the whole risk, so most of what is
// checked here is who is allowed to use it. As in `user_api.rs`, the service
// runs with authorization disabled and takes the `x-vw-user` header at face
// value — which is exactly what makes it possible to arrive as somebody who is
// not an administrator and confirm the door is shut.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tempfile::TempDir;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// The administrator the service is started with.
const ADMIN: &str = "picard";

/// Somebody who uses the service but does not run it.
const DEVELOPER: &str = "barclay";

/// A running service, reachable on both of its APIs.
struct TestServer {
    child: Child,
    users: String,
    admin: String,
    client: reqwest::Client,
}

impl TestServer {
    async fn start(db_path: &Path) -> TestServer {
        let user_port = free_port();
        let admin_port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_vw-svc"))
            .arg("serve")
            .args(["--address", "127.0.0.1"])
            .args(["--user-api-port", &user_port.to_string()])
            .args(["--admin-api-port", &admin_port.to_string()])
            .args(["--db-path", db_path.to_str().expect("utf8 database path")])
            .args(["--admin-users", ADMIN])
            .arg("--no-auth")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vw-svc");

        let mut server = TestServer {
            child,
            users: format!("http://127.0.0.1:{user_port}"),
            admin: format!("http://127.0.0.1:{admin_port}"),
            client: reqwest::Client::new(),
        };
        server.wait_until_ready().await;
        server
    }

    async fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if self
                .environments(ADMIN)
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK)
            {
                return;
            }
            if let Some(status) =
                self.child.try_wait().expect("check on vw-svc")
            {
                panic!("vw-svc exited during startup: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "vw-svc never started answering on {}",
                self.admin,
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Ask the admin API for every environment.
    async fn environments(
        &self,
        caller: &str,
    ) -> reqwest::Result<reqwest::Response> {
        self.client
            .get(format!("{}/environments", self.admin))
            .header("x-vw-user", caller)
            .send()
            .await
    }

    /// Delete somebody's environment through the admin API.
    async fn delete(
        &self,
        caller: &str,
        user: &str,
        name: &str,
    ) -> reqwest::Response {
        self.client
            .delete(format!("{}/environment/{user}/{name}", self.admin))
            .header("x-vw-user", caller)
            .send()
            .await
            .expect("delete environment")
    }

    /// Create an environment the ordinary way, as `user`.
    async fn create(&self, user: &str, name: &str) {
        let response = self
            .client
            .put(format!("{}/environment/{name}", self.users))
            .header("x-vw-user", user)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("create environment");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    /// What `user` can see of their own environments, through the user API.
    async fn own_environments(&self, user: &str) -> Vec<String> {
        let response = self
            .client
            .get(format!("{}/environments", self.users))
            .header("x-vw-user", user)
            .send()
            .await
            .expect("list own environments");
        assert_eq!(response.status(), StatusCode::OK);

        let page: serde_json::Value =
            response.json().await.expect("decode listing");
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["name"].as_str().expect("name").to_owned())
            .collect()
    }

    /// Every environment the admin API reports, as `user/name`.
    async fn everything(&self) -> Vec<String> {
        let response = self.environments(ADMIN).await.expect("list");
        assert_eq!(response.status(), StatusCode::OK);

        let page: serde_json::Value =
            response.json().await.expect("decode listing");
        let mut found: Vec<String> = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| {
                format!(
                    "{}/{}",
                    item["user"].as_str().expect("user"),
                    item["environment"]["name"].as_str().expect("name"),
                )
            })
            .collect();
        found.sort();
        found
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

async fn server() -> (TempDir, TestServer) {
    let dir = TempDir::new().expect("scratch directory");
    let server = TestServer::start(&dir.path().join("vw-svc.redb")).await;
    (dir, server)
}

#[tokio::test]
async fn an_administrator_sees_every_environment() {
    let (_dir, server) = server().await;
    server.create(DEVELOPER, "darmok").await;
    server.create(DEVELOPER, "jalad").await;
    server.create("laforge", "tanagra").await;

    // The user API shows one developer their own two and nothing else; this
    // is the endpoint that exists because that is not enough for whoever runs
    // the rack.
    assert_eq!(server.own_environments(DEVELOPER).await.len(), 2);

    assert_eq!(
        server.everything().await,
        ["barclay/darmok", "barclay/jalad", "laforge/tanagra"],
    );
}

#[tokio::test]
async fn a_developer_is_not_an_administrator() {
    let (_dir, server) = server().await;
    server.create(DEVELOPER, "darmok").await;

    let refused = server.environments(DEVELOPER).await.expect("list");

    // Forbidden rather than unauthorized: they are who they say they are, and
    // it is not enough.
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_administrator_can_delete_somebody_elses_environment() {
    // The reason this API exists: reclaiming a rack should not require the
    // developer who filled it to still be around.
    let (_dir, server) = server().await;
    server.create(DEVELOPER, "darmok").await;
    server.create(DEVELOPER, "jalad").await;

    let deleted = server.delete(ADMIN, DEVELOPER, "darmok").await;

    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(server.everything().await, ["barclay/jalad"]);
    // And the developer sees the same thing, since there is one record of it.
    assert_eq!(server.own_environments(DEVELOPER).await, ["jalad"]);
}

#[tokio::test]
async fn a_developer_cannot_delete_anything_through_the_admin_api() {
    let (_dir, server) = server().await;
    server.create(DEVELOPER, "darmok").await;
    server.create("laforge", "tanagra").await;

    let refused = server.delete(DEVELOPER, "laforge", "tanagra").await;

    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    // Nothing happened, which is the part that matters.
    assert_eq!(
        server.everything().await,
        ["barclay/darmok", "laforge/tanagra"],
    );
}

#[tokio::test]
async fn deleting_an_environment_that_is_not_there_is_not_found() {
    let (_dir, server) = server().await;
    server.create(DEVELOPER, "darmok").await;

    let missing = server.delete(ADMIN, DEVELOPER, "tanagra").await;

    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(server.everything().await, ["barclay/darmok"]);
}

#[tokio::test]
async fn a_service_with_no_environments_lists_none() {
    let (_dir, server) = server().await;

    assert!(server.everything().await.is_empty());
}
