// The property the remote flow lives or dies by: output reaches the developer
// while the command is still running.
//
// A synthesis run produces messages for minutes before it produces a result.
// If those only arrived when the command finished, watching a remote build
// would mean watching nothing at all, and the whole thing would be worse than
// useless — it would look hung. So this drives a `RemoteBackend` against an
// agent that deliberately takes its time, and checks that the chunks land as
// they are sent rather than in a heap at the end.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use vw_eda::{EdaBackend, StreamKind};
use vw_remote::{RemoteBackend, SessionEvent};

/// How long the pretend worker waits between chunks.
const GAP: Duration = Duration::from_millis(200);

/// An agent that answers one eval with `chunks`, spaced out in time, and then
/// a result.
async fn agent_that_dawdles(chunks: Vec<(StreamKind, &'static str)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr").to_string();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket =
            WebSocketStream::from_raw_socket(stream, Role::Server, None).await;

        // Wait to be asked for something.
        let request = socket.next().await.expect("a request").expect("frame");
        let request: vw_eda::Request = match request {
            Message::Text(text) => {
                serde_json::from_str(&text).expect("a request")
            }
            other => panic!("unexpected frame: {other:?}"),
        };

        for (kind, data) in chunks {
            tokio::time::sleep(GAP).await;
            let event = SessionEvent::Chunk {
                kind,
                data: data.to_owned(),
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&event).expect("encode"),
                ))
                .await
                .expect("send chunk");
        }

        tokio::time::sleep(GAP).await;
        let done = SessionEvent::Response(vw_eda::Response::ok(
            request.id,
            serde_json::json!("finished"),
        ));
        socket
            .send(Message::Text(serde_json::to_string(&done).expect("encode")))
            .await
            .expect("send response");
    });

    address
}

async fn connect(address: &str) -> RemoteBackend<TcpStream> {
    let stream = TcpStream::connect(address).await.expect("connect");
    RemoteBackend::new(
        WebSocketStream::from_raw_socket(stream, Role::Client, None).await,
    )
}

#[tokio::test]
async fn output_arrives_while_the_command_is_still_running() {
    let address = agent_that_dawdles(vec![
        (StreamKind::Stdout, "starting synthesis\n"),
        (StreamKind::Info, "INFO: [Synth 8-6157] done elaborating\n"),
        (StreamKind::Stdout, "wrapping up\n"),
    ])
    .await;
    let mut backend = connect(&address).await;

    // When each chunk reached the caller, measured from the moment the command
    // was issued.
    let seen: Arc<Mutex<Vec<(Duration, StreamKind, String)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let recorder = Arc::clone(&seen);
    backend.set_stdout_sink(Box::new(move |kind, chunk: &str| {
        recorder.lock().expect("lock").push((
            started.elapsed(),
            kind,
            chunk.to_owned(),
        ));
    }));

    let output = backend.eval("synth_design").await.expect("eval");
    let finished = started.elapsed();
    let seen = seen.lock().expect("lock").clone();

    assert_eq!(seen.len(), 3, "every chunk should have arrived: {seen:?}");

    // The point of the whole exercise. The first chunk must have been in the
    // caller's hands long before the command answered — not merely delivered
    // in the right order once it was over.
    assert!(
        seen[0].0 < finished / 2,
        "the first chunk arrived at {:?}, but the command only finished at \
         {finished:?} — that is not streaming, that is a transcript",
        seen[0].0,
    );

    // And spaced the way the agent sent them, rather than bunched together at
    // the end, which is what a buffer somewhere in the middle would produce.
    for pair in seen.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(
            gap > GAP / 2,
            "chunks {:?} and {:?} arrived {gap:?} apart; something is holding \
             them",
            pair[0].2,
            pair[1].2,
        );
    }

    assert_eq!(
        seen[1].1,
        StreamKind::Info,
        "the severity survived the trip"
    );
    assert_eq!(output.value, "finished");
    assert!(
        output.stdout.is_empty(),
        "with a sink installed the chunks belong to it, not to the result — \
         otherwise the caller prints everything twice",
    );
}

#[tokio::test]
async fn without_a_sink_the_output_comes_back_with_the_result() {
    // What `vw check`'s in-process runs and any other non-rendering caller
    // relies on: no sink means the output is not thrown away, it is collected.
    let address = agent_that_dawdles(vec![
        (StreamKind::Stdout, "one\n"),
        (StreamKind::Stdout, "two\n"),
    ])
    .await;
    let mut backend = connect(&address).await;

    let output = backend.eval("puts one; puts two").await.expect("eval");

    assert_eq!(output.stdout, "one\ntwo\n");
}

#[tokio::test]
async fn a_worker_that_dies_is_reported_rather_than_waited_on() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr").to_string();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket =
            WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
        let _ = socket.next().await;
        let event = SessionEvent::Fatal {
            message: "vivado exited during elaboration".to_owned(),
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&event).expect("encode"),
            ))
            .await
            .expect("send");
    });

    let mut backend = connect(&address).await;
    let failure = backend.eval("synth_design").await.expect_err("should fail");

    assert!(
        failure
            .to_string()
            .contains("vivado exited during elaboration"),
        "the reason should survive: {failure}",
    );

    // And a second command should fail at once with the same reason rather
    // than hanging on an answer that is never coming.
    let again = backend.eval("place_design").await.expect_err("should fail");
    assert!(again.to_string().contains("vivado exited"), "{again}");
}
