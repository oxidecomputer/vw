// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Shim-initiated RPC — how htcl library procs reach into `vw`
//! (Rust) for values Vivado can't provide on its own.
//!
//! Direction is the mirror of the eval loop: `vw` normally sends
//! requests and the shim answers. Here, the shim sends an
//! [`RpcCall`](vw_eda::protocol::RpcCall) and `vw` answers. The
//! answer is written back as a plain [`Response`](vw_eda::protocol::Response)
//! keyed on the RPC call's id, so no new response type is needed.
//!
//! ## Shape of a handler
//!
//! An [`RpcHandler`] is a `Send + Sync` object with a single async
//! method that looks up a `method` string and computes a JSON
//! payload. Callers construct a concrete handler with whatever
//! Rust-side state they need to serve (workspace root, design
//! source list, dep graph, …) and hand it to
//! [`VivadoConfig::rpc_handler`](crate::VivadoConfig::rpc_handler).
//!
//! Handler methods are named as flat strings (`"workspace_root"`).
//! Namespaces on the htcl side (`vw::workspace_root`) are a
//! call-site convention, not a wire concern.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use vw_eda::StreamKind;

/// What a handler gets from the worker whose shim is calling it.
///
/// Most handlers just compute an answer. The ones that do real work while
/// the caller waits — building IP checkpoints in worker Vivados — need to be
/// seen doing it, and need to stop when the caller is interrupted.
#[derive(Clone)]
pub struct RpcContext {
    /// Output for the session, as though the calling worker had produced
    /// it: the worker feeds it to its own stream sink while the call is
    /// outstanding, so it is classified, counted and rendered with
    /// everything else (critical warnings included).
    pub output: UnboundedSender<(StreamKind, String)>,
    /// The calling worker's process. Interrupting it
    /// ([`crate::interrupt_process_group`]) interrupts any worker a
    /// handler registers under it too.
    pub caller_pid: Option<u32>,
}

impl RpcContext {
    /// A context whose output goes nowhere, for calls made outside an eval.
    pub fn detached() -> Self {
        let (output, _) = tokio::sync::mpsc::unbounded_channel();
        Self {
            output,
            caller_pid: None,
        }
    }

    /// Send `text` to the session as a `kind` message.
    pub fn emit(&self, kind: StreamKind, text: impl Into<String>) {
        let _ = self.output.send((kind, text.into()));
    }
}

/// Trait implemented by anything that can service RPC calls from
/// the shim. Registered via
/// [`VivadoConfig::rpc_handler`](crate::VivadoConfig::rpc_handler).
///
/// Unknown methods should return `Err("unknown method: …".into())` —
/// the shim surfaces that verbatim to the caller.
#[async_trait]
pub trait RpcHandler: Send + Sync {
    async fn call(
        &self,
        method: &str,
        args: Value,
        ctx: RpcContext,
    ) -> Result<Value, String>;
}

/// Convenience impl so callers can wrap a closure without hand-
/// rolling a struct. `Arc<F>` because the trait bound is
/// `Send + Sync + 'static` and we hold it in an `Arc<dyn RpcHandler>`.
#[async_trait]
impl<F, Fut> RpcHandler for FnHandler<F>
where
    F: Fn(String, Value, RpcContext) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, String>> + Send,
{
    async fn call(
        &self,
        method: &str,
        args: Value,
        ctx: RpcContext,
    ) -> Result<Value, String> {
        (self.f)(method.to_string(), args, ctx).await
    }
}

/// Type-erased wrapper for closure-based [`RpcHandler`]
/// construction. Use [`FnHandler::new`] rather than constructing
/// directly.
pub struct FnHandler<F> {
    f: F,
}

impl<F> FnHandler<F> {
    #[allow(clippy::new_ret_no_self)] // returns a trait-object Arc, not Self
    pub fn new<Fut>(f: F) -> Arc<dyn RpcHandler>
    where
        F: Fn(String, Value, RpcContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, String>>
            + Send
            + 'static,
    {
        Arc::new(FnHandler { f })
    }
}
