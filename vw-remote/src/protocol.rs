// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! What a client and an agent say to each other over a session.
//!
//! Deliberately thin. `vw-eda` already defines what driving a TCL worker looks
//! like — a [`Request`] in, output chunks and a [`Response`] back — and that
//! protocol was built to stream, because a synthesis run produces output for
//! minutes before it produces a result. Moving the worker to another machine
//! does not change any of that. It changes the pipe.
//!
//! So this adds two things and no more: an envelope that says which of the two
//! kinds of thing is coming back, and a way for the agent to report a failure
//! that belongs to no particular request — Vivado dying, or never starting.

use serde::{Deserialize, Serialize};
use vw_eda::{Request, Response, StreamKind};

/// What a client sends.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionRequest {
    /// Something for the worker to do — the same [`Request`] a local backend
    /// would receive, which is the point: the client is driving the same
    /// worker it always drove.
    Run { request: Request },

    /// Abandon whatever is running, but keep the session.
    ///
    /// What Ctrl-C means in the REPL. Not a request for the worker, which is
    /// busy and by definition not reading — it is a request about the worker,
    /// and the agent acts on it by signalling the process the way an
    /// interactive user's terminal would if vivado were on their own machine.
    /// The eval then comes back as an error, the interpreter survives, and the
    /// developer keeps everything they had loaded.
    Interrupt,
}

/// What an agent sends back.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Output, as it is produced.
    ///
    /// Already classified, because classification reads Vivado's own message
    /// format out of the byte stream and the agent is the side holding that
    /// stream. Sending raw text and classifying at the far end would mean two
    /// implementations that have to agree.
    Chunk { kind: StreamKind, data: String },

    /// The result of a request.
    Response(Response),

    /// Something worth saying that is not output and not a result — a stale
    /// project wiped, dependencies fetched, a fallback taken while starting.
    ///
    /// These happen before any request is in flight, so they cannot be
    /// reported as one failing.
    Note { message: String },

    /// The session cannot continue.
    ///
    /// Distinct from an error response: a response means a command failed and
    /// the worker is still there to take another. This means there is no
    /// worker, and every request still outstanding will never be answered.
    Fatal { message: String },
}

/// How a client asks for a session to be set up.
///
/// Everything the agent needs to spawn a worker the way this run wants it,
/// and nothing it can work out for itself: the tree, the workspace config and
/// the dependency cache are all already on its side, so none of them are here.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionParams {
    /// `--part`, for a workspace whose parts are declared at the top level.
    pub part: Option<String>,
    /// `--variant`, for a workspace that declares variants.
    pub variant: Option<String>,
    /// Attach the Tcl call stack to INFO messages as well as warnings and
    /// errors.
    pub info_with_stack: bool,
    /// Forward Vivado's unclassified chatter — its banner, source echo and
    /// idle output — rather than reading and discarding it.
    pub verbose: bool,
}

impl SessionParams {
    /// Render as query parameters for the session URL.
    ///
    /// Hand-rolled rather than pulled from a query-string crate: there are
    /// four fields, they are all scalars, and the agent parses them back with
    /// the same list in view.
    pub fn to_query(&self) -> Vec<(&'static str, String)> {
        let mut query = Vec::new();
        if let Some(part) = &self.part {
            query.push(("part", part.clone()));
        }
        if let Some(variant) = &self.variant {
            query.push(("variant", variant.clone()));
        }
        if self.info_with_stack {
            query.push(("info_with_stack", "true".to_owned()));
        }
        if self.verbose {
            query.push(("verbose", "true".to_owned()));
        }
        query
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn an_event_says_which_kind_it_is() {
        let chunk = SessionEvent::Chunk {
            kind: StreamKind::CriticalWarning,
            data: "CRITICAL WARNING: [Synth 8-7080]\n".to_owned(),
        };
        let json = serde_json::to_string(&chunk).expect("serialize");

        assert!(json.contains(r#""event":"chunk""#), "{json}");
        assert!(json.contains(r#""kind":"critical_warning""#), "{json}");

        let back: SessionEvent =
            serde_json::from_str(&json).expect("deserialize");
        match back {
            SessionEvent::Chunk { kind, .. } => {
                assert_eq!(kind, StreamKind::CriticalWarning)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_response_survives_the_trip() {
        let event = SessionEvent::Response(vw_eda::Response::ok(
            7,
            serde_json::json!("done"),
        ));
        let json = serde_json::to_string(&event).expect("serialize");

        let back: SessionEvent =
            serde_json::from_str(&json).expect("deserialize");
        match back {
            SessionEvent::Response(r) => assert_eq!(r.id, 7),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_interrupt_is_distinguishable_from_work() {
        let interrupt = serde_json::to_string(&SessionRequest::Interrupt)
            .expect("serialize");
        let work = serde_json::to_string(&SessionRequest::Run {
            request: Request {
                id: 1,
                op: vw_eda::RequestOp::Eval {
                    tcl: "synth_design".to_owned(),
                },
            },
        })
        .expect("serialize");

        assert!(interrupt.contains(r#""op":"interrupt""#), "{interrupt}");
        assert!(work.contains(r#""op":"run""#), "{work}");

        // And the round trip keeps them apart, which is the whole point: an
        // interrupt mistaken for work would be queued behind the very thing
        // it is trying to stop.
        assert!(matches!(
            serde_json::from_str::<SessionRequest>(&interrupt),
            Ok(SessionRequest::Interrupt),
        ));
        assert!(matches!(
            serde_json::from_str::<SessionRequest>(&work),
            Ok(SessionRequest::Run { .. }),
        ));
    }

    #[test]
    fn only_what_was_asked_for_is_in_the_query() {
        let params = SessionParams {
            variant: Some("metro".to_owned()),
            ..Default::default()
        };

        assert_eq!(params.to_query(), [("variant", "metro".to_owned())]);
    }
}
