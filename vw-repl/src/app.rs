// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! REPL state machine + event loop.
//!
//! Single tokio task drives both the ratatui screen and the Vivado
//! worker. Inputs are crossterm key events; outputs are eval results
//! from the worker plus our own scrollback updates. A `tokio::select!`
//! arbitrates the two so neither side blocks the other.
//!
//! A Vivado eval can take seconds to minutes. The UI stays
//! responsive throughout: the input area locks (`eval_in_flight`)
//! but the screen still redraws, the worker's stdout still streams
//! into scrollback as it arrives, and Ctrl-C cancels the in-flight
//! eval (sent as a TCL interrupt to the worker).

use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_textarea::{Input, TextArea};
use vw_eda::EdaBackend;

use crate::history::History;
use crate::session::Session;
use crate::ui::{self, WorkerStatusView};
use crate::{ReplError, ReplOptions};

/// What category an entry in the scrollback log belongs to. Drives
/// the per-line gutter prefix and color.
#[derive(Clone, Copy, Debug)]
pub enum ScrollbackKind {
    /// Echo of an input the user submitted.
    Input,
    /// A return value from a successful eval.
    Result,
    /// Captured stdout from `puts` etc. during an eval.
    Stdout,
    /// An error — TCL-level or REPL-level.
    Error,
    /// A pre-flight warning the user should see before the
    /// underlying eval result — e.g. "this call uses keyword args
    /// but isn't a loaded htcl wrapper." Distinct color from
    /// notices so it actually pulls the eye.
    Warning,
    /// Internal notice (`vivado: ready`, `:load`, `:restart`, etc.).
    Notice,
}

#[derive(Clone, Debug)]
pub struct ScrollbackEntry {
    pub kind: ScrollbackKind,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ReverseSearch {
    /// The substring the user is searching for.
    pub query: String,
    /// Index in [`History::entries`] of the current match.
    pub match_index: Option<usize>,
    /// The matched entry's text, cloned for the UI's static lifetime.
    pub match_text: String,
}

pub struct App {
    opts: ReplOptions,
    input: TextArea<'static>,
    history: History,
    session: Session,
    scrollback: Vec<ScrollbackEntry>,
    scrollback_scroll: u16,
    reverse_search: Option<ReverseSearch>,
    worker_state: WorkerState,
    worker_tx: mpsc::Sender<WorkerCmd>,
    eval_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    /// The input we shipped to the worker but haven't yet seen a
    /// result for. Held aside so a successful eval (and only a
    /// successful one) commits to the session document.
    pending_input: Option<String>,
    /// Proc-name → body-location map for the in-flight batch. Used
    /// by the error renderer to translate Tcl's `(procedure "X"
    /// line N)` frames back to htcl file:line locations.
    pending_procs: std::collections::HashMap<String, crate::lower::ProcLocation>,
    /// Set when `:quit` (or Ctrl-D on an empty buffer) fires, so the
    /// outer loop bails out after the current frame.
    exit: bool,
}

enum WorkerState {
    Starting,
    Ready,
    Running,
    Down,
}

/// Commands sent from the UI to the worker task. A batch ships one
/// or more lowered htcl statements; the worker fires `eval` per
/// item, sends one [`WorkerEvent::EvalDone`] per item, and stops at
/// the first failure so we don't keep running a script after it's
/// hit an error.
enum WorkerCmd {
    EvalBatch(Vec<crate::lower::PreparedCommand>),
    Shutdown,
}

/// Events sent from the worker task back to the UI.
enum WorkerEvent {
    Started,
    Stdout(String),
    /// One item of a batch completed. `origin` is the htcl source
    /// location the lowered Tcl came from so the renderer can show
    /// `file:line` rather than a Tcl stack trace pointing at the
    /// shim. `last_in_batch` lets the UI know when to commit to
    /// the session document.
    EvalDone {
        origin: crate::lower::Origin,
        result: Result<vw_eda::EvalOutput, vw_eda::BackendError>,
        last_in_batch: bool,
    },
    StartFailed(vw_eda::BackendError),
}

pub async fn run(opts: ReplOptions) -> Result<(), ReplError> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal, opts).await;

    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    opts: ReplOptions,
) -> Result<(), ReplError> {
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerCmd>(8);
    let (event_tx, eval_rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let verbose = opts.verbose;
    tokio::spawn(worker_task(worker_rx, event_tx, verbose));

    let mut app = App::new(opts, worker_tx, eval_rx);
    let mut crossterm_events = crossterm::event::EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.exit {
            let _ = app.worker_tx.send(WorkerCmd::Shutdown).await;
            return Ok(());
        }

        tokio::select! {
            maybe_event = crossterm_events.next() => {
                match maybe_event {
                    Some(Ok(ev)) => app.handle_terminal_event(ev).await,
                    Some(Err(e)) => {
                        app.push(ScrollbackKind::Error, format!("terminal: {e}"));
                    }
                    None => {
                        app.exit = true;
                    }
                }
            }
            Some(event) = app.eval_rx.recv() => {
                app.handle_worker_event(event).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                // Periodic wake: lets the spinner / "starting" status
                // animate even when nothing else is happening.
            }
        }
    }
}

impl App {
    fn new(
        opts: ReplOptions,
        worker_tx: mpsc::Sender<WorkerCmd>,
        eval_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    ) -> Self {
        let mut input = TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            opts,
            input,
            history: History::load_default(),
            session: Session::new(),
            scrollback: Vec::new(),
            scrollback_scroll: 0,
            reverse_search: None,
            worker_state: WorkerState::Starting,
            worker_tx,
            eval_rx,
            pending_input: None,
            pending_procs: std::collections::HashMap::new(),
            exit: false,
        }
    }

    // --- queries used by ui.rs ---------------------------------------

    pub fn scrollback(&self) -> &[ScrollbackEntry] {
        &self.scrollback
    }
    pub fn scrollback_scroll(&self) -> u16 {
        self.scrollback_scroll
    }
    pub fn input_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.input
    }
    pub fn input_line_count(&self) -> usize {
        self.input.lines().len()
    }
    pub fn reverse_search(&self) -> Option<&ReverseSearch> {
        self.reverse_search.as_ref()
    }
    pub fn worker_state(&self) -> WorkerStatusView {
        match self.worker_state {
            WorkerState::Starting => WorkerStatusView::Starting,
            WorkerState::Ready => WorkerStatusView::Ready,
            WorkerState::Running => WorkerStatusView::Running,
            WorkerState::Down => WorkerStatusView::Down,
        }
    }
    pub fn eval_in_flight(&self) -> bool {
        matches!(self.worker_state, WorkerState::Running)
    }

    /// Whether the parser considers the current input buffer ready
    /// to ship. Drives the input-area title and Enter behavior.
    pub fn input_is_complete(&self) -> bool {
        let buf = self.current_input_text();
        is_buffer_complete(&buf)
    }

    fn current_input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    // --- event handling ---------------------------------------------

    async fn handle_terminal_event(&mut self, ev: Event) {
        let Event::Key(key) = ev else { return };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        if self.reverse_search.is_some() {
            self.handle_reverse_search_key(key).await;
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                if self.input.is_empty() {
                    self.push(
                        ScrollbackKind::Notice,
                        "exit".to_string(),
                    );
                    self.exit = true;
                }
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Clear the current input (reedline convention). Once
                // we have eval cancellation we'll also kick the
                // worker here when an eval is in flight.
                self.input = TextArea::default();
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                self.reverse_search = Some(ReverseSearch {
                    query: String::new(),
                    match_index: None,
                    match_text: String::new(),
                });
            }
            (KeyCode::PageUp, _) => {
                self.scrollback_scroll =
                    self.scrollback_scroll.saturating_add(5);
            }
            (KeyCode::PageDown, _) => {
                self.scrollback_scroll =
                    self.scrollback_scroll.saturating_sub(5);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.on_submit().await;
            }
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::ALT)
                    || m.contains(KeyModifiers::SHIFT) =>
            {
                // Explicit newline regardless of parser
                // completeness — escape hatch for "I really want to
                // keep typing."
                self.input.insert_newline();
            }
            _ => {
                // Forward everything else to the text editor.
                let input: Input = key.into();
                let _consumed = self.input.input(input);
            }
        }
    }

    async fn handle_reverse_search_key(&mut self, key: KeyEvent) {
        let Some(rs) = self.reverse_search.as_mut() else { return };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.reverse_search = None;
            }
            (KeyCode::Enter, _) => {
                let text = std::mem::take(&mut rs.match_text);
                self.reverse_search = None;
                if !text.is_empty() {
                    self.set_input_to(&text);
                }
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                let start = rs.match_index;
                if let Some((idx, hit)) =
                    self.history.search_back(&rs.query, start)
                {
                    rs.match_index = Some(idx);
                    rs.match_text = hit.to_string();
                }
            }
            (KeyCode::Backspace, _) => {
                rs.query.pop();
                self.rerun_reverse_search();
            }
            (KeyCode::Char(c), m)
                if !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                rs.query.push(c);
                self.rerun_reverse_search();
            }
            _ => {}
        }
    }

    fn rerun_reverse_search(&mut self) {
        let Some(rs) = self.reverse_search.as_mut() else { return };
        match self.history.search_back(&rs.query, None) {
            Some((idx, hit)) => {
                rs.match_index = Some(idx);
                rs.match_text = hit.to_string();
            }
            None => {
                rs.match_index = None;
                rs.match_text.clear();
            }
        }
    }

    fn set_input_to(&mut self, text: &str) {
        let mut ta = TextArea::default();
        ta.set_cursor_line_style(ratatui::style::Style::default());
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                ta.insert_newline();
            }
            ta.insert_str(line);
        }
        self.input = ta;
    }

    async fn on_submit(&mut self) {
        let text = self.current_input_text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if !is_buffer_complete(&text) {
            self.input.insert_newline();
            return;
        }

        self.history.append(&text);
        self.push(ScrollbackKind::Input, text.clone());

        if let Some(cmd) = trimmed.strip_prefix(':') {
            self.run_meta_command(cmd).await;
        } else {
            self.dispatch_eval(text).await;
        }

        self.input = TextArea::default();
        self.scrollback_scroll = 0;
    }

    async fn dispatch_eval(&mut self, text: String) {
        if matches!(self.worker_state, WorkerState::Down) {
            self.push(
                ScrollbackKind::Error,
                "vivado worker is down — try :restart".into(),
            );
            return;
        }
        if matches!(self.worker_state, WorkerState::Starting) {
            self.push(
                ScrollbackKind::Notice,
                "queued — vivado still starting".into(),
            );
        }

        // Lower htcl → Tcl through the same loader / signature-
        // table / call-site-rewrite pipeline `vw run` uses, against
        // the workspace whose `vw.toml` lives at or above the cwd.
        // A lowering failure (unknown dep, parse error in an
        // imported file, etc.) never reaches Vivado.
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let lowered = match crate::lower::prepare(&text, &cwd) {
            Ok(l) => l,
            Err(e) => {
                // The user cares "did my input run or not" — the
                // fact that this came back from the lowering
                // pipeline (vs. the Vivado worker) is internal
                // accounting. Just say ERROR.
                self.push(
                    ScrollbackKind::Error,
                    format!("ERROR: {e}"),
                );
                return;
            }
        };

        // Surface any pre-flight warnings *before* shipping. If the
        // eval then fails, the user already has the context they
        // need to interpret the Vivado error.
        for w in &lowered.warnings {
            let where_ =
                render_origin_path(w.origin.file.as_deref(), w.origin.line);
            self.push(
                ScrollbackKind::Warning,
                format!("warning: {where_}: {}", w.message),
            );
        }
        if lowered.commands.is_empty() {
            // Pure `src` import or comments-only input. Commit the
            // imported source to the session anyway so future
            // analyzer queries see the imported procs.
            self.session.commit(&lowered.committed_source);
            self.push(
                ScrollbackKind::Notice,
                "(no Tcl to evaluate)".into(),
            );
            return;
        }

        // Commit to the session document only after every command
        // in the batch succeeds (see `handle_worker_event`); a
        // failure mid-batch shouldn't pollute the analyzer's view.
        let _ = self
            .worker_tx
            .send(WorkerCmd::EvalBatch(lowered.commands))
            .await;
        self.pending_input = Some(lowered.committed_source);
        self.pending_procs = lowered.procs;
        self.worker_state = WorkerState::Running;
    }

    async fn run_meta_command(&mut self, cmd: &str) {
        let mut parts = cmd.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match name {
            "quit" | "q" | "exit" => {
                self.exit = true;
            }
            "restart" => {
                self.push(
                    ScrollbackKind::Notice,
                    "restart not yet implemented (stubbed for v1)".into(),
                );
            }
            "load" => {
                if arg.is_empty() {
                    self.push(
                        ScrollbackKind::Error,
                        ":load needs a path".into(),
                    );
                    return;
                }
                match std::fs::read_to_string(arg) {
                    Ok(content) => {
                        self.push(
                            ScrollbackKind::Notice,
                            format!("loading {arg}"),
                        );
                        self.dispatch_eval(content).await;
                    }
                    Err(e) => {
                        self.push(
                            ScrollbackKind::Error,
                            format!("could not read {arg}: {e}"),
                        );
                    }
                }
            }
            other => {
                self.push(
                    ScrollbackKind::Error,
                    format!("unknown meta-command :{other}"),
                );
            }
        }
    }

    async fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Started => {
                self.worker_state = WorkerState::Ready;
                self.push(
                    ScrollbackKind::Notice,
                    "vivado ready".into(),
                );
                if let Some(path) = self.opts.initial_load.clone() {
                    match std::fs::read_to_string(path.as_std_path()) {
                        Ok(content) => {
                            self.push(
                                ScrollbackKind::Notice,
                                format!("auto-loading {path}"),
                            );
                            self.dispatch_eval(content).await;
                        }
                        Err(e) => {
                            self.push(
                                ScrollbackKind::Error,
                                format!("could not read {path}: {e}"),
                            );
                        }
                    }
                }
            }
            WorkerEvent::StartFailed(e) => {
                self.worker_state = WorkerState::Down;
                self.push(
                    ScrollbackKind::Error,
                    format!("vivado failed to start: {e}"),
                );
            }
            WorkerEvent::Stdout(chunk) => {
                self.push(ScrollbackKind::Stdout, chunk);
            }
            WorkerEvent::EvalDone {
                origin,
                result,
                last_in_batch,
            } => {
                match result {
                    Ok(out) => {
                        // Drop the per-statement chatter — only the
                        // last item's value lands in scrollback so a
                        // `src @vivado-cmd` that runs 851 wrappers
                        // doesn't drown the user in "ok" lines. The
                        // intermediate procs etc. are silent unless
                        // they `puts` something (already streamed).
                        if last_in_batch {
                            if !out.stdout.is_empty() {
                                self.push(
                                    ScrollbackKind::Stdout,
                                    out.stdout
                                        .trim_end_matches('\n')
                                        .to_string(),
                                );
                            }
                            if !out.value.is_empty() {
                                self.push(
                                    ScrollbackKind::Result,
                                    out.value.clone(),
                                );
                            }
                            let pending =
                                self.pending_input.take().unwrap_or_default();
                            self.session.commit(&pending);
                            self.worker_state = WorkerState::Ready;
                        }
                    }
                    Err(err) => {
                        self.worker_state = WorkerState::Ready;
                        self.pending_input = None;
                        render_eval_error(self, &origin, err);
                    }
                }
            }
        }
    }

    pub(crate) fn push(&mut self, kind: ScrollbackKind, text: String) {
        self.scrollback.push(ScrollbackEntry { kind, text });
    }
}

// ---------------------------------------------------------------------
// Worker task: owns the Vivado backend, serializes evals.
// ---------------------------------------------------------------------

async fn worker_task(
    mut rx: mpsc::Receiver<WorkerCmd>,
    tx: mpsc::UnboundedSender<WorkerEvent>,
    verbose: bool,
) {
    let backend = vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
        verbose,
        ..Default::default()
    })
    .await;
    let mut backend = match backend {
        Ok(b) => {
            let _ = tx.send(WorkerEvent::Started);
            b
        }
        Err(e) => {
            let _ = tx.send(WorkerEvent::StartFailed(e));
            return;
        }
    };

    // Stream stdout chunks to the UI as they arrive. The closure
    // captures the unbounded sender so it can fire without
    // awaiting.
    let stdout_tx = tx.clone();
    backend.set_stdout_sink(move |chunk: &str| {
        let _ = stdout_tx.send(WorkerEvent::Stdout(chunk.to_string()));
    });

    while let Some(cmd) = rx.recv().await {
        match cmd {
            WorkerCmd::EvalBatch(items) => {
                let total = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    let result = backend.eval(&item.tcl).await;
                    let failed = result.is_err();
                    let last_in_batch = i + 1 == total || failed;
                    let _ = tx.send(WorkerEvent::EvalDone {
                        origin: item.origin,
                        result,
                        last_in_batch,
                    });
                    // Stop the batch at the first failure — running
                    // the rest of a script after an error confuses
                    // the user and risks side effects nobody
                    // intended.
                    if failed {
                        break;
                    }
                }
            }
            WorkerCmd::Shutdown => break,
        }
    }
    let _ = backend.shutdown().await;
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Render a Vivado error as a clean Python-style stack trace —
/// one frame per `file:line` from the outermost `src` the user
/// typed, down through any nested `src` imports, the leaf
/// statement we shipped, and any `(procedure X line N)` frames the
/// Tcl interpreter reported. The error message itself comes last.
///
/// ```text
/// ip/cips.htcl:1
///   src @cips
/// ~/src/htcl/amd/cips/module.htcl:3
///   ip::check -name "xilinx.com:ip:versal_cips:3.4"
/// ~/src/htcl/amd/vivado-cmd/ip.htcl:18
///   set ip_obj [get_ipdefs -all "$name"]
/// ERROR: [Common 17-53] No open project. ...
/// ```
fn render_eval_error(
    app: &mut App,
    origin: &crate::lower::Origin,
    err: vw_eda::BackendError,
) {
    let mut frames: Vec<Frame> = Vec::new();

    // Outermost first: walk the `via` chain in reverse so the
    // entry `src` lands at the top.
    for f in origin.via.iter().rev() {
        frames.push(Frame {
            file: f.file.clone(),
            line: f.line,
            snippet: f.snippet.clone(),
        });
    }
    // Leaf htcl statement — the actual call site that triggered
    // the Tcl evaluation.
    frames.push(Frame {
        file: origin.file.clone(),
        line: origin.line,
        snippet: origin.snippet.clone(),
    });

    // If Vivado gave us a Tcl trace, drill into any
    // `(procedure "X" line N)` frames whose proc we recognize, so
    // the user sees the actual failing line inside the proc body
    // — not just the call to it.
    let (message, code, info, stdout) = match err {
        vw_eda::BackendError::Tcl {
            message,
            code,
            info,
            stdout,
        } => (message, code, info, stdout),
        other => {
            for frame in &frames {
                push_frame(app, frame);
            }
            app.push(
                ScrollbackKind::Error,
                format!("{other}"),
            );
            return;
        }
    };
    if let Some(info) = info.as_deref() {
        for tcl_frame in parse_tcl_proc_frames(info) {
            let Some(loc) = app.pending_procs.get(&tcl_frame.proc) else {
                continue;
            };
            let Some((abs_line, content)) =
                loc.resolve_body_line(tcl_frame.line)
            else {
                continue;
            };
            frames.push(Frame {
                file: loc.file.clone(),
                line: abs_line,
                snippet: content.trim().to_string(),
            });
        }
    }

    if !stdout.is_empty() {
        app.push(
            ScrollbackKind::Stdout,
            stdout.trim_end_matches('\n').to_string(),
        );
    }

    for frame in &frames {
        push_frame(app, frame);
    }
    app.push(ScrollbackKind::Error, message.trim().to_string());
    if let Some(code) = code.filter(|s| !s.is_empty() && s != "NONE") {
        app.push(ScrollbackKind::Notice, format!("({code})"));
    }
}

struct Frame {
    file: Option<std::path::PathBuf>,
    line: u32,
    snippet: String,
}

fn push_frame(app: &mut App, frame: &Frame) {
    let where_ = render_origin_path(frame.file.as_deref(), frame.line);
    app.push(ScrollbackKind::Notice, where_);
    if frame.snippet.is_empty() {
        return;
    }
    // Indent every line of the snippet — for a multi-line
    // command (`set proj [\n  create_project\n    -name x\n]`)
    // this preserves the user's relative indentation so the
    // structure is readable, while the gutter prefix added by
    // `entry_lines` distinguishes the first line from the
    // continuations.
    let body = frame
        .snippet
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push(ScrollbackKind::Notice, body);
}

/// Parse `(procedure "NAME" line N)` annotations out of Tcl's
/// `$errorInfo`. Returned in the order they appear in `info`,
/// which is innermost-first per Tcl convention — but the renderer
/// wants OUTERMOST-first (we already have the outer leaf frame from
/// the htcl side), so we reverse here and yield the inner frames
/// in execution order.
fn parse_tcl_proc_frames(info: &str) -> Vec<TclProcFrame> {
    let mut out = Vec::new();
    for line in info.lines() {
        let trimmed = line.trim();
        // Expected shape: `(procedure "NAME" line N)`
        let Some(rest) = trimmed.strip_prefix("(procedure \"") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once("\" line ") else {
            continue;
        };
        let Some(num) = rest.strip_suffix(')') else { continue };
        let Ok(n) = num.parse::<u32>() else { continue };
        out.push(TclProcFrame {
            proc: name.to_string(),
            line: n,
        });
    }
    // errorInfo lists innermost first; we want execution order
    // (outermost first) so reverse.
    out.reverse();
    out
}

struct TclProcFrame {
    proc: String,
    line: u32,
}

fn render_origin_path(
    file: Option<&std::path::Path>,
    line: u32,
) -> String {
    match file {
        Some(p) => format!("{}:{line}", display_path(p)),
        None => format!("(input):{line}"),
    }
}

/// Shorten a path for display: drop the cwd prefix when it lines
/// up, leave it absolute otherwise. Saves screen real estate when
/// reporting errors from a dep cached deep under `~/.vw/deps/...`.
fn display_path(path: &std::path::Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
        if let Some(home) = dirs::home_dir() {
            if let Ok(rel) = path.strip_prefix(&home) {
                return format!("~/{}", rel.display());
            }
        }
    }
    path.display().to_string()
}

/// Decide whether the input buffer parses cleanly enough to ship to
/// Vivado, or whether the user is still in the middle of typing
/// (unterminated brace, etc.). We re-use the htcl parser since
/// it already understands every multi-line construct (procs,
/// `[ … ]` substitutions, braced groups).
fn is_buffer_complete(text: &str) -> bool {
    let parsed = vw_htcl::parse(text);
    !parsed
        .errors
        .iter()
        .any(|e| e.message.contains("unterminated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_complete_for_simple_statement() {
        assert!(is_buffer_complete("set x 1"));
        assert!(is_buffer_complete("puts \"hi\""));
    }

    #[test]
    fn buffer_incomplete_with_unterminated_brace() {
        assert!(!is_buffer_complete(
            "set x [\n  create_cpm5\n    -name cpm5"
        ));
        assert!(!is_buffer_complete("proc foo {"));
    }

    #[test]
    fn buffer_complete_for_multiline_well_formed_proc() {
        assert!(is_buffer_complete(
            "proc foo {\n  @default(1) x\n} {\n  puts $x\n}"
        ));
    }
}
