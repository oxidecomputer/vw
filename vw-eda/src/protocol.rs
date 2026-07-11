// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Newline-delimited JSON wire protocol between `vw` and a vendor
//! TCL worker.
//!
//! v0 implements the `eval` op only. The `eval_structured` op (Phase 4
//! of the project plan) will land as an additional [`RequestOp`]
//! variant without breaking the wire format.

use serde::{Deserialize, Serialize};

/// A request sent from `vw` to the worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// Monotonic request id chosen by the sender. The worker echoes
    /// it in the matching [`Response`].
    pub id: u64,
    #[serde(flatten)]
    pub op: RequestOp,
}

/// The operation a [`Request`] performs.
///
/// Serialized with `op` as the discriminator (`{"op": "eval", "tcl":
/// "..."}`), matching the project plan's spec.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestOp {
    /// Evaluate a TCL command in the worker's interpreter and return
    /// the result as a string.
    Eval { tcl: String },
    /// Cleanly shut the worker down. Issued by [`crate::EdaBackend::shutdown`].
    Shutdown,
}

/// A response from the worker for a single request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub result: ResponseResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseResult {
    Ok {
        ok: OkMarker,
        #[serde(default)]
        result: serde_json::Value,
    },
    Err {
        ok: ErrMarker,
        error: ErrorPayload,
    },
}

/// Streaming notification emitted by the worker between request and
/// response. `puts` writes from inside an eval are forwarded as these
/// so callers can show output live rather than waiting for the eval
/// to complete (necessary for any long-running synthesis or
/// implementation command).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamMessage {
    /// Id of the in-flight request this stream chunk belongs to.
    pub id: u64,
    /// `"stdout"` today; reserved for `"stderr"` etc. later.
    pub stream: String,
    /// The chunk's bytes, including any trailing newline as written.
    pub data: String,
}

/// One wire-level message read from the worker. Either a streaming
/// chunk for an in-flight request, the request's final response, or
/// an unsolicited RPC call FROM the worker asking `vw` (Rust side) to
/// compute a value. Discriminated by structural inspection:
/// - stream chunks have a `stream` field,
/// - responses have `ok`,
/// - RPC calls have `rpc` set to `true` plus a `method` field.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum WireMessage {
    Stream(StreamMessage),
    Response(Response),
    Rpc(RpcCall),
}

/// An RPC call FROM the worker (shim) TO `vw`. The shim's htcl
/// library uses this to reach Rust-implemented externs like
/// `vw::workspace_root` and `vw::design_sources` — anything whose
/// answer lives on the tool side, not in Vivado.
///
/// Wire shape: `{"id": M, "rpc": true, "method": "...", "args": ...}`
/// The `rpc: true` marker keeps the untagged `WireMessage` union
/// unambiguous — plain responses have `ok`, streams have `stream`,
/// RPC calls have `rpc`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcCall {
    pub id: u64,
    /// Always `true` — used as an untagged-enum discriminator. See
    /// [`RpcMarker`] for the deserializer.
    pub rpc: RpcMarker,
    pub method: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Marker that always serializes to the literal `true`. Same
/// pattern as [`OkMarker`] — distinguishes RPC calls from Responses
/// in the untagged `WireMessage` union.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RpcMarker(#[serde(deserialize_with = "deserialize_true")] pub bool);

impl RpcMarker {
    pub const TRUE: RpcMarker = RpcMarker(true);
}

/// Marker that always serializes to the literal `true`. Lets us use
/// the same `ok` field as a discriminator without a custom serializer.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct OkMarker(#[serde(deserialize_with = "deserialize_true")] pub bool);

impl OkMarker {
    pub const TRUE: OkMarker = OkMarker(true);
}

fn deserialize_true<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<bool, D::Error> {
    let v = bool::deserialize(de)?;
    if v {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("expected `true`"))
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ErrMarker(#[serde(deserialize_with = "deserialize_false")] pub bool);

impl ErrMarker {
    pub const FALSE: ErrMarker = ErrMarker(false);
}

fn deserialize_false<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<bool, D::Error> {
    let v = bool::deserialize(de)?;
    if !v {
        Ok(false)
    } else {
        Err(serde::de::Error::custom("expected `false`"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: ResponseResult::Ok {
                ok: OkMarker::TRUE,
                result,
            },
        }
    }

    pub fn err(id: u64, error: ErrorPayload) -> Self {
        Self {
            id,
            result: ResponseResult::Err {
                ok: ErrMarker::FALSE,
                error,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_eval_request() {
        let req = Request {
            id: 1,
            op: RequestOp::Eval {
                tcl: "puts hi".into(),
            },
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"eval\""));
        assert!(s.contains("\"tcl\":\"puts hi\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        match back.op {
            RequestOp::Eval { tcl } => assert_eq!(tcl, "puts hi"),
            _ => panic!(),
        }
    }

    #[test]
    fn round_trip_ok_response() {
        let r = Response::ok(7, serde_json::json!("hi"));
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back.result {
            ResponseResult::Ok { result, .. } => {
                assert_eq!(result, serde_json::json!("hi"))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn round_trip_err_response() {
        let r = Response::err(
            8,
            ErrorPayload {
                message: "boom".into(),
                code: Some("E1".into()),
                info: None,
            },
        );
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back.result {
            ResponseResult::Err { error, .. } => {
                assert_eq!(error.message, "boom");
                assert_eq!(error.code.as_deref(), Some("E1"));
            }
            _ => panic!(),
        }
    }
}
