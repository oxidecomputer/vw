// The agent driven the way `vw-svc` will drive it: over HTTP, against a real
// filesystem, with no shortcuts through the engine underneath.
//
// The engine's own behaviour is covered in `vw-sync`. What is worth checking
// here is everything the HTTP layer adds — that a manifest survives the round
// trip, that content is verified on arrival, that a request for the wrong
// environment is refused, and that a caller who lies about a path or a digest
// gets a refusal rather than a file somewhere it should not be.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use reqwest::StatusCode;
use tempfile::TempDir;
use vw_api_types_versions::latest::{
    CommitResult, Credentials, Digest, FileEntry, SyncPlan, TreeManifest,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times to try for a port before calling it a real failure.
const STARTUP_ATTEMPTS: usize = 5;
const ENVIRONMENT: &str = "darmok";

/// Start an agent on `port`, with everything it needs under `base`.
fn spawn_agent(
    base: &Utf8Path,
    root: &Utf8Path,
    netrc: &Utf8Path,
    port: u16,
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_vw-agent"))
        .arg("serve")
        .args(["--address", "127.0.0.1"])
        .args(["--port", &port.to_string()])
        .args(["--environment", ENVIRONMENT])
        .args(["--root", root.as_str()])
        .args(["--store", base.join("store").as_str()])
        .args(["--netrc", netrc.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vw-agent")
}

/// Whether an agent came up and answered.
///
/// `false` when it exited instead, which in practice means it lost the race
/// for its port. Any answer at all counts as up — this one is a `404` until
/// somebody says where artifacts go, which is the state an agent starts in.
async fn listening(
    client: &reqwest::Client,
    base_url: &str,
    child: &mut Child,
) -> bool {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if client
            .get(format!(
                "{base_url}/environment/{ENVIRONMENT}/artifact-target"
            ))
            .send()
            .await
            .is_ok()
        {
            return true;
        }
        if child.try_wait().expect("check on vw-agent").is_some() {
            return false;
        }
        assert!(
            Instant::now() < deadline,
            "vw-agent never started listening on {base_url}",
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A client that says which version of the agent API it speaks.
///
/// The agent serves endpoints that exist only from a given version onwards, so
/// it refuses a request that does not say — exactly as it would refuse one
/// from a `vw-svc` too old to know about them, which is the point of asking.
fn versioned_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static(
            vw_sync_api::API_VERSION_HEADER,
        ),
        reqwest::header::HeaderValue::from_str(
            &vw_sync_api::latest_version().to_string(),
        )
        .expect("a version is a valid header value"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("build a client")
}

/// A running agent with a tree and a content store of its own.
struct Agent {
    child: Child,
    base_url: String,
    client: reqwest::Client,
    _dir: TempDir,
    root: Utf8PathBuf,
    netrc: Utf8PathBuf,
}

impl Agent {
    async fn start() -> Agent {
        let dir = TempDir::new().expect("scratch directory");
        let base = Utf8Path::from_path(dir.path())
            .expect("utf8 temp dir")
            .to_owned();
        let root = base.join("tree");
        let netrc = base.join("home/.netrc");
        let client = versioned_client();

        // A port is chosen by binding one and letting it go, so two agents
        // starting at the same moment can be handed the same one and whichever
        // gets there second fails to bind. Nothing is wrong when that happens
        // and there is nothing to do about it but pick another.
        for _ in 0..STARTUP_ATTEMPTS {
            let port = free_port();
            let mut child = spawn_agent(&base, &root, &netrc, port);
            let base_url = format!("http://127.0.0.1:{port}");

            if listening(&client, &base_url, &mut child).await {
                return Agent {
                    child,
                    base_url,
                    client,
                    _dir: dir,
                    root,
                    netrc,
                };
            }

            let _ = child.kill();
            let _ = child.wait();
        }

        panic!("vw-agent exited during startup {STARTUP_ATTEMPTS} times");
    }

    async fn plan_raw(
        &self,
        environment: &str,
        manifest: &TreeManifest,
    ) -> reqwest::Result<reqwest::Response> {
        self.client
            .post(format!(
                "{}/environment/{environment}/sync/plan",
                self.base_url
            ))
            .json(manifest)
            .send()
            .await
    }

    async fn plan(&self, manifest: &TreeManifest) -> SyncPlan {
        let response = self
            .plan_raw(ENVIRONMENT, manifest)
            .await
            .expect("plan request");
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("decode plan")
    }

    async fn put_blob(
        &self,
        digest: &Digest,
        contents: &[u8],
    ) -> reqwest::Response {
        self.client
            .put(format!(
                "{}/environment/{ENVIRONMENT}/sync/blob/{digest}",
                self.base_url
            ))
            .body(contents.to_vec())
            .send()
            .await
            .expect("blob request")
    }

    async fn commit_raw(&self, manifest: &TreeManifest) -> reqwest::Response {
        self.client
            .post(format!(
                "{}/environment/{ENVIRONMENT}/sync/commit",
                self.base_url
            ))
            .json(manifest)
            .send()
            .await
            .expect("commit request")
    }

    async fn clear_raw(&self, environment: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/environment/{environment}/sync", self.base_url))
            .send()
            .await
            .expect("clear request")
    }

    async fn clear(&self) -> CommitResult {
        let response = self.clear_raw(ENVIRONMENT).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("decode clear result")
    }

    async fn put_credentials(
        &self,
        environment: &str,
        credentials: &Credentials,
    ) -> reqwest::Response {
        self.client
            .put(format!(
                "{}/environment/{environment}/credentials",
                self.base_url
            ))
            .json(credentials)
            .send()
            .await
            .expect("credentials request")
    }

    async fn clean_raw(&self, environment: &str) -> reqwest::Response {
        self.client
            .delete(format!(
                "{}/environment/{environment}/build-output",
                self.base_url
            ))
            .send()
            .await
            .expect("clean request")
    }

    async fn commit(&self, manifest: &TreeManifest) -> CommitResult {
        let response = self.commit_raw(manifest).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("decode commit result")
    }

    /// A full synchronization of `files`, returning what the commit did and
    /// how many blobs actually crossed the wire.
    async fn sync(&self, files: &[(&str, &str)]) -> (CommitResult, usize) {
        let manifest = manifest_of(files);
        let plan = self.plan(&manifest).await;

        for digest in &plan.missing {
            let contents = files
                .iter()
                .find(|(_, body)| {
                    vw_sync::digest_bytes(body.as_bytes()) == *digest
                })
                .expect("the plan asked for something in the manifest")
                .1;
            let response = self.put_blob(digest, contents.as_bytes()).await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        (self.commit(&manifest).await, plan.missing.len())
    }

    fn contents(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path))
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = vw_sync::scan(&self.root)
            .expect("scan tree")
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        paths.sort();
        paths
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn manifest_of(files: &[(&str, &str)]) -> TreeManifest {
    TreeManifest {
        entries: files
            .iter()
            .map(|(path, contents)| FileEntry {
                path: (*path).to_owned(),
                digest: vw_sync::digest_bytes(contents.as_bytes()),
                executable: false,
            })
            .collect(),
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

#[tokio::test]
async fn a_tree_arrives_over_http() {
    let agent = Agent::start().await;

    let (result, uploaded) = agent
        .sync(&[
            ("hdl/top.vhd", "entity top is end;"),
            ("vw.toml", "[workspace]"),
        ])
        .await;

    assert_eq!(uploaded, 2);
    assert_eq!(result.created, 2);
    assert_eq!(agent.paths(), ["hdl/top.vhd", "vw.toml"]);
    assert_eq!(agent.contents("hdl/top.vhd"), "entity top is end;");
}

#[tokio::test]
async fn a_second_sync_of_the_same_tree_sends_nothing() {
    let agent = Agent::start().await;
    let files = [("hdl/top.vhd", "entity top is end;")];

    agent.sync(&files).await;
    let (result, uploaded) = agent.sync(&files).await;

    assert_eq!(uploaded, 0);
    assert_eq!(result.unchanged, 1);
}

#[tokio::test]
async fn an_edit_sends_only_the_edited_file() {
    let agent = Agent::start().await;
    agent
        .sync(&[
            ("hdl/top.vhd", "entity top is end;"),
            ("hdl/other.vhd", "entity other is end;"),
        ])
        .await;

    let (result, uploaded) = agent
        .sync(&[
            ("hdl/top.vhd", "entity top is end; -- edited"),
            ("hdl/other.vhd", "entity other is end;"),
        ])
        .await;

    assert_eq!(uploaded, 1);
    assert_eq!(result.updated, 1);
    assert_eq!(result.unchanged, 1);
}

#[tokio::test]
async fn a_rename_crosses_no_wire() {
    let agent = Agent::start().await;
    let body = "x".repeat(50_000);
    agent.sync(&[("hdl/big.vhd", body.as_str())]).await;

    let (result, uploaded) =
        agent.sync(&[("hdl/renamed.vhd", body.as_str())]).await;

    assert_eq!(uploaded, 0, "the content is already on the instance");
    assert_eq!(result.created, 1);
    assert_eq!(result.deleted, 1);
    assert_eq!(agent.paths(), ["hdl/renamed.vhd"]);
}

#[tokio::test]
async fn content_that_does_not_match_its_digest_is_refused() {
    let agent = Agent::start().await;
    let honest = vw_sync::digest_bytes(b"the real thing");

    let response = agent.put_blob(&honest, b"something else").await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_digest_that_is_not_a_digest_is_refused() {
    let agent = Agent::start().await;

    // The digest becomes a filename in the content store, so anything that is
    // not 64 hex characters has to stop at the door.
    //
    // `..` is not in this list because it never reaches a handler: URL
    // normalization removes dot segments before routing, so the request
    // arrives at a path that matches nothing. The store refuses it anyway, and
    // `vw-sync` tests that directly — this is about what survives the wire.
    for hostile in ["not-hex", &"f".repeat(63), &"F".repeat(64), "0"] {
        let response = agent
            .put_blob(&Digest(hostile.to_owned()), b"payload")
            .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "'{hostile}' should be refused",
        );
    }
}

#[tokio::test]
async fn a_manifest_cannot_write_outside_the_tree() {
    let agent = Agent::start().await;
    let payload = "payload";
    let digest = vw_sync::digest_bytes(payload.as_bytes());
    agent.put_blob(&digest, payload.as_bytes()).await;

    for path in ["../escaped.vhd", "/etc/passwd", "hdl/../../escaped.vhd"] {
        let response = agent.commit_raw(&manifest_of(&[(path, payload)])).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "'{path}' should be refused",
        );
    }
}

#[tokio::test]
async fn committing_before_delivering_is_refused() {
    let agent = Agent::start().await;

    // Nothing has been uploaded, so the commit cannot be satisfied. It should
    // say so rather than write a tree with holes in it.
    let response = agent
        .commit_raw(&manifest_of(&[("hdl/top.vhd", "entity top is end;")]))
        .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(agent.paths().is_empty());
}

/// A request that does not say which version of the API it speaks is refused
/// rather than guessed at.
///
/// The agent has endpoints that exist only from a given version onwards. A
/// `vw-svc` too old to know about them says so, and gets what it was written
/// for; one that said nothing would be handed the newest of everything on the
/// assumption that it could cope, which is the kind of assumption that is
/// right until it is not.
#[tokio::test]
async fn a_request_that_names_no_api_version_is_refused() {
    let agent = Agent::start().await;

    let response = reqwest::Client::new()
        .post(format!(
            "{}/environment/{ENVIRONMENT}/sync/plan",
            agent.base_url
        ))
        .json(&TreeManifest::default())
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The flush endpoint exists, and says plainly that it has nowhere to send
/// anything until somebody says where artifacts go.
///
/// An agent that answered a flush cheerfully in that state would tell whoever
/// asked that the store was up to date, which is the one thing a flush must
/// never do wrongly — the caller is about to collect on the strength of it.
#[tokio::test]
async fn flushing_before_anyone_said_where_artifacts_go_is_refused() {
    let agent = Agent::start().await;

    let response = agent
        .client
        .post(format!(
            "{}/environment/{ENVIRONMENT}/artifact-flush",
            agent.base_url
        ))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_request_for_another_environment_is_refused() {
    let agent = Agent::start().await;

    // An agent belongs to one environment. Being asked about a different one
    // means something upstream routed badly, and serving it would put one
    // developer's source on another's instance.
    let response = agent
        .plan_raw("jalad", &TreeManifest::default())
        .await
        .expect("plan request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_build_output_directory_survives_synchronization() {
    let agent = Agent::start().await;
    agent.sync(&[("hdl/top.vhd", "entity top is end;")]).await;

    // What a build leaves behind afterwards.
    std::fs::create_dir_all(agent.root.join("target/synth")).expect("mkdir");
    std::fs::write(agent.root.join("target/synth/top.dcp"), "checkpoint")
        .expect("write");

    agent
        .sync(&[("hdl/top.vhd", "entity top is end; -- edited")])
        .await;

    assert_eq!(agent.contents("target/synth/top.dcp"), "checkpoint");
}

#[tokio::test]
async fn a_cleared_agent_asks_for_the_whole_tree_again() {
    let agent = Agent::start().await;
    let files = [
        ("hdl/top.vhd", "entity top is end;"),
        ("vw.toml", "[workspace]"),
    ];
    agent.sync(&files).await;

    // What `vw cloud sync --force` does: rather than argue with the instance
    // about what it has, leave it with nothing to argue about.
    let cleared = agent.clear().await;
    assert_eq!(cleared.deleted, 2);
    assert!(agent.paths().is_empty());

    let (result, uploaded) = agent.sync(&files).await;
    assert_eq!(uploaded, 2, "everything should be sent again");
    assert_eq!(result.created, 2);
    assert_eq!(result.unchanged, 0);
    assert_eq!(agent.contents("hdl/top.vhd"), "entity top is end;");
}

#[tokio::test]
async fn clearing_leaves_a_build_where_it_stands() {
    let agent = Agent::start().await;
    agent.sync(&[("hdl/top.vhd", "entity top is end;")]).await;
    std::fs::create_dir_all(agent.root.join("target/synth")).expect("mkdir");
    std::fs::write(agent.root.join("target/synth/top.dcp"), "checkpoint")
        .expect("write");

    agent.clear().await;

    assert_eq!(agent.contents("target/synth/top.dcp"), "checkpoint");
}

#[tokio::test]
async fn clearing_another_environment_is_refused() {
    let agent = Agent::start().await;
    agent.sync(&[("hdl/top.vhd", "entity top is end;")]).await;

    // Of everything relayed here this is the one worth being surest about:
    // serving it would delete a developer's tree on somebody else's say-so.
    let response = agent.clear_raw("jalad").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(agent.paths(), ["hdl/top.vhd"]);
}

fn credentials(token: &str) -> Credentials {
    Credentials {
        user: "picard".to_owned(),
        token: token.to_owned(),
    }
}

#[tokio::test]
async fn credentials_land_where_a_build_will_find_them() {
    let agent = Agent::start().await;

    let response = agent
        .put_credentials(ENVIRONMENT, &credentials("ghp_darmokandjalad"))
        .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let contents = std::fs::read_to_string(&agent.netrc).expect("read netrc");
    assert!(contents.contains("machine github.com"), "{contents}");
    assert!(contents.contains("login picard"), "{contents}");
    assert!(
        contents.contains("password ghp_darmokandjalad"),
        "{contents}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_credentials_file_is_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let agent = Agent::start().await;

    agent
        .put_credentials(ENVIRONMENT, &credentials("ghp_darmokandjalad"))
        .await;

    let mode = std::fs::metadata(&agent.netrc)
        .expect("stat netrc")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
}

#[tokio::test]
async fn credentials_for_another_environment_are_refused() {
    let agent = Agent::start().await;

    // The instance would otherwise take a stranger's token and hand it to
    // whatever it builds next.
    let response = agent
        .put_credentials("jalad", &credentials("ghp_darmokandjalad"))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!agent.netrc.exists());
}

#[tokio::test]
async fn clearing_a_tree_does_not_take_the_credentials_with_it() {
    // `--force` throws away the source tree. Credentials are not source, and
    // an instance that lost them would fail its next fetch for no reason the
    // developer could see.
    let agent = Agent::start().await;
    agent
        .put_credentials(ENVIRONMENT, &credentials("ghp_darmokandjalad"))
        .await;
    agent.sync(&[("hdl/top.vhd", "entity top is end;")]).await;

    agent.clear().await;

    assert!(agent.netrc.is_file());
}

#[tokio::test]
async fn build_output_can_be_removed_over_http() {
    let agent = Agent::start().await;
    agent.sync(&[("hdl/top.vhd", "entity top is end;")]).await;
    std::fs::create_dir_all(agent.root.join("target/synth")).expect("mkdir");
    std::fs::write(agent.root.join("target/synth/top.dcp"), "checkpoint")
        .expect("write");

    let response = agent.clean_raw(ENVIRONMENT).await;

    assert_eq!(response.status(), StatusCode::OK);
    let result: serde_json::Value = response.json().await.expect("decode");
    assert_eq!(result["existed"], true);
    assert!(result["bytes"].as_u64().expect("bytes") > 0);
    assert!(!agent.root.join("target").exists());
    // The source is still here, so the next sync has nothing to do.
    assert_eq!(agent.paths(), ["hdl/top.vhd"]);
}

#[tokio::test]
async fn cleaning_another_environment_is_refused() {
    let agent = Agent::start().await;
    std::fs::create_dir_all(agent.root.join("target")).expect("mkdir");
    std::fs::write(agent.root.join("target/keep.me"), "output").expect("write");

    let response = agent.clean_raw("jalad").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(agent.root.join("target/keep.me").exists());
}
