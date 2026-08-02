// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The agent's half: a real Vivado worker, driven by whoever is on the socket.
//!
//! Everything Vivado needs to be told is worked out here rather than sent:
//! which part, which variant, where the sources are, whether a checkpoint is
//! still good. All of those are answers about the tree, and the tree is here.
//! The client sends the two flags it cannot know — `--part` and `--variant` —
//! and nothing else.
//!
//! Reading and writing are separated on purpose. A command can run for minutes
//! and produce output the whole time, so the side that forwards output cannot
//! be the side that is blocked waiting for the command to finish.

use camino::{Utf8Path, Utf8PathBuf};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use vw_eda::{EdaBackend, Request, RequestOp};

use crate::protocol::{SessionEvent, SessionParams};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("resolving what to build: {0}")]
    Selection(String),
    #[error("starting vivado")]
    Spawn(#[source] vw_eda::BackendError),
    #[error("talking to the client")]
    Socket(#[source] tokio_tungstenite::tungstenite::Error),
}

/// Run a session against the workspace at `root` until the client is done.
///
/// Returns when the client asks to shut down or goes away. Vivado is torn down
/// either way — a worker whose client has vanished is holding a great deal of
/// memory for nobody.
pub async fn serve<S>(
    socket: WebSocketStream<S>,
    root: &Utf8Path,
    params: SessionParams,
) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut outgoing, mut incoming) = socket.split();
    let (events, mut to_send) = mpsc::unbounded_channel::<SessionEvent>();

    // One task owns writing. Output produced while a command runs goes out as
    // it is produced rather than piling up behind the command — which is the
    // difference between watching a synthesis run and staring at a blank
    // terminal for twenty minutes.
    let writer = tokio::spawn(async move {
        while let Some(event) = to_send.recv().await {
            let text = match serde_json::to_string(&event) {
                Ok(text) => text,
                Err(e) => {
                    tracing::error!("cannot encode a session event: {e}");
                    continue;
                }
            };
            if outgoing.send(Message::Text(text)).await.is_err() {
                // The client is gone. Nothing left to say to it.
                break;
            }
        }
        let _ = outgoing.close().await;
    });

    let result = match start(&events, root, &params).await {
        Ok(backend) => drive(&mut incoming, &events, Box::new(backend)).await,
        Err(e) => Err(e),
    };

    if let Err(e) = &result {
        // The client is owed a reason. It cannot see this instance's log, and
        // a session that simply closed would look like a network fault.
        let _ = events.send(SessionEvent::Fatal {
            message: e.to_string(),
        });
    }

    drop(events);
    let _ = writer.await;

    result
}

/// Pump requests through a worker until the client is done with it.
///
/// Takes the worker rather than making one so that the part worth testing —
/// what happens when the developer walks away mid-command — can be tested
/// without a vivado installation.
async fn drive<S>(
    incoming: &mut futures::stream::SplitStream<WebSocketStream<S>>,
    events: &UnboundedSender<SessionEvent>,
    mut backend: Box<dyn EdaBackend>,
) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    while let Some(message) = incoming.next().await {
        let message = message.map_err(SessionError::Socket)?;

        let text = match message {
            Message::Text(text) => text,
            Message::Binary(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!("discarding a non-utf8 request: {e}");
                    continue;
                }
            },
            // The client hung up. Not a failure — it is how a run ends when
            // the user interrupts one.
            Message::Close(_) => break,
            _ => continue,
        };

        let request: Request = match serde_json::from_str(&text) {
            Ok(request) => request,
            Err(e) => {
                tracing::warn!("discarding an unreadable request: {e}");
                continue;
            }
        };

        let shutting_down = matches!(request.op, RequestOp::Shutdown);
        let id = request.id;

        // The command and the socket are watched together. A synthesis run
        // takes minutes, and for all of them the developer may press Ctrl-C —
        // so the socket cannot go unread until the command finishes, or the
        // answer to "stop" would be "in a little while". Nothing else arrives
        // mid-command: a request is answered before the next is sent, so
        // anything on the socket now is the client leaving.
        let outcome = tokio::select! {
            result = backend.send(request) => Ran(result),
            () = client_left(incoming) => Left,
        };

        match outcome {
            Ran(Ok(response)) => {
                let _ = events.send(SessionEvent::Response(response));
            }
            Ran(Err(e)) => {
                // The worker itself failed, not the command — a command that
                // fails comes back as an error response. There is nothing left
                // to run requests against.
                let _ = events.send(SessionEvent::Fatal {
                    message: format!("the vivado worker failed: {e}"),
                });
                tracing::error!("worker failed answering request {id}: {e}");
                break;
            }
            Left => {
                // Killed rather than asked to stop. `shutdown` sends vivado a
                // request and waits for the reply, and a vivado in the middle
                // of `synth_design` will not read it for another twenty
                // minutes — by which time it has finished the work nobody is
                // waiting for and burned an instance doing it. Dropping the
                // backend kills the process.
                tracing::info!(
                    "client left while request {id} was running; killing                      vivado"
                );
                drop(backend);
                return Ok(());
            }
        }

        if shutting_down {
            break;
        }
    }

    let _ = backend.shutdown().await;

    Ok(())
}

/// What became of a request.
use Outcome::{Left, Ran};
enum Outcome {
    /// The worker answered, for better or worse.
    Ran(Result<vw_eda::Response, vw_eda::BackendError>),
    /// The developer went away while it was still running.
    Left,
}

/// Resolve when there is no longer a client on the other end.
///
/// A close frame is the polite version; a connection that simply ends is what
/// happens when the process is killed outright. Neither leaves anyone to send
/// an answer to, so they are the same event.
async fn client_left<S>(
    incoming: &mut futures::stream::SplitStream<WebSocketStream<S>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        match incoming.next().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
            // Anything else would be a client talking over its own
            // outstanding request. There is nothing useful to do with it and
            // it is not a reason to abandon the command.
            Some(Ok(_)) => continue,
        }
    }
}

/// Bring up Vivado for this session.
async fn start(
    events: &UnboundedSender<SessionEvent>,
    root: &Utf8Path,
    params: &SessionParams,
) -> Result<vw_vivado::VivadoBackend, SessionError> {
    let note = |message: String| {
        let _ = events.send(SessionEvent::Note { message });
    };

    // Dependencies are fetched here rather than synchronized. They are named
    // by revision in `vw.lock`, so the instance can get them itself and get
    // exactly what the developer's machine would — using the credentials the
    // sync that preceded this put in place.
    fetch_dependencies(root, &note).await;

    let selection = vw_vivado::resolve_workspace_selection(
        root,
        params.part.as_deref(),
        params.variant.as_deref(),
    )
    .map_err(SessionError::Selection)?;

    for message in &selection.notes {
        note(message.clone());
    }

    // Only consulted on demand: nothing has been shipped to this Vivado yet,
    // so an empty map is the truth, and `compile_htcl_module` will load what
    // it needs from the tree.
    let preload: vw_vivado::SharedPreload =
        std::sync::Arc::new(std::sync::RwLock::new(Default::default()));
    let cw_count: vw_vivado::SharedCriticalWarningCount =
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    let rpc_handler = vw_vivado::make_handler_full(
        Some(root.as_std_path().to_path_buf()),
        selection.active_variant.clone(),
        preload,
        cw_count.clone(),
    );

    let raw_log =
        match vw_vivado::raw_log_path_for_workspace(root.as_std_path()) {
            Ok(path) => Some(path),
            Err(e) => {
                note(format!("raw vivado log unavailable: {e}"));
                None
            }
        };

    // Said before the wait rather than after it. Vivado takes the better part
    // of a minute to come up, and a minute of silence on a remote build reads
    // as a hang — the developer cannot see the process starting the way they
    // could if it were on their own machine.
    note("starting vivado".to_owned());

    let mut backend =
        vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
            verbose: params.verbose,
            info_with_stack: params.info_with_stack,
            rpc_handler: Some(rpc_handler),
            auto_project: selection.auto_project,
            raw_log,
            ..Default::default()
        })
        .await
        .map_err(SessionError::Spawn)?;

    note("vivado is ready".to_owned());

    // Everything the worker produces goes straight out. The counter the
    // checkpoint gates read lives on this side too, because the RPC that
    // reads it is answered on this side.
    let sink_events = events.clone();
    backend.set_stdout_sink(Box::new(move |kind, chunk: &str| {
        if matches!(kind, vw_eda::StreamKind::CriticalWarning) {
            cw_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = sink_events.send(SessionEvent::Chunk {
            kind,
            data: chunk.to_owned(),
        });
    }));

    Ok(backend)
}

/// Make sure the workspace's dependencies are on this instance.
///
/// Best effort: a failure here is reported and the run continues, because the
/// run may not need the missing dependency and failing now would be a worse
/// answer than failing where it is actually used.
async fn fetch_dependencies(root: &Utf8Path, note: &impl Fn(String)) {
    if vw_lib::dependencies_present(root) {
        return;
    }

    note("fetching missing dependencies".to_owned());

    let credentials = vw_lib::get_access_credentials_from_netrc("github.com")
        .ok()
        .flatten();
    if credentials.is_none() {
        note(
            "no github credentials on this instance; a private dependency \
             will not be fetchable"
                .to_owned(),
        );
    }

    if let Err(e) = vw_lib::update_workspace_with_token(root, credentials).await
    {
        note(format!("could not fetch dependencies: {e}"));
    }
}

/// Where a session's workspace lives on an instance.
///
/// The same tree synchronization writes to. A build has to see what was
/// pushed, and there is only one copy.
pub fn workspace_root(tree: &Utf8Path) -> Utf8PathBuf {
    tree.to_owned()
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use futures::SinkExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::protocol::Role;

    use super::*;

    /// A worker that takes a very long time and notices being dropped.
    ///
    /// Stands in for a vivado in the middle of `synth_design`: it will not
    /// answer for a good while, and the only way to stop it is to kill it.
    struct SlowWorker {
        killed: Arc<AtomicBool>,
    }

    impl Drop for SlowWorker {
        fn drop(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl EdaBackend for SlowWorker {
        fn name(&self) -> &str {
            "slow"
        }

        async fn eval(
            &mut self,
            _tcl: &str,
        ) -> Result<vw_eda::EvalOutput, vw_eda::BackendError> {
            unreachable!("the session only ever calls send")
        }

        async fn send(
            &mut self,
            request: Request,
        ) -> Result<vw_eda::Response, vw_eda::BackendError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(vw_eda::Response::ok(request.id, serde_json::json!("late")))
        }

        fn set_stdout_sink(&mut self, _sink: vw_eda::StdoutSink) {}

        async fn shutdown(&mut self) -> Result<(), vw_eda::BackendError> {
            // Vivado's real shutdown asks the interpreter to exit and waits
            // for it to answer, which a busy one will not do. Modelled as
            // never returning, because that is what it amounts to.
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    /// A session with a worker on one end and a socket the test drives.
    async fn session(
        killed: Arc<AtomicBool>,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio_tungstenite::WebSocketStream<TcpStream>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");

        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                stream,
                Role::Server,
                None,
            )
            .await;
            let (_outgoing, mut incoming) = socket.split();
            let (events, _drain) = mpsc::unbounded_channel();
            drive(&mut incoming, &events, Box::new(SlowWorker { killed }))
                .await
                .expect("drive");
        });

        let stream = TcpStream::connect(address).await.expect("connect");
        let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
            stream,
            Role::Client,
            None,
        )
        .await;

        (served, client)
    }

    #[tokio::test]
    async fn a_client_that_leaves_takes_the_worker_with_it() {
        let killed = Arc::new(AtomicBool::new(false));
        let (served, mut client) = session(Arc::clone(&killed)).await;

        // Ask for something long, then walk away — the developer pressing
        // Ctrl-C twenty seconds into a synthesis run.
        let request = Request {
            id: 1,
            op: RequestOp::Eval {
                tcl: "synth_design".to_owned(),
            },
        };
        client
            .send(Message::Text(
                serde_json::to_string(&request).expect("encode"),
            ))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(client);

        // Promptly, not when the command it no longer cares about finishes.
        // The worker holds an instance's worth of memory and would otherwise
        // spend twenty minutes producing something nobody will collect.
        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .expect("the session should end when the client does")
            .expect("session task");

        assert!(
            killed.load(Ordering::SeqCst),
            "the worker should have been killed, not asked to stop",
        );
    }

    #[tokio::test]
    async fn a_close_frame_ends_it_too() {
        // The polite version of the same thing, which is what a client that
        // gets to run its own shutdown sends.
        let killed = Arc::new(AtomicBool::new(false));
        let (served, mut client) = session(Arc::clone(&killed)).await;

        let request = Request {
            id: 1,
            op: RequestOp::Eval {
                tcl: "synth_design".to_owned(),
            },
        };
        client
            .send(Message::Text(
                serde_json::to_string(&request).expect("encode"),
            ))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.close(None).await.expect("close");

        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .expect("the session should end on a close frame")
            .expect("session task");

        assert!(killed.load(Ordering::SeqCst));
    }
}
