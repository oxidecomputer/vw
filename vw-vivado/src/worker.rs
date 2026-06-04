// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Vivado worker process: spawn under a PTY, accept the shim's
//! loopback connection, drive the request/response loop.
//!
//! Vivado is spawned with its stdin/stdout/stderr attached to a
//! pseudo-terminal slave (via [`portable_pty`]). The PTY matters:
//! when stdout is a pipe, glibc puts it in full-block-buffering mode
//! and Vivado's banner / source-echo / info messages don't appear
//! until ~4 KB accumulates, which kills the `--verbose` UX. With a
//! PTY Vivado sees a TTY on stdout and switches to line buffering,
//! so output streams as it's produced. `portable_pty` works on Linux
//! and macOS via Unix PTYs, and on Windows via ConPTY.

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use portable_pty::{
    native_pty_system, Child as PtyChild, CommandBuilder, MasterPty, PtySize,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tracing::{debug, warn};
use vw_eda::{
    BackendError, EdaBackend, EvalOutput, Request, RequestOp, Response,
    ResponseResult, WireMessage,
};

/// Embedded shim TCL. Written to a temp file at worker startup and
/// passed to `vivado -source`.
const SHIM_TCL: &str = include_str!("../shim/vivado-shim.tcl");

/// How long to wait for the shim to connect back to our loopback
/// listener. Vivado's startup takes most of this on a cold cache; on
/// a warm cache it's a few seconds.
const SHIM_CONNECT_TIMEOUT: Duration = Duration::from_secs(180);

/// Sink type for streamed stdout chunks. Called from the response
/// reader as each `{"stream":"stdout"}` notification arrives.
pub type StdoutSink = Box<dyn FnMut(&str) + Send>;

/// Spawn-time configuration for [`VivadoBackend`].
#[derive(Clone, Debug, Default)]
pub struct VivadoConfig {
    /// Override the `vivado` executable path. If `None`, resolution
    /// order is `$VW_VIVADO`, then a `vivado` lookup on `$PATH`.
    pub vivado: Option<PathBuf>,
    /// Working directory for the spawned process. If `None`, a
    /// scratch tempdir is created so Vivado's incidental files don't
    /// litter the user's cwd.
    pub working_dir: Option<PathBuf>,
    /// When `true`, forward Vivado's PTY output (banner, source-echo,
    /// info messages) to vw's stderr as it's produced. When `false`
    /// (default), the bytes are read and discarded so they don't
    /// pollute either of vw's output streams. User TCL `puts` is
    /// always captured per-eval via the shim and streamed in the
    /// protocol, independent of this setting.
    pub verbose: bool,
}

/// Vivado [`EdaBackend`] implementation.
pub struct VivadoBackend {
    child: Option<Box<dyn PtyChild + Send + Sync>>,
    /// Master end of the PTY. Kept alive so the slave (Vivado) doesn't
    /// receive EOF on its stdin.
    _master: Box<dyn MasterPty + Send>,
    proto_read: BufReader<OwnedReadHalf>,
    proto_write: OwnedWriteHalf,
    next_id: AtomicU64,
    stdout_pump: Option<std::thread::JoinHandle<()>>,
    stdout_sink: Option<StdoutSink>,
    _shim_dir: TempDir,
    _scratch_dir: Option<TempDir>,
}

impl VivadoBackend {
    /// Spawn a Vivado worker under a PTY, wait for the shim to
    /// connect back on our loopback listener, and return once we're
    /// ready to accept [`EdaBackend::eval`] calls.
    pub async fn spawn(config: VivadoConfig) -> Result<Self, BackendError> {
        let vivado_path = resolve_vivado(&config)?;

        let shim_dir = tempfile::Builder::new()
            .prefix("vw-vivado-shim-")
            .tempdir()
            .map_err(BackendError::Io)?;
        let shim_path = shim_dir.path().join("vivado-shim.tcl");
        tokio::fs::write(&shim_path, SHIM_TCL)
            .await
            .map_err(BackendError::Io)?;

        let (cwd, scratch_dir) = match &config.working_dir {
            Some(dir) => (dir.clone(), None),
            None => {
                let tmp = tempfile::Builder::new()
                    .prefix("vw-vivado-cwd-")
                    .tempdir()
                    .map_err(BackendError::Io)?;
                (tmp.path().to_path_buf(), Some(tmp))
            }
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(BackendError::Io)?;
        let local_addr = listener.local_addr().map_err(BackendError::Io)?;
        debug!(?vivado_path, ?shim_path, ?cwd, %local_addr, "spawning vivado worker");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| {
                BackendError::Worker(format!("openpty failed: {e}"))
            })?;

        // `-mode tcl` keeps Vivado alive as a long-running TCL
        // interpreter; once the shim's socket loop takes over, Vivado
        // never returns to its interactive prompt.
        let mut cmd = CommandBuilder::new(&vivado_path);
        cmd.arg("-mode");
        cmd.arg("tcl");
        cmd.arg("-nojournal");
        cmd.arg("-nolog");
        cmd.arg("-source");
        cmd.arg(&shim_path);
        cmd.env("VW_PROTOCOL_ADDR", local_addr.to_string());
        cmd.cwd(&cwd);

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            BackendError::Worker(format!(
                "failed to spawn vivado at {}: {}",
                vivado_path.display(),
                e
            ))
        })?;
        // Release our handle to the slave so the master sees EOF
        // when the child exits.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| {
            BackendError::Worker(format!("pty reader clone failed: {e}"))
        })?;
        let stdout_pump = spawn_stdout_pump(reader, config.verbose);

        // Wait for the shim to connect.
        let accept_result =
            tokio::time::timeout(SHIM_CONNECT_TIMEOUT, listener.accept()).await;
        let stream = match accept_result {
            Ok(Ok((stream, _peer))) => stream,
            Ok(Err(e)) => return Err(BackendError::Io(e)),
            Err(_) => {
                return Err(BackendError::Worker(
                    "timed out waiting for shim to connect".into(),
                ));
            }
        };
        stream.set_nodelay(true).map_err(BackendError::Io)?;
        debug!("shim connected");

        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            child: Some(child),
            _master: pair.master,
            proto_read: BufReader::new(read_half),
            proto_write: write_half,
            next_id: AtomicU64::new(1),
            stdout_pump: Some(stdout_pump),
            stdout_sink: None,
            _shim_dir: shim_dir,
            _scratch_dir: scratch_dir,
        })
    }

    /// Install a sink that's called per streaming stdout chunk as
    /// `puts` output is produced during eval. With a sink set, chunks
    /// are NOT also accumulated into [`EvalOutput::stdout`] — the
    /// sink owns the data, and the caller is expected to display
    /// or persist it directly.
    pub fn set_stdout_sink<F>(&mut self, sink: F)
    where
        F: FnMut(&str) + Send + 'static,
    {
        self.stdout_sink = Some(Box::new(sink));
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn write_request(
        &mut self,
        req: &Request,
    ) -> Result<(), BackendError> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.proto_write.write_all(line.as_bytes()).await?;
        self.proto_write.flush().await?;
        Ok(())
    }

    /// Read messages until we get the response that matches
    /// `expected_id`. Stream notifications for the same id are routed
    /// to [`Self::stdout_sink`] if set, or accumulated into the
    /// returned `String` if not.
    async fn read_response_for(
        &mut self,
        expected_id: u64,
    ) -> Result<(Response, String), BackendError> {
        let mut accumulated = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .proto_read
                .read_line(&mut line)
                .await
                .map_err(BackendError::Io)?;
            if n == 0 {
                return Err(BackendError::Worker(
                    "vivado shim closed protocol socket".into(),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: WireMessage =
                serde_json::from_str(trimmed).map_err(|e| {
                    BackendError::Worker(format!(
                        "malformed message from shim: {e}; payload={trimmed}"
                    ))
                })?;
            match msg {
                WireMessage::Stream(s) if s.id == expected_id => {
                    if let Some(sink) = self.stdout_sink.as_mut() {
                        sink(&s.data);
                    } else {
                        accumulated.push_str(&s.data);
                    }
                }
                WireMessage::Stream(s) => {
                    warn!(
                        got = s.id,
                        expected = expected_id,
                        "stream id mismatch; discarding"
                    );
                }
                WireMessage::Response(r) if r.id == expected_id => {
                    return Ok((r, accumulated));
                }
                WireMessage::Response(r) => {
                    warn!(
                        got = r.id,
                        expected = expected_id,
                        "response id mismatch; discarding"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl EdaBackend for VivadoBackend {
    fn name(&self) -> &str {
        "vivado"
    }

    async fn eval(&mut self, tcl: &str) -> Result<EvalOutput, BackendError> {
        let id = self.alloc_id();
        let req = Request {
            id,
            op: RequestOp::Eval { tcl: tcl.into() },
        };
        self.write_request(&req).await?;
        let (resp, stdout) = self.read_response_for(id).await?;
        match resp.result {
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
        if request.id == 0 {
            request.id = self.alloc_id();
        }
        let id = request.id;
        self.write_request(&request).await?;
        let (resp, _stdout) = self.read_response_for(id).await?;
        Ok(resp)
    }

    async fn shutdown(&mut self) -> Result<(), BackendError> {
        if self.child.is_none() {
            return Ok(());
        }
        let id = self.alloc_id();
        let req = Request {
            id,
            op: RequestOp::Shutdown,
        };
        let _ = self.write_request(&req).await;
        let _ = self.read_response_for(id).await;
        if let Some(mut child) = self.child.take() {
            // Vivado's tear-down is slow; bound it.
            let waited = tokio::task::spawn_blocking(move || {
                let deadline =
                    std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => return Ok(status),
                        Ok(None) => {
                            if std::time::Instant::now() >= deadline {
                                let _ = child.kill();
                                return child.wait();
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
            .await;
            match waited {
                Ok(Ok(status)) => debug!(?status, "vivado exited"),
                Ok(Err(e)) => return Err(BackendError::Io(e)),
                Err(e) => warn!(?e, "vivado wait join error"),
            }
        }
        if let Some(handle) = self.stdout_pump.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for VivadoBackend {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        // The pump thread will exit on its own when the PTY master is
        // dropped and the read returns EOF.
    }
}

/// Pump Vivado's PTY output in the background.
///
/// We always *read* the bytes — otherwise the PTY backpressures and
/// Vivado eventually blocks. Whether we *forward* depends on
/// `verbose`: when `true` they go to vw's stderr live; when `false`
/// they're read and discarded so they don't pollute either output
/// stream.
fn spawn_stdout_pump(
    mut reader: Box<dyn Read + Send>,
    verbose: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if verbose {
                        use std::io::Write;
                        let _ = std::io::stderr().write_all(&buf[..n]);
                        let _ = std::io::stderr().flush();
                    }
                }
                Err(e) => {
                    debug!(error = %e, "pty read error");
                    break;
                }
            }
        }
    })
}

fn resolve_vivado(config: &VivadoConfig) -> Result<PathBuf, BackendError> {
    if let Some(path) = &config.vivado {
        return Ok(path.clone());
    }
    if let Ok(env) = std::env::var("VW_VIVADO") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("vivado");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(BackendError::Worker(
        "could not find `vivado` on PATH; set $VW_VIVADO or pass \
         VivadoConfig::vivado"
            .into(),
    ))
}
