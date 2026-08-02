// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! A worker on another machine, driven as though it were on this one.
//!
//! Implements the same [`EdaBackend`] the local Vivado worker implements, so
//! everything above it — `vw run`'s eval loop, the REPL's session, the block
//! renderer, the exit-code ladder — cannot tell the difference and does not
//! have to be told. The one thing it must keep faith with is streaming: a
//! caller that installed a sink gets its chunks while the command is still
//! running, exactly as it would locally, because that is the whole reason
//! anyone can bear to watch a synthesis run.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use vw_eda::{
    BackendError, EdaBackend, EvalOutput, Request, RequestOp, Response,
    ResponseResult, StdoutSink,
};

use crate::protocol::SessionEvent;

/// Where an agent's progress reports go.
///
/// Separate from the output sink because these are not the build talking, they
/// are the machinery around it: dependencies being fetched, vivado starting.
/// A caller that shows them tells the developer why nothing is happening yet,
/// which for the half minute vivado takes to come up is the difference between
/// waiting and wondering.
pub type NoteSink = Box<dyn FnMut(&str) + Send>;

/// A Vivado worker reached over a session.
pub struct RemoteBackend<S> {
    socket: WebSocketStream<S>,
    stdout_sink: Option<StdoutSink>,
    note_sink: Option<NoteSink>,
    next_id: u64,
    /// Set once the far end says the session is over, so a later call fails
    /// with the reason rather than waiting for an answer that is not coming.
    fatal: Option<String>,
}

impl<S> RemoteBackend<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Drive the worker on the far end of `socket`.
    pub fn new(socket: WebSocketStream<S>) -> RemoteBackend<S> {
        RemoteBackend {
            socket,
            stdout_sink: None,
            note_sink: None,
            next_id: 1,
            fatal: None,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn write(&mut self, request: &Request) -> Result<(), BackendError> {
        let text = serde_json::to_string(request)?;
        self.socket
            .send(Message::Text(text))
            .await
            .map_err(|e| BackendError::Worker(format!("sending request: {e}")))
    }

    /// Read until the answer to `id` arrives, feeding everything else where it
    /// belongs on the way.
    ///
    /// Returns the response and whatever output was produced while waiting.
    /// The output is only accumulated when no sink is installed — with one,
    /// the sink owns the chunks, which is the same bargain the local backend
    /// strikes and what keeps `EvalOutput::stdout` from silently doubling
    /// everything the caller has already printed.
    async fn read_until(
        &mut self,
        id: u64,
    ) -> Result<(Response, String), BackendError> {
        let mut stdout = String::new();

        loop {
            let message = self.socket.next().await.ok_or_else(|| {
                BackendError::Worker(
                    "the session ended before the command answered".to_owned(),
                )
            })?;

            let message = message.map_err(|e| {
                BackendError::Worker(format!("reading from the session: {e}"))
            })?;

            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => String::from_utf8(bytes)
                    .map_err(|e| BackendError::Worker(e.to_string()))?,
                // Pings are answered by the library; a close means the far end
                // is gone and there is no answer coming.
                Message::Close(_) => {
                    return Err(BackendError::Worker(
                        self.fatal.clone().unwrap_or_else(|| {
                            "the session was closed by the other end".to_owned()
                        }),
                    ))
                }
                _ => continue,
            };

            match serde_json::from_str::<SessionEvent>(&text)? {
                SessionEvent::Chunk { kind, data } => {
                    match self.stdout_sink.as_mut() {
                        Some(sink) => sink(kind, &data),
                        None => stdout.push_str(&data),
                    }
                }
                SessionEvent::Note { message } => {
                    match self.note_sink.as_mut() {
                        Some(sink) => sink(&message),
                        None => tracing::info!("{message}"),
                    }
                }
                SessionEvent::Response(response) if response.id == id => {
                    return Ok((response, stdout))
                }
                // An answer to something else. Nothing issues two requests at
                // once — `eval` takes `&mut self` — so this is a duplicate or
                // a stale reply, and dropping it is better than mistaking it
                // for the one being waited on.
                SessionEvent::Response(response) => {
                    tracing::warn!(
                        "ignoring a response for {} while waiting for {id}",
                        response.id,
                    );
                }
                SessionEvent::Fatal { message } => {
                    self.fatal = Some(message.clone());
                    return Err(BackendError::Worker(message));
                }
            }
        }
    }

    /// Install a sink for the agent's progress reports.
    ///
    /// Not on [`EdaBackend`] because a local worker has nothing to report: the
    /// waiting it does is on this machine, where the caller can already see
    /// it. Only a session across a network has a gap to explain.
    pub fn set_note_sink(&mut self, sink: NoteSink) {
        self.note_sink = Some(sink);
    }

    /// Fail immediately if the session has already been declared over.
    fn check_alive(&self) -> Result<(), BackendError> {
        match &self.fatal {
            Some(message) => Err(BackendError::Tcl {
                message: message.clone(),
                code: Some("VW_SESSION_DEAD".to_owned()),
                info: None,
                stdout: String::new(),
            }),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl<S> EdaBackend for RemoteBackend<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    fn name(&self) -> &str {
        "vivado (remote)"
    }

    async fn eval(&mut self, tcl: &str) -> Result<EvalOutput, BackendError> {
        self.check_alive()?;

        let id = self.alloc_id();
        self.write(&Request {
            id,
            op: RequestOp::Eval { tcl: tcl.into() },
        })
        .await?;

        let (response, stdout) = self.read_until(id).await?;

        match response.result {
            ResponseResult::Ok { result, .. } => {
                let value = match result {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                Ok(EvalOutput { value, stdout })
            }
            ResponseResult::Err { error, .. } => Err(BackendError::Tcl {
                message: error.message,
                code: error.code,
                info: error.info,
                stdout,
            }),
        }
    }

    async fn send(
        &mut self,
        mut request: Request,
    ) -> Result<Response, BackendError> {
        self.check_alive()?;

        if request.id == 0 {
            request.id = self.alloc_id();
        }
        let id = request.id;
        self.write(&request).await?;
        let (response, _stdout) = self.read_until(id).await?;
        Ok(response)
    }

    fn set_stdout_sink(&mut self, sink: StdoutSink) {
        self.stdout_sink = Some(sink);
    }

    async fn shutdown(&mut self) -> Result<(), BackendError> {
        // A session that has already died has nothing to shut down, and saying
        // so would turn a clean exit into an error the user has to read.
        if self.fatal.is_some() {
            return Ok(());
        }

        let id = self.alloc_id();
        let _ = self
            .write(&Request {
                id,
                op: RequestOp::Shutdown,
            })
            .await;
        // The far end tears Vivado down and closes; either an answer or the
        // close will end this, and neither is worth failing over.
        let _ = self.read_until(id).await;
        let _ = self.socket.close(None).await;

        Ok(())
    }
}
