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

use std::fs::File;
use std::io::{BufWriter, Read, Write};
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

/// Tag attached to each chunk a [`StdoutSink`] receives, so the
/// caller can route it to the right UI lane. The shim's
/// `puts`-interception path always produces [`StreamKind::Stdout`]
/// — user TCL has no way to "label" a write. The PTY-line filter
/// classifies Vivado's standard message format
/// (`ERROR:`/`WARNING:`/`CRITICAL WARNING:`/`INFO:`) into the
/// corresponding kind.
///
/// A consumer that doesn't care (e.g. `vw run` capturing for
/// stdout pass-through) can ignore the kind and treat every chunk
/// identically; the REPL uses it to colour error/warning lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    /// User TCL `puts` output, or any other chunk we don't have a
    /// reason to label otherwise. Default.
    Stdout,
    /// Vivado `INFO:` line — usually low-importance chatter from
    /// the message system.
    Info,
    /// Vivado `WARNING:` / `CRITICAL WARNING:` line.
    Warning,
    /// Vivado `ERROR:` line. Distinct from the final
    /// [`BackendError::Tcl`] returned by `eval` — these are
    /// emitted *during* an eval and the final error often refers
    /// back to them ("failed due to earlier errors").
    Error,
}

/// Sink for streamed output during an eval. Called once per chunk
/// the worker observes — from the shim's `puts` interception (Tcl
/// user output) or from the PTY-line filter (Vivado's own message
/// system). The [`StreamKind`] tags the chunk so the caller can
/// route warnings and errors to a more attention-grabbing UI
/// surface than ordinary stdout.
pub type StdoutSink = Box<dyn FnMut(StreamKind, &str) + Send>;

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
    /// info messages) as it's produced. When `false` (default), the
    /// bytes are read and discarded so they don't pollute either of
    /// vw's output streams. User TCL `puts` is always captured per-
    /// eval via the shim and streamed in the protocol, independent
    /// of this setting.
    ///
    /// Where the verbose output goes depends on [`verbose_log`](Self::verbose_log):
    /// when set, lines stream into that file; when unset, they go
    /// to vw's stderr.
    pub verbose: bool,
    /// When `true`, every Vivado-formatted message
    /// (`INFO:`/`WARNING:`/`ERROR:`/`CRITICAL WARNING:`) gets the
    /// Tcl call stack appended as `at <file>:<line> in ::proc`
    /// continuation lines. When `false` (default), stacks are
    /// attached only to WARNINGs and ERRORs — INFO messages render
    /// as a single line. INFOs are noisy under heavy Vivado
    /// activity (CIPS customization can emit dozens per call) and
    /// the stack adds little signal compared to the message text;
    /// power users diagnosing why a particular INFO fires can flip
    /// this on with `--info-with-stack`.
    pub info_with_stack: bool,
    /// Optional path to a log file for verbose output. When set,
    /// supersedes the default stderr destination — necessary for
    /// the REPL, which owns the terminal in alternate-screen mode
    /// and would corrupt the TUI rendering if anyone wrote raw
    /// bytes to stderr mid-frame. The file is created (or
    /// truncated) at spawn time and flushed per-line so it's safe
    /// to `tail -f` from another terminal.
    pub verbose_log: Option<PathBuf>,
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
    /// Lines the PTY pump has read from Vivado's process stdout, in
    /// arrival order. Drained during eval so Vivado's own message
    /// system (ERROR/WARNING/CRITICAL WARNING/INFO) reaches the
    /// stdout sink alongside user `puts` output — otherwise the
    /// "earlier errors" Vivado refers to when failing a command are
    /// invisible to the caller.
    pty_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    /// Mirrors [`VivadoConfig::verbose`]. When true, PTY lines that
    /// don't classify (banner, source-echo, idle chatter) are
    /// surfaced — to [`verbose_log`](Self::verbose_log) if set,
    /// otherwise to vw's stderr. Classified lines always route
    /// through the message filter regardless of verbose.
    verbose: bool,
    /// Optional log file the verbose firehose streams into. The
    /// REPL uses this so verbose output doesn't blow through its
    /// TUI alternate screen by hitting stderr.
    verbose_log: Option<BufWriter<File>>,
    /// Off by default. When true (set via the
    /// `VW_TRACE_MESSAGE_SOURCES` env var at spawn time), emit a
    /// gray `[vw-pty]` Info line before every classified PTY
    /// chunk so the caller can see which path produced it.
    /// Useful for diagnosing "where is this warning coming from?"
    /// questions; noisy enough that it shouldn't be on by
    /// default.
    trace_message_sources: bool,
    /// Brief-buffer classifier for multi-line PTY warnings. See
    /// [`PtyClassifier`] for the merging semantics.
    pty_classifier: PtyClassifier,
    /// Stack of ready-to-use frame sets sent by the shim via
    /// `__VW_CTX_*` PTY markers. Each entry is a set of frames
    /// captured at a nesting level: outer entries are older, the
    /// top is the innermost currently-executing wrap (a user proc's
    /// body, or a wrapped `set_property` / `generate_netlist_ip`
    /// call). When a Warning/Error chunk lands without its own
    /// trace, the top entry's frames get appended as `\n  at
    /// <frame>` lines — that's what lets the REPL show "this
    /// IP_Flow warning came from configure_cips →
    /// create_versal_cips → set_property" even though Vivado's C++
    /// never went through our Tcl stack capture. Nesting matters
    /// because user procs call each other and each level's marker
    /// wraps the next; a single active slot would clobber outer
    /// context when the inner call returned mid-warning-emission.
    pty_context_stack: Vec<Vec<String>>,
    /// Frames currently being assembled between
    /// `__VW_CTX_BEGIN__` and `__VW_CTX_READY__`. Pushed onto
    /// `pty_context_stack` atomically on READY so a partial
    /// marker stream can't leak half-formed traces into emitted
    /// warnings. Scalar (not per-nesting-level) because BEGIN and
    /// READY are emitted synchronously in a single Tcl step — the
    /// shim never emits a nested BEGIN before its outer READY.
    building_pty_context: Vec<String>,
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
        // Read by the shim's `is_vivado_message` / stack-attach
        // logic to decide whether INFO-level messages get stacks
        // attached. WARNING / ERROR / CRITICAL always do.
        cmd.env(
            "VW_INFO_WITH_STACK",
            if config.info_with_stack { "1" } else { "0" },
        );
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
        let (pty_tx, pty_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let stdout_pump = spawn_stdout_pump(reader, pty_tx);

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
        let trace_message_sources = std::env::var("VW_TRACE_MESSAGE_SOURCES")
            .map(|v| {
                let v = v.trim();
                !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
            })
            .unwrap_or(false);

        let verbose_log = config
            .verbose_log
            .as_ref()
            .map(|p| File::create(p).map(BufWriter::new))
            .transpose()
            .map_err(BackendError::Io)?;

        Ok(Self {
            child: Some(child),
            _master: pair.master,
            proto_read: BufReader::new(read_half),
            proto_write: write_half,
            next_id: AtomicU64::new(1),
            stdout_pump: Some(stdout_pump),
            stdout_sink: None,
            pty_rx,
            verbose: config.verbose,
            verbose_log,
            trace_message_sources,
            pty_classifier: PtyClassifier::new(PTY_CONTINUATION_WINDOW),
            pty_context_stack: Vec::new(),
            building_pty_context: Vec::new(),
            _shim_dir: shim_dir,
            _scratch_dir: scratch_dir,
        })
    }

    /// Install a sink that's called per streaming chunk as output
    /// is produced during eval. With a sink set, chunks are NOT
    /// also accumulated into [`EvalOutput::stdout`] — the sink owns
    /// the data, and the caller is expected to display or persist
    /// it directly. The [`StreamKind`] argument tags each chunk
    /// (user `puts` vs. Vivado's WARNING/ERROR/INFO messages) so
    /// the caller can route the chunk to the appropriate UI lane.
    pub fn set_stdout_sink<F>(&mut self, sink: F)
    where
        F: FnMut(StreamKind, &str) + Send + 'static,
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
    ///
    /// While waiting, the worker also drains the PTY channel — so
    /// Vivado's own `send_msg_id` output (ERROR/WARNING/CRITICAL
    /// WARNING/INFO lines printed to the process stdout, not through
    /// the shim's `puts` interception) reaches the same sink and
    /// gets attributed to the in-flight eval. Without this, the
    /// "earlier errors" Vivado refers to when a command fails are
    /// invisible to the caller.
    async fn read_response_for(
        &mut self,
        expected_id: u64,
    ) -> Result<(Response, String), BackendError> {
        let mut accumulated = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            // Race the protocol socket against the PTY channel. The
            // protocol path eventually terminates the loop (a
            // Response arrives); PTY lines are forwarded as
            // best-effort context until then.
            tokio::select! {
                biased;
                pty = self.pty_rx.recv() => {
                    let Some(pty_line) = pty else {
                        // Pump exited: Vivado's PTY closed.
                        // Stop trying to drain it but keep waiting
                        // for a Response on the protocol socket —
                        // most teardown sequences send the
                        // shutdown ack before the PTY EOFs.
                        continue;
                    };
                    self.handle_pty_line_during_eval(
                        &pty_line,
                        &mut accumulated,
                    );
                    continue;
                }
                read = self.proto_read.read_line(&mut line) => {
                    let n = read.map_err(BackendError::Io)?;
                    if n == 0 {
                        return Err(BackendError::Worker(
                            "vivado shim closed protocol socket".into(),
                        ));
                    }
                }
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
                        // Default: shim-stream chunks are user
                        // `puts` output (StreamKind::Stdout). But
                        // our send_msg_id override in the shim
                        // also emits via this path — those chunks
                        // start with a Vivado-standard severity
                        // prefix (`WARNING:`/`ERROR:`/etc.) which
                        // we re-use the PTY-line classifier to
                        // detect.
                        let kind = classify_chunk_for_sink(&s.data);
                        // Provenance marker (opt-in via
                        // VW_TRACE_MESSAGE_SOURCES), only on
                        // classified (non-Stdout) chunks — plain
                        // user output doesn't benefit from a
                        // "came via the shim" tag, but a warning
                        // does so the user can tell it from a
                        // PTY-routed one.
                        if self.trace_message_sources
                            && kind != StreamKind::Stdout
                        {
                            sink(
                                StreamKind::Info,
                                &format!(
                                    "[vw-shim-stream] \
                                     classified-as={kind:?}\n"
                                ),
                            );
                        }
                        sink(kind, &s.data);
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
                    // Force-flush any buffered PTY message — a
                    // classified line that arrived right before
                    // the Vivado response would otherwise linger
                    // in the classifier until the next eval's
                    // drain. Flushing here means the user always
                    // sees every classified message attributable
                    // to the eval before the eval's result.
                    if let Some((kind, text)) = self.pty_classifier.flush() {
                        self.emit_pty_chunk(kind, &text, &mut accumulated);
                    }
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

    /// Filter a PTY line received during an in-flight eval. Lines
    /// matching Vivado's standard message format are forwarded to
    /// the stdout sink (or accumulated when there's no sink);
    /// everything else (banner, source-echo, idle chatter) is
    /// dropped — or stderr-mirrored when `verbose` is set, so a
    /// user diagnosing a flaky eval can still get the full firehose.
    fn handle_pty_line_during_eval(
        &mut self,
        line: &str,
        accumulated: &mut String,
    ) {
        if self.consume_ctx_marker(line) {
            return;
        }
        let outcome =
            self.pty_classifier.handle(line, std::time::Instant::now());
        for (kind, text) in outcome.chunks {
            self.emit_pty_chunk(kind, &text, accumulated);
        }
        if !outcome.absorbed && self.verbose {
            self.write_verbose_line(line);
        }
    }

    /// Recognize one of the `__VW_CTX_*` lines the shim emits
    /// around wrapped commands and user proc bodies. Returns
    /// `true` if the line was a marker (and should be swallowed);
    /// `false` if it's a normal PTY line for the classifier.
    ///
    /// The marker protocol is stack-based: BEGIN opens a new
    /// entry, FRAME lines accumulate into it, READY seals it onto
    /// `pty_context_stack`, END pops the top entry. Nested proc
    /// calls produce nested BEGIN/END pairs, and the top of the
    /// stack — the innermost wrap in flight — is what tags any
    /// traceless warning/error that arrives while it's active.
    fn consume_ctx_marker(&mut self, line: &str) -> bool {
        let stripped = line.trim_end_matches(['\r', '\n']);
        match stripped {
            "__VW_CTX_BEGIN__" => {
                self.building_pty_context.clear();
                true
            }
            "__VW_CTX_READY__" => {
                let frames = std::mem::take(&mut self.building_pty_context);
                self.pty_context_stack.push(frames);
                true
            }
            "__VW_CTX_END__" => {
                self.pty_context_stack.pop();
                // Defensively clear building too — a stray FRAME
                // that arrived after END shouldn't leak into the
                // next window.
                self.building_pty_context.clear();
                true
            }
            _ => {
                if let Some(frame) = stripped.strip_prefix("__VW_CTX_FRAME__:")
                {
                    self.building_pty_context.push(frame.to_string());
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Drain whatever PTY lines have queued up between evals. Two
    /// classes of line show up here in practice:
    ///
    /// 1. **Shim startup logs** (`[vw-shim] ...`) — emitted by
    ///    Vivado-startup-time code before the user's first eval.
    ///    Routed via the sink as Info so the user can see
    ///    whether the send_msg_id override installed.
    /// 2. **Vivado messages emitted between evals** (e.g., delayed
    ///    `WARNING:` from a previous eval's async work, or
    ///    initialization messages fired by Vivado without any
    ///    eval in flight). Same deal — route to sink so the user
    ///    sees them in scrollback.
    ///
    /// Unclassified lines (banner, source-echo, the Vivado prompt)
    /// still drop on the floor — or stderr-mirror if `verbose`.
    /// Forwarding those would flood scrollback.
    fn drain_pty_between_evals(&mut self) {
        // Force-flush any pending PTY message from the previous
        // eval first — if the eval ended right after a classified
        // line and before any continuation could arrive, we want
        // it surfaced before whatever the drain finds.
        let mut sink_void = String::new();
        if let Some((kind, text)) = self.pty_classifier.flush() {
            self.emit_pty_chunk(kind, &text, &mut sink_void);
        }
        while let Ok(line) = self.pty_rx.try_recv() {
            if self.consume_ctx_marker(&line) {
                continue;
            }
            let outcome =
                self.pty_classifier.handle(&line, std::time::Instant::now());
            for (kind, text) in outcome.chunks {
                self.emit_pty_chunk(kind, &text, &mut sink_void);
            }
            if !outcome.absorbed && self.verbose {
                self.write_verbose_line(&line);
            }
        }
        // Flush again at end of drain — a classified line that
        // landed right before the drain stopped might still be
        // buffered. No new lines will arrive before the next
        // eval's read_response_for, so we'd rather surface this
        // now than wait.
        if let Some((kind, text)) = self.pty_classifier.flush() {
            self.emit_pty_chunk(kind, &text, &mut sink_void);
        }
    }

    /// Write one verbose-firehose line — to the log file when
    /// configured, otherwise to vw's stderr. Used for unclassified
    /// PTY lines we'd otherwise discard. Errors silently because
    /// dropping a verbose line shouldn't break the eval.
    fn write_verbose_line(&mut self, line: &str) {
        if let Some(w) = self.verbose_log.as_mut() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        } else {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    }

    /// Emit one classified PTY chunk: the optional gray provenance
    /// marker (when `trace_message_sources` is on) followed by the
    /// chunk itself. Used by both the in-eval and between-eval
    /// classification paths so the marker / sink-vs-accumulator
    /// rule lives in exactly one place.
    ///
    /// `[vw-*]` self-diagnostic chunks (shim install logs, future
    /// internal tracers) are suppressed unless trace is on. Most
    /// users don't care that the shim connected to a port and
    /// installed an override — that's housekeeping noise. When
    /// something goes wrong, set `VW_TRACE_MESSAGE_SOURCES=1` to
    /// surface both these chunks AND the per-message provenance
    /// markers.
    fn emit_pty_chunk(
        &mut self,
        kind: StreamKind,
        text: &str,
        accumulated: &mut String,
    ) {
        if !self.trace_message_sources && is_vw_log_chunk(text) {
            return;
        }
        // Tag warnings/errors that arrived without a trace with the
        // innermost active context (the top of `pty_context_stack`
        // — frames captured by the shim around the in-flight C++
        // call or user proc body). This is the path that resolves
        // "IP_Flow 19-7090" and friends — they go straight from
        // Vivado's C++ to the PTY, bypassing every Tcl-side
        // stack-capture hook.
        let tagged: String;
        let payload: &str = if let Some(frames) = self
            .pty_context_stack
            .last()
            .filter(|_| {
                matches!(kind, StreamKind::Warning | StreamKind::Error)
                    && !text.contains("\n  at ")
            })
            .filter(|f| !f.is_empty())
        {
            let trimmed = text.trim_end_matches('\n');
            let mut buf = String::with_capacity(text.len() + 80);
            buf.push_str(trimmed);
            for frame in frames {
                buf.push_str("\n  at ");
                buf.push_str(frame);
            }
            // Restore the trailing newline if the caller had one
            // — downstream chunk handling assumes line-terminated.
            if text.ends_with('\n') {
                buf.push('\n');
            }
            tagged = buf;
            &tagged
        } else {
            text
        };
        if let Some(sink) = self.stdout_sink.as_mut() {
            if self.trace_message_sources {
                sink(
                    StreamKind::Info,
                    &format!("[vw-pty] classified-as={kind:?}\n"),
                );
            }
            sink(kind, payload);
        } else {
            accumulated.push_str(payload);
        }
    }
}

/// True when the chunk's first non-whitespace content matches one
/// of our `[vw-*]` self-diagnostic prefixes (see [`VW_LOG_PREFIXES`]).
/// Used by [`VivadoBackend::emit_pty_chunk`] to suppress these
/// chunks when trace isn't enabled.
pub(crate) fn is_vw_log_chunk(text: &str) -> bool {
    let trimmed = text.trim_start();
    VW_LOG_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

#[async_trait]
impl EdaBackend for VivadoBackend {
    fn name(&self) -> &str {
        "vivado"
    }

    async fn eval(&mut self, tcl: &str) -> Result<EvalOutput, BackendError> {
        self.drain_pty_between_evals();
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
        self.drain_pty_between_evals();
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
/// Pump Vivado's process stdout into the worker as a stream of
/// newline-split lines. Runs on a blocking std thread because
/// `portable_pty` only exposes a synchronous `Read`.
///
/// We always *read* the bytes — otherwise the PTY backpressures and
/// Vivado eventually blocks. Splitting on `\n` here (rather than
/// shipping raw chunks) keeps line semantics consistent for the
/// downstream message-line filter, which works one line at a time.
/// `\r` is stripped — Vivado's PTY output sometimes contains CRLF.
///
/// The thread exits when the PTY closes (Vivado died) or the
/// receiver is dropped (the worker shut down).
fn spawn_stdout_pump(
    mut reader: Box<dyn Read + Send>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut line = String::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            let send = std::mem::take(&mut line);
                            if tx.send(send).is_err() {
                                return;
                            }
                        } else if b != b'\r' {
                            // Best-effort UTF-8 — replace bytes that
                            // aren't valid mid-line. Vivado's stdout
                            // is ASCII in practice.
                            line.push(b as char);
                        }
                    }
                }
                Err(e) => {
                    debug!(error = %e, "pty read error");
                    break;
                }
            }
        }
        // Flush any partial trailing line so EOF doesn't swallow the
        // last unterminated message.
        if !line.is_empty() {
            let _ = tx.send(line);
        }
    })
}

/// Classify `line` as a Vivado standard-format message and
/// translate the prefix into the [`StreamKind`] the sink should
/// receive. Returns `None` for lines that don't match the
/// `common::send_msg_id` prefix set — those are banner /
/// source-echo / idle chatter and don't reach the sink.
///
/// Conservative on purpose: a false-negative just means a useful
/// line is dropped (recoverable with `verbose=true` for users who
/// need the full firehose), but a false-positive injects banner /
/// source-echo noise into every eval's output, which would degrade
/// the REPL experience for everyone. Leading whitespace is allowed
/// because Vivado occasionally indents within scripted blocks.
/// Classify a multi-line shim-stream chunk for sink routing. The
/// chunk's first line determines its kind: a Vivado-standard
/// severity prefix routes to the matching [`StreamKind`];
/// anything else falls back to [`StreamKind::Stdout`] (the
/// chunk is treated as user `puts` output).
///
/// Continuation lines (the `at file:line in proc` frames the
/// send_msg_id override appends) inherit the first-line kind by
/// virtue of being part of the same chunk. The downstream
/// renderer treats each chunk as one scrollback entry.
pub(crate) fn classify_chunk_for_sink(chunk: &str) -> StreamKind {
    let first = chunk.lines().next().unwrap_or("");
    classify_vivado_message_line(first).unwrap_or(StreamKind::Stdout)
}

pub(crate) fn classify_vivado_message_line(line: &str) -> Option<StreamKind> {
    let l = line.trim_start();
    // `CRITICAL WARNING:` must be checked BEFORE `WARNING:` because
    // the latter is a prefix of the former when leading whitespace
    // is trimmed.
    if l.starts_with("ERROR:") {
        Some(StreamKind::Error)
    } else if l.starts_with("CRITICAL WARNING:") || l.starts_with("WARNING:") {
        Some(StreamKind::Warning)
    } else if l.starts_with("INFO:") {
        Some(StreamKind::Info)
    } else if VW_LOG_PREFIXES.iter().any(|p| l.starts_with(p)) {
        // Our own diagnostics — see [`VW_LOG_PREFIXES`] for the
        // canonical list. All route as Info: they're gray "where
        // did this come from" markers, not warnings. The
        // allowlist (rather than a generic `starts_with("[vw-")`)
        // prevents a user's `puts "[vw-mystuff] hi"` from getting
        // accidentally absorbed.
        Some(StreamKind::Info)
    } else {
        None
    }
}

/// Allowlist of prefixes our shim and worker emit for self-
/// diagnostics. Any line starting with one of these classifies
/// as [`StreamKind::Info`].
pub(crate) const VW_LOG_PREFIXES: &[&str] =
    &["[vw-shim]", "[vw-pty]", "[vw-shim-stream]"];

/// Window during which a classified PTY line will absorb a
/// following unclassified line as a continuation. Vivado
/// occasionally emits multi-line messages where the severity
/// prefix only sits on the first line; treating an
/// immediately-following unclassified line as part of the same
/// message renders the warning as one scrollback entry instead
/// of two.
///
/// 20ms is well above the inter-line latency our PTY pump sees
/// for a single Vivado write (which is sub-millisecond) but
/// well below human reaction time, so a real follow-up message
/// from a *different* call site can't be misattributed.
pub(crate) const PTY_CONTINUATION_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(20);

/// Per-message buffer the worker uses to merge a multi-line PTY
/// warning into one chunk. See [`PtyClassifier`] for the merge
/// semantics.
#[derive(Debug, Clone)]
struct PendingPtyMessage {
    kind: StreamKind,
    text: String,
    arrived_at: std::time::Instant,
}

/// Outcome of feeding one PTY line through [`PtyClassifier`].
#[derive(Debug, Default)]
pub(crate) struct ClassifyOutcome {
    /// Chunks ready for the sink (or accumulator) in arrival
    /// order. At most one *new* classified chunk per call; an
    /// additional preceding entry appears only when this call
    /// flushed a previously-pending message (either because a
    /// new classified line arrived or the window expired).
    pub chunks: Vec<(StreamKind, String)>,
    /// True when the classifier took responsibility for the
    /// input line (stored it as pending, or appended it to a
    /// pending message). False when the line was an unclassified
    /// non-continuation — caller may stderr-mirror it.
    pub absorbed: bool,
}

/// Brief-buffer classifier for PTY lines. Holds one classified
/// message at a time; an unclassified line arriving within
/// [`PTY_CONTINUATION_WINDOW`] gets folded into it, so a multi-
/// line Vivado warning whose first line carries the severity
/// prefix renders as a single chunk (and thus a single scrollback
/// entry on the App side).
///
/// Pure / time-injected so it's unit-testable without setting up
/// a worker.
#[derive(Debug)]
pub(crate) struct PtyClassifier {
    pending: Option<PendingPtyMessage>,
    window: std::time::Duration,
}

impl PtyClassifier {
    pub fn new(window: std::time::Duration) -> Self {
        Self {
            pending: None,
            window,
        }
    }

    /// Feed one PTY line. `now` is when the line arrived (taken
    /// as a parameter so tests can drive the clock).
    pub fn handle(
        &mut self,
        line: &str,
        now: std::time::Instant,
    ) -> ClassifyOutcome {
        let mut out = ClassifyOutcome::default();
        if let Some(kind) = classify_vivado_message_line(line) {
            // A new classified line starts a new pending. Flush
            // whatever was pending first — same path as the
            // window-expired case below.
            if let Some(prev) = self.pending.take() {
                out.chunks
                    .push((prev.kind, with_trailing_newline(&prev.text)));
            }
            self.pending = Some(PendingPtyMessage {
                kind,
                text: line.to_string(),
                arrived_at: now,
            });
            out.absorbed = true;
            return out;
        }
        // Unclassified. Maybe a continuation of the current
        // pending warning/error? We only fold for Warning/Error
        // kinds because Info messages (`[vw-shim] ...` and
        // Vivado `INFO:`) are always single-line in practice —
        // and absorbing into them swallowed Vivado's source-echo
        // of our shim script (`# catch {...}`, `# while {1} {`,
        // etc.) which arrived inside the window during boot.
        // Restricting to Warning|Error covers the case we
        // actually care about (Vivado occasionally emits multi-
        // line WARNING/ERROR text with `\n` between the header
        // and a body) without the noise.
        if let Some(p) = self.pending.as_mut() {
            let merges =
                matches!(p.kind, StreamKind::Warning | StreamKind::Error);
            if merges && now.duration_since(p.arrived_at) < self.window {
                p.text.push('\n');
                p.text.push_str(line);
                // Refresh the arrival time so a chain of
                // continuation lines all qualifies, not just the
                // first.
                p.arrived_at = now;
                out.absorbed = true;
                return out;
            }
            // Either kind doesn't merge, or the window expired —
            // flush the pending. The current line itself is not
            // absorbed; caller may stderr-mirror it.
            let p = self.pending.take().unwrap();
            out.chunks.push((p.kind, with_trailing_newline(&p.text)));
        }
        out
    }

    /// Force-flush any pending message. Called at eval end so a
    /// message buffered right before the Vivado response doesn't
    /// linger unseen.
    pub fn flush(&mut self) -> Option<(StreamKind, String)> {
        self.pending
            .take()
            .map(|p| (p.kind, with_trailing_newline(&p.text)))
    }
}

fn with_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        classify_chunk_for_sink, classify_vivado_message_line, is_vw_log_chunk,
        PtyClassifier, StreamKind,
    };

    fn classifier(window_ms: u64) -> PtyClassifier {
        PtyClassifier::new(Duration::from_millis(window_ms))
    }

    #[test]
    fn classifier_emits_classified_line_only_on_flush() {
        // A classified line gets buffered, not emitted, until
        // either a new classified line arrives or flush() is
        // called. This is what enables the multi-line-message
        // merge that follows.
        let mut c = classifier(20);
        let t0 = Instant::now();
        let out = c.handle("WARNING: [X 1-1] hi", t0);
        assert!(out.chunks.is_empty(), "{:?}", out.chunks);
        assert!(out.absorbed);
        let flushed = c.flush().expect("pending must flush");
        assert_eq!(flushed.0, StreamKind::Warning);
        assert_eq!(flushed.1, "WARNING: [X 1-1] hi\n");
    }

    #[test]
    fn classifier_absorbs_unclassified_continuation_within_window() {
        let mut c = classifier(20);
        let t0 = Instant::now();
        let out = c.handle("WARNING: [X 1-1] header", t0);
        assert!(out.absorbed);
        let out = c.handle("second body line", t0 + Duration::from_millis(5));
        assert!(out.chunks.is_empty(), "{:?}", out.chunks);
        assert!(out.absorbed);
        // A third continuation works too (window refreshes on
        // each absorb).
        let out = c.handle("third line", t0 + Duration::from_millis(15));
        assert!(out.chunks.is_empty(), "{:?}", out.chunks);
        assert!(out.absorbed);
        let (kind, text) = c.flush().unwrap();
        assert_eq!(kind, StreamKind::Warning);
        assert_eq!(
            text,
            "WARNING: [X 1-1] header\nsecond body line\nthird line\n"
        );
    }

    #[test]
    fn classifier_flushes_pending_when_window_expires() {
        let mut c = classifier(20);
        let t0 = Instant::now();
        let out = c.handle("WARNING: [X 1-1] one", t0);
        assert!(out.absorbed);
        // Unclassified line arrives well past the window:
        // pending flushes, line itself is reported as
        // not-absorbed so the caller may stderr-mirror it.
        let out = c.handle("a", t0 + Duration::from_millis(50));
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(out.chunks[0].0, StreamKind::Warning);
        assert_eq!(out.chunks[0].1, "WARNING: [X 1-1] one\n");
        assert!(!out.absorbed);
        // Nothing further pending.
        assert!(c.flush().is_none());
    }

    #[test]
    fn classifier_flushes_previous_pending_on_new_classified() {
        // Two classified lines back-to-back: the first flushes
        // as soon as the second arrives, and the second becomes
        // the new pending.
        let mut c = classifier(20);
        let t0 = Instant::now();
        c.handle("WARNING: [X 1-1] one", t0);
        let out = c.handle("ERROR: [Y 1-1] two", t0 + Duration::from_millis(5));
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(out.chunks[0].0, StreamKind::Warning);
        assert_eq!(out.chunks[0].1, "WARNING: [X 1-1] one\n");
        assert!(out.absorbed);
        let (kind, text) = c.flush().unwrap();
        assert_eq!(kind, StreamKind::Error);
        assert_eq!(text, "ERROR: [Y 1-1] two\n");
    }

    #[test]
    fn vw_log_chunk_detection() {
        // Recognized chunks for emit-time suppression.
        assert!(is_vw_log_chunk(
            "[vw-shim] installed send_msg_id override\n"
        ));
        assert!(is_vw_log_chunk("[vw-pty] classified-as=Warning\n"));
        assert!(is_vw_log_chunk("  [vw-shim-stream] classified-as=Error\n"));
        // Real message content stays — never accidentally
        // suppress a Vivado warning that mentions our tag in its
        // body.
        assert!(!is_vw_log_chunk("WARNING: [Common 17-1] no\n"));
        assert!(!is_vw_log_chunk(
            "INFO: [Common 17-1] something about [vw-shim]\n"
        ));
        assert!(!is_vw_log_chunk(""));
    }

    #[test]
    fn classifier_info_kind_does_not_absorb_continuations() {
        // Regression guard: Info-kind pending (typically a
        // [vw-shim] log line or a Vivado INFO line) must NOT
        // absorb subsequent unclassified lines, because those
        // are almost always unrelated content arriving in the
        // same time window (Vivado source-echo during boot,
        // banner lines, etc.). Only Warning/Error kinds merge.
        let mut c = classifier(20);
        let t0 = Instant::now();
        c.handle("[vw-shim] installed send_msg_id override", t0);
        let out = c.handle(
            "# catch {::vw::install_send_msg_override}",
            t0 + Duration::from_millis(5),
        );
        // The pending Info flushed as its own chunk; the source-
        // echo line was NOT absorbed.
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(out.chunks[0].0, StreamKind::Info);
        assert_eq!(
            out.chunks[0].1,
            "[vw-shim] installed send_msg_id override\n"
        );
        assert!(!out.absorbed);
    }

    #[test]
    fn classifier_drops_unclassified_lines_when_no_pending() {
        let mut c = classifier(20);
        let out = c.handle("plain output, no prefix", Instant::now());
        assert!(out.chunks.is_empty());
        assert!(!out.absorbed);
        assert!(c.flush().is_none());
    }

    #[test]
    fn classifies_each_standard_prefix_to_its_stream_kind() {
        let cases = [
            (
                "ERROR: [Common 17-53] No open project. ...",
                StreamKind::Error,
            ),
            (
                "WARNING: [Coretcl 2-1184] no open project",
                StreamKind::Warning,
            ),
            (
                "CRITICAL WARNING: [Vivado 12-180] ...",
                // Critical warnings are still warnings as far as
                // the UI is concerned — orange, not red. The
                // `CRITICAL` prefix is preserved in the line
                // content for the user to see.
                StreamKind::Warning,
            ),
            (
                "INFO: [Vivado 12-3661] auto-pinning enabled",
                StreamKind::Info,
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                classify_vivado_message_line(line),
                Some(expected),
                "wrong classification for: {line}"
            );
        }
    }

    #[test]
    fn classifies_lines_with_leading_whitespace() {
        // Scripted blocks sometimes indent messages — still a
        // Vivado-formatted line, still useful to surface.
        assert_eq!(
            classify_vivado_message_line("  ERROR: [X 1-2] indented"),
            Some(StreamKind::Error)
        );
        assert_eq!(
            classify_vivado_message_line("\tWARNING: [X 1-2] tabbed"),
            Some(StreamKind::Warning)
        );
    }

    #[test]
    fn drops_banner_and_source_echo_and_chatter() {
        for line in [
            "",
            "Vivado v2024.2 (64-bit)",
            "SW Build 5095499 on Wed Nov 13 22:37:05 MST 2024",
            "Copyright 1986-2024 Xilinx, Inc.",
            "Vivado% ", // The interactive prompt
            "source /tmp/vw-vivado-shim/vivado-shim.tcl -notrace",
            "create_bd_design metroid",
            "errors not at start of line: ERROR: foo",
            // Has the substring but not at the start — looks like
            // shell or log output, not a message-system line.
            "[2024-01-01 12:00] INFO: forwarded by other tool",
        ] {
            assert_eq!(
                classify_vivado_message_line(line),
                None,
                "should drop: {line:?}"
            );
        }
    }

    #[test]
    fn does_not_match_partial_prefixes() {
        // `ERRORS:` would be a hypothetical other label and we
        // shouldn't false-positive on it.
        assert_eq!(classify_vivado_message_line("ERRORS: bogus prefix"), None);
        assert_eq!(
            classify_vivado_message_line("INFOMERCIAL: not a message"),
            None
        );
    }

    #[test]
    fn plain_chunk_routes_to_stdout() {
        // User `puts hi` produces an ordinary stdout chunk —
        // nothing matches the Vivado prefix set so it falls
        // through to Stdout.
        assert_eq!(classify_chunk_for_sink("hi\n"), StreamKind::Stdout);
        assert_eq!(
            classify_chunk_for_sink("progress: 42%\n"),
            StreamKind::Stdout
        );
        assert_eq!(classify_chunk_for_sink(""), StreamKind::Stdout);
    }

    #[test]
    fn classified_chunk_with_stack_inherits_first_line_kind() {
        // The exact shape our send_msg_id override produces: a
        // severity-prefixed first line followed by `at ...`
        // continuation frames. The whole chunk should route to
        // the kind matching the first line, so the warning and
        // its stack stay together as one orange (or red) entry.
        let warning_with_stack = "WARNING: [Common 17-1496] tclapp out of date\n\
                                  \x20\x20at /opt/Vivado/foo.tcl:42 in ::tclapp::loader\n\
                                  \x20\x20at /opt/Vivado/init.tcl:10\n";
        assert_eq!(
            classify_chunk_for_sink(warning_with_stack),
            StreamKind::Warning
        );

        let error_with_stack = "ERROR: [BD 5-148] no open project\n\
                                \x20\x20at /opt/Vivado/bd.tcl:99 in ::bd::create\n";
        assert_eq!(
            classify_chunk_for_sink(error_with_stack),
            StreamKind::Error
        );
    }

    #[test]
    fn shim_log_lines_route_as_info() {
        // Our shim's own log lines start with `[vw-shim]`. They're
        // diagnostic-level info — we don't want them to look like
        // a hot error, just a "here's what the worker said"
        // notice in the scrollback.
        assert_eq!(
            classify_vivado_message_line(
                "[vw-shim] installed send_msg_id override"
            ),
            Some(StreamKind::Info)
        );
        assert_eq!(
            classify_vivado_message_line(
                "[vw-shim] ::common::send_msg_id not present; skipping override"
            ),
            Some(StreamKind::Info)
        );
        // The matcher uses an explicit allowlist — see
        // VW_LOG_PREFIXES. Each member routes as Info; anything
        // outside the list does NOT match, even when it bears
        // the `[vw-` shape.
        for prefix in super::VW_LOG_PREFIXES {
            let line = format!("{prefix} something");
            assert_eq!(
                classify_vivado_message_line(&line),
                Some(StreamKind::Info),
                "should classify our known prefix: {prefix}"
            );
        }
        // Look-alike inside our namespace that we DIDN'T sanction
        // (a future shim subsystem nobody added to the allowlist,
        // or a user's puts that happens to bracket `[vw-*]`) is
        // rejected — keeps the classifier conservative.
        assert_eq!(classify_vivado_message_line("[vw-mystuff] foo"), None);
        assert_eq!(classify_vivado_message_line("[other] foo"), None);
    }

    #[test]
    fn classified_chunk_with_leading_indent_still_routes() {
        // `classify_vivado_message_line` tolerates leading
        // whitespace; `classify_chunk_for_sink` should inherit
        // that — Vivado occasionally indents scripted messages.
        let chunk = "  WARNING: [X 1-2] indented\n  at foo:1\n";
        assert_eq!(classify_chunk_for_sink(chunk), StreamKind::Warning);
    }
}
