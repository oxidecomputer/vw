// The gap between a build finishing and its artifacts being in the store,
// driven the way a script drives it.
//
// The uploader is a poller: it waits to see a file stop changing before
// sending it, so an artifact is in the bucket a second or two after the last
// byte is written rather than immediately. Nobody collecting by hand has ever
// noticed, because thinking about collecting takes longer than that. A script
// that collects the moment a build ends notices every time, and what it gets
// is a listing that is short and looks exactly like a complete one.
//
// So there are two tests here and they are the same test twice: collect
// straight away and the artifact is not there yet, ask for a flush first and
// it is. The first one is the bug, the second is the reason `--flush` exists.

use std::io::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vw_api_types_versions::latest::{ArtifactFlush, S3Credentials};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times to try for a port before calling it a real failure.
const STARTUP_ATTEMPTS: usize = 5;
const ENVIRONMENT: &str = "darmok";
const BUCKET: &str = "vivado-darmok";

/// Long enough for the poller to have noticed several times over.
///
/// Only ever waited out in full by a failing test — the uploader settles a
/// file across two passes a second apart, so a working one is done in about
/// two seconds.
const PICKUP_TIMEOUT: Duration = Duration::from_secs(20);

/// The artifact these tests are about: written last, and the one a collection
/// taken too early comes back without.
const ARTIFACT: &str = "reports/worst-paths.csv";

/// A stand-in for the object store.
///
/// Answers a `PUT` of anything with `200` and remembers what it was called.
/// What the real store does with an artifact is not in question here; whether
/// the agent has sent it yet is the whole question.
struct Store {
    address: SocketAddr,
    received: Arc<Mutex<Vec<String>>>,
}

impl Store {
    async fn start() -> Store {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a store");
        let address = listener.local_addr().expect("read the store's address");
        let received = Arc::new(Mutex::new(Vec::new()));

        let keys = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(take_object(stream, Arc::clone(&keys)));
            }
        });

        Store { address, received }
    }

    /// Whether an artifact has arrived.
    fn holds(&self, artifact: &str) -> bool {
        let wanted = format!("/{BUCKET}/{artifact}");
        self.received
            .lock()
            .expect("the store's record")
            .contains(&wanted)
    }

    /// Everything that has arrived, for a failure message worth reading.
    fn contents(&self) -> Vec<String> {
        self.received.lock().expect("the store's record").clone()
    }

    /// Wait for an artifact to turn up on its own.
    async fn wait_for(&self, artifact: &str) -> bool {
        let deadline = Instant::now() + PICKUP_TIMEOUT;
        while Instant::now() < deadline {
            if self.holds(artifact) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// What to tell the agent so that it uploads here.
    fn credentials(&self) -> S3Credentials {
        S3Credentials {
            endpoint: format!("http://{}", self.address),
            port: self.address.port(),
            region: String::from("garage"),
            bucket: String::from(BUCKET),
            access_key_id: String::from("GK00000000000000000000000"),
            secret_access_key: String::from("shhh"),
        }
    }
}

/// Answer one upload and record the name it was sent under.
///
/// An artifact small enough to fit in one chunk is a plain `PUT`, which is
/// what every artifact in these tests is, so there is no multipart handshake
/// to imitate.
async fn take_object(
    mut stream: tokio::net::TcpStream,
    received: Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];

    // The head ends at the blank line.
    let head = loop {
        if let Some(at) =
            buffer.windows(4).position(|window| window == b"\r\n\r\n")
        {
            break at;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head]).into_owned();
    let mut lines = head.lines();
    let request = lines.next().unwrap_or_default().to_owned();

    // Read the body out before answering. A client whose upload is answered
    // before it has finished sending it sees the connection close under the
    // request, which reads as a failed upload rather than a served one.
    let declared = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer.len().saturating_sub(head.len() + 4);
    while body < declared {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => body += read,
        }
    }

    // `PUT /bucket/key HTTP/1.1`, path style, because the store is reached by
    // address rather than by name.
    let mut parts = request.split_whitespace();
    if let (Some("PUT"), Some(path)) = (parts.next(), parts.next()) {
        received
            .lock()
            .expect("the store's record")
            .push(path.to_owned());
    }

    let _ = stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;
    let _ = stream.shutdown().await;
}

/// Start an agent on `port`, with everything it needs under `base`.
fn spawn_agent(base: &Utf8Path, root: &Utf8Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_vw-agent"))
        .arg("serve")
        .args(["--address", "127.0.0.1"])
        .args(["--port", &port.to_string()])
        .args(["--environment", ENVIRONMENT])
        .args(["--root", root.as_str()])
        .args(["--store", base.join("store").as_str()])
        .args(["--netrc", base.join("netrc").as_str()])
        // Overridden because the default is under `/var/lib`, which a test has
        // no business writing to and usually cannot.
        .args([
            "--artifact-target",
            base.join("artifact-target.json").as_str(),
        ])
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A running agent with a workspace of its own.
struct Agent {
    child: Child,
    base_url: String,
    client: reqwest::Client,
    _dir: TempDir,
    root: Utf8PathBuf,
}

impl Agent {
    async fn start() -> Agent {
        let dir = TempDir::new().expect("scratch directory");
        let base = Utf8Path::from_path(dir.path())
            .expect("utf8 scratch directory")
            .to_owned();
        let root = base.join("root");
        std::fs::create_dir_all(&root).expect("workspace");
        let client = versioned_client();

        // A port is chosen by binding one and letting it go, so two agents
        // starting at the same moment can be handed the same one and whichever
        // gets there second fails to bind. Nothing is wrong when that happens
        // and there is nothing to do about it but pick another.
        for _ in 0..STARTUP_ATTEMPTS {
            let port = free_port();
            let mut child = spawn_agent(&base, &root, port);
            let base_url = format!("http://127.0.0.1:{port}");

            if listening(&client, &base_url, &mut child).await {
                return Agent {
                    child,
                    base_url,
                    client,
                    _dir: dir,
                    root,
                };
            }

            let _ = child.kill();
            let _ = child.wait();
        }

        panic!("vw-agent exited during startup {STARTUP_ATTEMPTS} times");
    }

    /// Tell the agent where its artifacts go, as `vw-svc` does on every sync.
    async fn upload_to(&self, store: &Store) {
        let response = self
            .client
            .put(format!(
                "{}/environment/{ENVIRONMENT}/artifact-target",
                self.base_url
            ))
            .json(&store.credentials())
            .send()
            .await
            .expect("set the artifact target");
        assert!(
            response.status().is_success(),
            "setting the artifact target answered {}",
            response.status(),
        );
    }

    /// Leave a finished artifact where a build would leave it.
    ///
    /// Flushed to disk before returning, so that "the build has written it" is
    /// true by the time the test goes looking — the race under test is the
    /// agent's, and one of the test's own would only muddle it.
    fn build_wrote(&self, artifact: &str, contents: &str) {
        let path = self.root.join("target").join(artifact);
        std::fs::create_dir_all(path.parent().expect("a parent"))
            .expect("build output directory");
        let mut file = std::fs::File::create(&path).expect("write an artifact");
        file.write_all(contents.as_bytes()).expect("write");
        file.sync_all().expect("flush an artifact to disk");
    }

    /// Wait for everything a build produced to reach the store.
    async fn flush(&self) -> ArtifactFlush {
        let response = self
            .client
            .post(format!(
                "{}/environment/{ENVIRONMENT}/artifact-flush",
                self.base_url
            ))
            .send()
            .await
            .expect("flush");
        assert!(
            response.status().is_success(),
            "flushing answered {}",
            response.status(),
        );
        response.json().await.expect("decode the flush")
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client that names the API version, as every real client does.
fn versioned_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static(
            vw_sync_api::API_VERSION_HEADER,
        ),
        vw_sync_api::latest_version()
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

/// Collecting the instant a build finishes comes back without the artifact it
/// just wrote.
///
/// This is the bug as it happens in CI: the build writes its last report, the
/// next step lists the store a moment later, and the listing is complete
/// apart from the file everybody was waiting for. Nothing in the result says
/// so, which is what makes it worth a test — the failure is a success with
/// something missing from it.
///
/// The second half is what makes the first half mean anything. An artifact
/// that never arrives would satisfy the assertion above just as well as one
/// that arrives late, and those are very different bugs, so this waits for the
/// uploader to get there on its own.
#[tokio::test]
async fn collecting_the_instant_a_build_finishes_misses_the_last_artifact() {
    let store = Store::start().await;
    let agent = Agent::start().await;
    agent.upload_to(&store).await;

    agent.build_wrote(ARTIFACT, "the last thing a build writes");

    assert!(
        !store.holds(ARTIFACT),
        "the store already had {ARTIFACT}, so this test proves nothing \
         about timing; it holds {:?}",
        store.contents(),
    );

    assert!(
        store.wait_for(ARTIFACT).await,
        "{ARTIFACT} never reached the store at all, so the miss above was \
         not a race; it holds {:?}",
        store.contents(),
    );
}

/// Flushing first gets the artifact that a bare collection misses.
///
/// Same build, same instant, one call in between. The flush returns only once
/// the agent has nothing left to send, so by the time anybody lists the store
/// the answer is the complete one — with no sleep here to make it true, which
/// is the point: waiting a bit longer is what CI was already doing.
#[tokio::test]
async fn flushing_first_gets_the_artifact_a_bare_collection_misses() {
    let store = Store::start().await;
    let agent = Agent::start().await;
    agent.upload_to(&store).await;

    agent.build_wrote(ARTIFACT, "the last thing a build writes");
    assert!(!store.holds(ARTIFACT), "not yet, as the other test shows");

    let flushed = agent.flush().await;

    assert!(
        flushed.settled,
        "the flush gave up before the build output came to rest",
    );
    assert!(
        store.holds(ARTIFACT),
        "the flush said it was done and {ARTIFACT} is not there; the store \
         holds {:?}",
        store.contents(),
    );
    // What rules out the other explanation. The store holding the artifact
    // after a call that took a second or two would look the same if the flush
    // did nothing and the ordinary poller happened to get there meanwhile;
    // this count is of what the flush itself sent, so a zero here would mean
    // exactly that.
    assert_eq!(
        flushed.uploaded, 1,
        "the flush reported sending nothing, so something else uploaded \
         {ARTIFACT} while it ran and this test is not measuring the flush",
    );
}

/// A second flush with nothing new to send says so.
///
/// Worth pinning because the count is what a caller reads to decide whether
/// waiting achieved anything, and a flush that re-sent everything it had ever
/// seen would report a number that looks like work being done — while
/// quietly costing an upload of every artifact on the instance, every time.
#[tokio::test]
async fn flushing_twice_sends_nothing_the_second_time() {
    let store = Store::start().await;
    let agent = Agent::start().await;
    agent.upload_to(&store).await;

    agent.build_wrote(ARTIFACT, "the last thing a build writes");
    assert_eq!(agent.flush().await.uploaded, 1);

    let again = agent.flush().await;

    assert!(again.settled);
    assert_eq!(
        again.uploaded, 0,
        "nothing changed, so the second flush had nothing to send",
    );
}
