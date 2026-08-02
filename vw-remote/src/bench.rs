// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Running a workspace's testbenches on the instance that holds it.
//!
//! The same shape as a vivado session and for the same reason: a batch of
//! benches takes minutes and finishes one at a time, so the developer has to
//! see each result as it lands rather than a verdict at the end. What crosses
//! the socket is `vw-bench`'s own event stream, so the panel on a developer's
//! terminal is driven by exactly the events it would be driven by locally.
//!
//! Nothing about *what* to run is decided here. Discovery reads the tree, and
//! the tree is on the instance.

use camino::Utf8Path;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// What an instance sends back while running a batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BenchEvent {
    /// Something that happened to a bench.
    Progress { event: vw_bench::Event },
    /// Every bench has finished.
    Done { passed: usize, failed: usize },
    /// The batch could not be run at all.
    Fatal { message: String },
}

/// Run the batch described by `request` and report as it goes.
///
/// `launch` says how the instance starts a single bench; the caller supplies
/// it because only the caller knows what binary it is.
pub async fn serve<S>(
    socket: WebSocketStream<S>,
    root: &Utf8Path,
    request: vw_bench::Request,
    launch: vw_bench::Launch,
) -> Result<(), crate::SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut outgoing, mut incoming) = socket.split();
    let (events, mut to_send) =
        tokio::sync::mpsc::unbounded_channel::<BenchEvent>();

    // One task writes, as elsewhere: results arrive while the batch is still
    // running and must go out then, not after.
    let writer = tokio::spawn(async move {
        while let Some(event) = to_send.recv().await {
            let Ok(text) = serde_json::to_string(&event) else {
                continue;
            };
            if outgoing.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        let _ = outgoing.close().await;
    });

    // A developer who walks away should not leave an instance running a batch
    // nobody will read. Watched alongside the run rather than after it.
    let departed = tokio::spawn(async move {
        while let Some(Ok(message)) = incoming.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let running = run(root, request, launch, events.clone());
    tokio::pin!(running);

    tokio::select! {
        () = &mut running => {}
        _ = departed => {
            tracing::info!("client left; abandoning the batch");
        }
    }

    drop(events);
    let _ = writer.await;

    Ok(())
}

/// The batch itself, reporting into `events`.
async fn run(
    root: &Utf8Path,
    request: vw_bench::Request,
    launch: vw_bench::Launch,
    events: tokio::sync::mpsc::UnboundedSender<BenchEvent>,
) {
    let standard = match request.standard.parse::<vw_lib::VhdlStandard>() {
        Ok(standard) => standard,
        Err(e) => {
            let _ = events.send(BenchEvent::Fatal {
                message: format!(
                    "'{}' is not a vhdl standard: {e}",
                    request.standard
                ),
            });
            return;
        }
    };

    let names = match vw_bench::discover(root, &request) {
        Ok(names) => names,
        Err(e) => {
            let _ = events.send(BenchEvent::Fatal {
                message: e.to_string(),
            });
            return;
        }
    };

    if names.is_empty() {
        let _ = events.send(BenchEvent::Progress {
            event: vw_bench::Event::Discovered { names },
        });
        let _ = events.send(BenchEvent::Done {
            passed: 0,
            failed: 0,
        });
        return;
    }

    if let Err(e) = vw_bench::prepare(root, standard).await {
        let _ = events.send(BenchEvent::Fatal {
            message: e.to_string(),
        });
        return;
    }

    let relay = events.clone();
    let summary =
        vw_bench::run(root, names, request.concurrency, launch, move |event| {
            let _ = relay.send(BenchEvent::Progress { event });
        })
        .await;

    let _ = events.send(BenchEvent::Done {
        passed: summary.passed,
        failed: summary.failed,
    });
}
