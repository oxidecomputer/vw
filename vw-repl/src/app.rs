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
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_textarea::{Input, TextArea};
use vw_eda::EdaBackend;

use crate::history::History;
use crate::lower::Origin;
use crate::session::{Session, SessionBatch};
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

/// Drag-selection over scrollback rows. Coordinates are `(row, col)`
/// indices into the post-wrap line list (see
/// [`crate::render::wrap_lines`]) — i.e. the same indexing the
/// renderer uses for `Paragraph::scroll`. `anchor` is where the user
/// pressed; `cursor` updates while dragging. The range may be
/// inverted (cursor before anchor) — callers normalize via
/// [`Selection::ordered`] before applying.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
}

impl Selection {
    /// Return `(start, end)` with `start <= end` so callers don't
    /// have to special-case backwards drags.
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
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
    /// Where the up/down (Ctrl-P/Ctrl-N) history walk is currently
    /// positioned. `None` means "composing a fresh entry" — the
    /// state any new keypress (other than Ctrl-P/N themselves)
    /// drops back into so editing after walking history doesn't
    /// keep recalling stale entries. `Some(i)` indexes
    /// `History::entries()` directly.
    history_cursor: Option<usize>,
    /// In-progress draft saved when the user first walks back into
    /// history with Ctrl-P. Restored on Ctrl-N past the newest
    /// entry, so the draft they were typing isn't lost.
    history_draft: String,
    session: Session,
    scrollback: Vec<ScrollbackEntry>,
    scrollback_scroll: u16,
    /// Whether the terminal is currently capturing mouse events.
    /// Default ON since we implement our own drag-to-select +
    /// clipboard copy (see [`Selection`]). F2 toggles it off for
    /// users who'd rather use terminal-native selection (which
    /// requires capture to be disabled because the protocol is
    /// all-or-nothing).
    mouse_capture: bool,
    /// Last scrollback render area, captured by `ui::draw_scrollback`
    /// each frame. Lets mouse handlers map screen coords back to
    /// scrollback-local cells without round-tripping through the UI.
    scrollback_area: Option<Rect>,
    /// Active drag-selection in scrollback. Coordinates are
    /// `(row, col)` indices into the post-wrap scrollback line list
    /// — see `render::wrap_lines`. `None` outside of an active drag
    /// or after a copy completes.
    selection: Option<Selection>,
    /// Tail-follow mode. When `true`, the renderer pins the
    /// effective scroll offset to the bottom of the wrapped-row
    /// list — same model as `tail -f` or a fresh terminal. Manual
    /// scroll-up flips this off so the user can read older content
    /// without the view jumping out from under them; scrolling
    /// back down to the bottom flips it back on. Submitting a new
    /// command resets to `true`.
    scrollback_follow: bool,
    /// The effective scroll offset the renderer used on the most
    /// recent frame. Written by `ui::draw_scrollback`, read by the
    /// mouse / keyboard scroll handlers so a manual move from
    /// tail-follow mode "takes over" the rendered position rather
    /// than jumping back to whatever stale value is in
    /// `scrollback_scroll`.
    last_rendered_scroll: u16,
    reverse_search: Option<ReverseSearch>,
    worker_state: WorkerState,
    worker_tx: mpsc::Sender<WorkerCmd>,
    eval_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    /// Origins of every command we shipped to the worker in the
    /// current batch, in eval order, paired with an index for the
    /// next not-yet-acknowledged command. Lets the stream handler
    /// tag a mid-eval Vivado warning with `at <user's file>:<line>
    /// in <user's call>` even when Vivado bypasses our Tcl-level
    /// stack capture (the IP_Flow C++ property validators don't
    /// call `::common::send_msg_id`, so neither shim override
    /// fires — without this fallback those warnings would arrive
    /// stack-less).
    pending_origins: Vec<Origin>,
    /// Parallel to `pending_origins`: the expected return type of
    /// each shipped command, when statically resolvable. Used by
    /// the `EvalDone` handler to (a) skip the heuristic formatter
    /// when the value is already type-formatted by the wrapped
    /// Tcl, and (b) suppress the Result push entirely for
    /// `unit`-typed expressions.
    pending_return_types: Vec<Option<vw_htcl::TypeExpr>>,
    pending_eval_index: usize,
    /// The batch we shipped to the worker but haven't yet seen a
    /// result for. Held aside so a successful eval (and only a
    /// successful one) commits to the session — and so the error
    /// renderer can look up procs declared in this in-flight
    /// batch (which aren't yet in `session`) when drilling into a
    /// Tcl stack frame.
    pending_batch: Option<SessionBatch>,
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
    /// One streaming chunk from the worker, with its source-of-
    /// origin tag so the UI can render Vivado WARNING/ERROR lines
    /// distinctly from user `puts` output.
    Stream {
        kind: vw_vivado::StreamKind,
        data: String,
    },
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
    // Mouse capture ON by default — the app implements its own
    // drag-to-select + clipboard copy so users get text selection
    // back (helix-style: app-level highlight rendered with
    // `Modifier::REVERSED`, copied to the OS clipboard on mouse
    // release). F2 toggles capture off if a user would rather use
    // their terminal's native selection.
    stdout.execute(EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal, opts).await;

    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // No-op if capture was already disabled via F2.
    let _ = stdout.execute(DisableMouseCapture);
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
    // Verbose output can't go to stderr in REPL mode — that's the
    // same fd the TUI renders on, so any byte stomps through the
    // alternate-screen buffer. Route it to a per-process tempfile
    // instead and tell the user where to find it.
    let verbose_log_path = if verbose {
        Some(
            std::env::temp_dir()
                .join(format!("vw-repl-vivado-{}.log", std::process::id())),
        )
    } else {
        None
    };
    tokio::spawn(worker_task(
        worker_rx,
        event_tx,
        verbose,
        verbose_log_path.clone(),
    ));

    let mut app = App::new(opts, worker_tx, eval_rx);
    if let Some(p) = verbose_log_path {
        app.push(
            ScrollbackKind::Notice,
            format!(
                "verbose output streaming to {} — `tail -f` from \
                 another terminal",
                p.display()
            ),
        );
    }
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
            history_cursor: None,
            history_draft: String::new(),
            session: Session::new(),
            scrollback: Vec::new(),
            scrollback_scroll: 0,
            mouse_capture: true,
            scrollback_area: None,
            selection: None,
            scrollback_follow: true,
            last_rendered_scroll: 0,
            reverse_search: None,
            worker_state: WorkerState::Starting,
            worker_tx,
            eval_rx,
            pending_batch: None,
            pending_origins: Vec::new(),
            pending_return_types: Vec::new(),
            pending_eval_index: 0,
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
    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }
    /// Called by `ui::draw_scrollback` each frame so subsequent
    /// mouse events can translate absolute screen coords into
    /// scrollback-local rows/cols.
    pub fn set_scrollback_area(&mut self, area: Rect) {
        self.scrollback_area = Some(area);
    }

    pub fn scrollback_follow(&self) -> bool {
        self.scrollback_follow
    }

    /// Renderer-side writeback: records the scroll offset that was
    /// actually used to paint the current frame. Mouse / keyboard
    /// scroll handlers anchor their deltas off this so transitioning
    /// out of tail-follow doesn't jump back to a stale
    /// `scrollback_scroll` value.
    pub fn set_last_rendered_scroll(&mut self, offset: u16) {
        self.last_rendered_scroll = offset;
    }

    /// Toggle terminal mouse capture. Writes the enable/disable
    /// sequence directly to stdout — the alternate-screen / raw-mode
    /// context that `run()` set up is still active.
    fn toggle_mouse_capture(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = if self.mouse_capture {
            stdout.execute(DisableMouseCapture)
        } else {
            stdout.execute(EnableMouseCapture)
        };
        self.mouse_capture = !self.mouse_capture;
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

    /// Walk the input history by `delta` (negative = older,
    /// positive = newer). Readline-style: first step back from the
    /// "composing" position saves the current draft; stepping
    /// past the newest entry restores it. Empty history is a no-op.
    fn history_step(&mut self, delta: i32) {
        let entries = self.history.entries();
        if entries.is_empty() {
            return;
        }
        let cursor = match (self.history_cursor, delta) {
            (None, d) if d >= 0 => return, // already at draft, can't go newer
            (None, _) => {
                // Stepping back from the draft for the first time —
                // capture the in-progress text so Ctrl-N past the
                // newest entry can restore it.
                self.history_draft = self.current_input_text();
                entries.len().saturating_sub(1)
            }
            (Some(i), d) => {
                let new = i as i32 + d;
                if new < 0 {
                    0
                } else if new >= entries.len() as i32 {
                    // Past the newest entry — drop back to draft.
                    self.history_cursor = None;
                    let draft = std::mem::take(&mut self.history_draft);
                    self.replace_input_with(&draft);
                    return;
                } else {
                    new as usize
                }
            }
        };
        self.history_cursor = Some(cursor);
        let text = entries[cursor].clone();
        self.replace_input_with(&text);
    }

    /// Reset the input buffer to `text`, placing the cursor at the
    /// end. Used by history navigation and reverse-search accept.
    fn replace_input_with(&mut self, text: &str) {
        self.input = TextArea::default();
        for (i, line) in text.lines().enumerate() {
            if i > 0 {
                self.input.insert_newline();
            }
            self.input.insert_str(line);
        }
        // If `text` ended with a newline, `lines()` drops it; preserve.
        if text.ends_with('\n') {
            self.input.insert_newline();
        }
    }

    // --- event handling ---------------------------------------------

    fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        // Wheel events scroll the scrollback buffer. 3 lines per
        // notch is the de-facto terminal-emulator default and
        // matches what feels natural when you've held the wheel
        // for half a second. Keyboard scroll (Ctrl-J/K) still
        // jumps 5 — the wheel is finer-grained because the user
        // can keep spinning.
        //
        // Direction: `scrollback_scroll` is ratatui's `scroll.y`,
        // which counts lines skipped from the TOP of the buffer.
        // Wheel-up should reveal older content above the viewport
        // (terminal convention), which means moving the viewport
        // UP through the buffer — i.e. SUBTRACTING from
        // `scrollback_scroll`. Wheel-down does the reverse.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_by(-3);
                return;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(3);
                return;
            }
            _ => {}
        }

        // Drag-selection lives within the scrollback area only.
        // Outside it, mouse events are ignored — the input box has
        // its own selection model via tui-textarea and we don't
        // want a click on a status bar to start a scrollback drag.
        let Some(area) = self.scrollback_area else {
            return;
        };
        let in_area = mouse.column >= area.x
            && mouse.column < area.x + area.width
            && mouse.row >= area.y
            && mouse.row < area.y + area.height;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if in_area => {
                self.selection = Some(Selection {
                    anchor: self.cell_to_buffer(mouse.column, mouse.row, area),
                    cursor: self.cell_to_buffer(mouse.column, mouse.row, area),
                });
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.selection.is_some() =>
            {
                // Clamp to the area: dragging outside still
                // updates the cursor to the edge so selection
                // can extend through the visible viewport even
                // when the mouse strays.
                let col = mouse
                    .column
                    .clamp(area.x, area.x + area.width.saturating_sub(1));
                let row = mouse
                    .row
                    .clamp(area.y, area.y + area.height.saturating_sub(1));
                let cursor = self.cell_to_buffer(col, row, area);
                if let Some(sel) = self.selection.as_mut() {
                    sel.cursor = cursor;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.selection.take() {
                    self.copy_selection_to_clipboard(sel);
                }
            }
            _ => {}
        }
    }

    /// Translate a screen cell `(col, row)` inside the scrollback
    /// `area` into a `(row, col)` index into the post-wrap line list.
    /// The row index is `effective_scroll + (row - area.y)` so the
    /// caller doesn't have to know about scroll state.
    ///
    /// Anchors against `last_rendered_scroll` rather than
    /// `scrollback_scroll`. While tail-follow is on the renderer
    /// computes the pinned offset on the fly and never writes it
    /// back to `scrollback_scroll` — using the stale field here
    /// would map mouse clicks to the wrong buffer rows once any
    /// real volume of output has scrolled the viewport.
    fn cell_to_buffer(&self, col: u16, row: u16, area: Rect) -> (usize, usize) {
        let local_row = row.saturating_sub(area.y) as usize;
        let local_col = col.saturating_sub(area.x) as usize;
        let buf_row = self.last_rendered_scroll as usize + local_row;
        (buf_row, local_col)
    }

    /// Build the same post-wrap line list the UI renders, extract
    /// the cells inside `sel`, and write the resulting plain text to
    /// the OS clipboard. Failure (no clipboard backend / Wayland
    /// permissions denied / …) surfaces as a Notice line so the
    /// user knows the copy didn't go through.
    fn copy_selection_to_clipboard(&mut self, sel: Selection) {
        let Some(area) = self.scrollback_area else {
            return;
        };
        let mut flat: Vec<ratatui::text::Line<'static>> = Vec::new();
        for entry in &self.scrollback {
            for line in crate::render::entry_lines(entry) {
                flat.push(line);
            }
        }
        let wrapped = crate::render::wrap_lines(flat, area.width);
        let (start, end) = sel.ordered();
        if start == end {
            return; // pure click, nothing to copy
        }
        let mut out = String::new();
        let last_row = end.0.min(wrapped.len().saturating_sub(1));
        for (row_idx, line) in wrapped
            .iter()
            .enumerate()
            .skip(start.0)
            .take(last_row + 1 - start.0)
        {
            let plain = crate::render::line_plain_text(line);
            let chars: Vec<char> = plain.chars().collect();
            let row_start = if row_idx == start.0 { start.1 } else { 0 };
            let row_end = if row_idx == end.0 { end.1 } else { chars.len() };
            let row_end = row_end.min(chars.len());
            let row_start = row_start.min(row_end);
            for c in &chars[row_start..row_end] {
                out.push(*c);
            }
            if row_idx < end.0 {
                out.push('\n');
            }
        }
        if out.is_empty() {
            return;
        }
        // Primary path: OSC 52. The terminal itself puts the text on
        // the system clipboard — no DISPLAY / Wayland socket /
        // pbcopy dependency, and it works through SSH. Most modern
        // terminals support it (kitty, ghostty, iTerm2, Alacritty,
        // Wezterm, recent xterm). Some require an opt-in
        // (`set -g set-clipboard on` in tmux, `Allow programs to use
        // clipboard` in iTerm2's General → Selection prefs).
        //
        // Secondary path: arboard. When a real clipboard daemon is
        // reachable, this also syncs into the X11/Wayland clipboard
        // so other GUI apps see the text. Failures here are silent
        // because OSC 52 above is already authoritative — the
        // X11-unreachable / Wayland-without-perms case used to
        // surface as a noisy "clipboard copy failed" Notice.
        send_osc52(&out);
        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(out));
    }

    async fn handle_terminal_event(&mut self, ev: Event) {
        if let Event::Mouse(mouse) = ev {
            self.handle_mouse_event(mouse);
            return;
        }
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
                    self.push(ScrollbackKind::Notice, "exit".to_string());
                    self.exit = true;
                }
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Clear the current input (reedline convention). Once
                // we have eval cancellation we'll also kick the
                // worker here when an eval is in flight.
                self.input = TextArea::default();
                self.history_cursor = None;
                self.history_draft.clear();
            }
            (KeyCode::F(2), _) => {
                // Flip terminal mouse-capture mode. OFF (the default)
                // lets the terminal handle text-selection drags
                // natively; ON routes wheel events into the app for
                // scrollback navigation, at the cost of text
                // selection requiring Shift-drag / Option-drag.
                self.toggle_mouse_capture();
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                self.reverse_search = Some(ReverseSearch {
                    query: String::new(),
                    match_index: None,
                    match_text: String::new(),
                });
            }
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_step(-1);
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.history_step(1);
            }
            // Scrollback nav. PageUp/PageDown for keyboards that
            // have them; Ctrl-K (up) / Ctrl-J (down) for compact
            // keyboards (Mac laptops, 60% boards) where PageUp
            // doesn't exist physically. Vim-style direction
            // mapping — `k` is up, `j` is down. Picked over
            // Ctrl-↑/↓ because macOS intercepts those (Mission
            // Control / app-switching).
            //
            // Direction: see `handle_mouse_event` — `k`/PageUp
            // moves the viewport UP toward older content, which
            // means decreasing the y-scroll offset.
            (KeyCode::PageUp, _)
            | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.scroll_by(-5);
            }
            (KeyCode::PageDown, _)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.scroll_by(5);
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
                // Once the user starts editing, drop the history
                // cursor so subsequent Ctrl-P starts at "newest"
                // again — readline behavior: an edited recall is
                // a new entry, not a continued walk.
                self.history_cursor = None;
                let input: Input = key.into();
                let _consumed = self.input.input(input);
            }
        }
    }

    async fn handle_reverse_search_key(&mut self, key: KeyEvent) {
        let Some(rs) = self.reverse_search.as_mut() else {
            return;
        };
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
        let Some(rs) = self.reverse_search.as_mut() else {
            return;
        };
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
        // Reset history walk: the next Ctrl-P should start from the
        // newest entry, not pick up where the previous walk left
        // off across submits.
        self.history_cursor = None;
        self.history_draft.clear();
        // Re-engage tail-follow: every new submit shows the input
        // echo + its output at the bottom of the viewport, even
        // if the user had scrolled up to inspect earlier results.
        // The actual pin happens in the renderer next frame.
        self.scrollback_follow = true;
    }

    fn resolve_stack_frames(&self, msg: &str) -> String {
        resolve_stack_frames(
            msg,
            &self.session,
            self.pending_batch.as_ref(),
            self.input_file_for_resolve(),
        )
    }

    /// Append a synthetic `  at <file>:<line>` frame to streamed
    /// warnings/errors that already arrived without a stack. Vivado
    /// emits some message classes (notably `[IP_Flow 19-7090]`
    /// "Invalid parameter" warnings during `set_property`) from
    /// C++ code paths that bypass the Tcl-level `send_msg_id`
    /// override — so the shim never sees them and can't attach a
    /// real Tcl call stack. The fallback is "which user command
    /// was the worker chewing on when this byte stream arrived,"
    /// which `pending_origins[pending_eval_index]` gives us. Won't
    /// add a frame if the message already has one (the
    /// `\n  at …` shape from `attach_stack_if_message`) or if it
    /// isn't a warning/error severity.
    fn tag_streamed_message(
        &self,
        kind: ScrollbackKind,
        msg: String,
    ) -> String {
        if !matches!(kind, ScrollbackKind::Warning | ScrollbackKind::Error) {
            return msg;
        }
        if msg.contains("\n  at ") {
            return msg;
        }
        let Some(origin) = self.pending_origins.get(self.pending_eval_index)
        else {
            return msg;
        };
        let path = match origin.file.as_deref() {
            Some(p) => display_path(p),
            None => match self.input_file_for_resolve() {
                Some(p) => display_path(p),
                None => "<input>".into(),
            },
        };
        format!("{msg}\n  at {path}:{}", origin.line)
    }

    /// File to substitute for `<input>` frames in stack traces.
    /// Comes from `--load <path>` for the auto-loaded program — its
    /// content was copied verbatim into the lowering scratch, so
    /// scratch line N corresponds to load-file line N. For
    /// REPL-typed input there's no source file, so callers leave
    /// `<input>` as-is.
    fn input_file_for_resolve(&self) -> Option<&std::path::Path> {
        self.opts.initial_load.as_ref().map(|p| p.as_std_path())
    }

    async fn dispatch_eval(&mut self, text: String) {
        self.dispatch_eval_with_echo(text, false).await;
    }

    /// Same as [`dispatch_eval`] but echoes each lowered top-level
    /// statement as an Input entry first. Used by the `--load`
    /// auto-run path so the user can see *which* commands ran when
    /// reading the trace, the same way manual REPL input shows up
    /// as `› <text>` for each submit.
    async fn dispatch_eval_with_echo(&mut self, text: String, echo: bool) {
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
        let lowered = match crate::lower::prepare(&text, &cwd, &self.session) {
            Ok(l) => l,
            Err(e) => {
                // The user cares "did my input run or not" — the
                // fact that this came back from the lowering
                // pipeline (vs. the Vivado worker) is internal
                // accounting. Just say ERROR.
                self.push(ScrollbackKind::Error, format!("ERROR: {e}"));
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
            // parsed batch to the session anyway so future
            // analyzer queries see the imported procs.
            self.session.commit(lowered.batch);
            self.push(ScrollbackKind::Notice, "(no Tcl to evaluate)".into());
            return;
        }

        if echo {
            // Echo every top-level statement from the entry file.
            // `entry_top_level` is the full list — including `src`
            // directives, which lower to empty Tcl (the loader has
            // already consumed them) and therefore wouldn't appear
            // if we walked `lowered.commands`. Statements pulled in
            // via `src` are filtered out at capture time in
            // `lower::prepare`, so e.g. `src @vivado-cmd` echoes
            // its own line but not the 10k+ wrapper definitions
            // it brings in.
            for origin in &lowered.entry_top_level {
                self.push(ScrollbackKind::Input, origin.snippet.clone());
            }
        }

        // Snapshot per-command origins + types for the stream-
        // tagging + result-display paths. EvalBatch consumes
        // `lowered.commands` below, so we grab both first.
        self.pending_origins =
            lowered.commands.iter().map(|c| c.origin.clone()).collect();
        self.pending_return_types = lowered
            .commands
            .iter()
            .map(|c| c.expected_return_type.clone())
            .collect();
        self.pending_eval_index = 0;

        // Commit to the session only after every command in the
        // batch succeeds (see `handle_worker_event`); a failure
        // mid-batch shouldn't pollute the analyzer's view.
        let _ = self
            .worker_tx
            .send(WorkerCmd::EvalBatch(lowered.commands))
            .await;
        self.pending_batch = Some(lowered.batch);
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
                self.push(ScrollbackKind::Notice, "vivado ready".into());
                if let Some(path) = self.opts.initial_load.clone() {
                    match std::fs::read_to_string(path.as_std_path()) {
                        Ok(content) => {
                            self.push(
                                ScrollbackKind::Notice,
                                format!("auto-loading {path}"),
                            );
                            self.dispatch_eval_with_echo(content, true).await;
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
            WorkerEvent::Stream { kind, data } => {
                let scrollback_kind = match kind {
                    vw_vivado::StreamKind::Stdout => ScrollbackKind::Stdout,
                    vw_vivado::StreamKind::Info => ScrollbackKind::Notice,
                    vw_vivado::StreamKind::Warning => ScrollbackKind::Warning,
                    vw_vivado::StreamKind::Error => ScrollbackKind::Error,
                };
                // The PTY filter emits one line per chunk and
                // the shim's `puts` capture preserves user-side
                // newlines; trim a single trailing newline so the
                // scrollback's per-entry layout doesn't insert a
                // blank gap between Vivado messages.
                let trimmed = data.trim_end_matches('\n').to_string();
                if !trimmed.is_empty() {
                    let resolved = self.resolve_stack_frames(&trimmed);
                    // Tag warnings/errors that arrived without a
                    // stack trace with the currently-executing
                    // user command's origin. Vivado's C++
                    // property-validation path emits messages
                    // straight to the PTY without going through
                    // `::common::send_msg_id`, so neither shim
                    // override gets a chance to capture a Tcl
                    // stack — the best we can do from the
                    // worker's side is "this happened while
                    // <user line> was running."
                    let tagged =
                        self.tag_streamed_message(scrollback_kind, resolved);
                    self.push(scrollback_kind, tagged);
                }
            }
            WorkerEvent::EvalDone {
                origin,
                result,
                last_in_batch,
            } => {
                // Grab the return type for THIS command (the one
                // that just finished) before we advance the index
                // and possibly clear the buffer.
                let finished_return_type = self
                    .pending_return_types
                    .get(self.pending_eval_index)
                    .cloned()
                    .flatten();
                // Advance past the command that just finished — the
                // stream-tagging path uses `pending_origins[index]`
                // to label warnings emitted by the *currently*
                // executing command, so the index should always
                // point at "in-flight," not "just done."
                self.pending_eval_index =
                    self.pending_eval_index.saturating_add(1);
                if last_in_batch {
                    self.pending_origins.clear();
                    self.pending_return_types.clear();
                    self.pending_eval_index = 0;
                }
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
                            // Result-rendering policy:
                            //   - `unit`-typed expressions push nothing
                            //     (the value is meaningless by design).
                            //   - Other typed expressions push verbatim
                            //     — the wrapped Tcl already ran the
                            //     type's `repr` proc, so `out.value`
                            //     is the formatted display string.
                            //   - Untyped expressions fall back to the
                            //     legacy heuristic, kept for now while
                            //     the wrapper libraries grow
                            //     annotations.
                            let suppress = matches!(
                                finished_return_type.as_ref(),
                                Some(vw_htcl::TypeExpr::Named { name, .. })
                                    if name == "unit"
                            );
                            if !suppress && !out.value.is_empty() {
                                let text = if finished_return_type.is_some() {
                                    out.value.clone()
                                } else {
                                    pretty_kv_list(&out.value)
                                        .unwrap_or_else(|| out.value.clone())
                                };
                                self.push(ScrollbackKind::Result, text);
                            }
                            if let Some(batch) = self.pending_batch.take() {
                                self.session.commit(batch);
                            }
                            self.worker_state = WorkerState::Ready;
                        }
                    }
                    Err(err) => {
                        self.worker_state = WorkerState::Ready;
                        // Hold the pending batch for the renderer
                        // — drill-down lookups need its proc map.
                        // It's cleared below once the trace is
                        // emitted (a pending batch only outlives a
                        // single result event).
                        render_eval_error(self, &origin, err);
                        self.pending_batch = None;
                    }
                }
            }
        }
    }

    pub(crate) fn push(&mut self, kind: ScrollbackKind, text: String) {
        // O(1). The tail-follow pin happens in the renderer (which
        // already knows the wrapped-row total for free), not here —
        // doing it per-push was O(N) per call, making a long burst
        // of Vivado stream chunks O(N²) and freezing the REPL for
        // minutes during `src @vivado-cmd` style fan-outs.
        self.scrollback.push(ScrollbackEntry { kind, text });
    }

    /// Apply a signed scroll delta (positive = down toward newer
    /// content, negative = up toward older content). Disengages
    /// tail-follow when the user scrolls up; re-engages when
    /// they scroll down past the bottom — same semantics as a
    /// scroll-wheel in a terminal emulator.
    ///
    /// Anchors the new offset against `last_rendered_scroll` (set
    /// by the renderer each frame) rather than `scrollback_scroll`,
    /// because while tail-follow is on `scrollback_scroll` is stale
    /// — the renderer computes the effective bottom-aligned offset
    /// without writing it back to that field. Starting the manual
    /// delta from the rendered offset is what lets Ctrl-K from
    /// tail-follow mode actually move up by 5 instead of jumping
    /// to position 5.
    fn scroll_by(&mut self, delta: i32) {
        let base = self.last_rendered_scroll as i32;
        let new = base.saturating_add(delta).max(0) as u16;
        if delta < 0 {
            self.scrollback_follow = false;
        }
        self.scrollback_scroll = new;
        // Re-engage tail-follow once the user scrolls back down to
        // the bottom — clamped by the renderer next frame.
        if self.scrollback_follow_threshold_reached(new) {
            self.scrollback_follow = true;
        }
    }

    fn scrollback_follow_threshold_reached(&self, offset: u16) -> bool {
        let Some(area) = self.scrollback_area else {
            return false;
        };
        // Cheap upper bound — we don't recompute wrapped rows here.
        // If `offset` is bigger than the line count of scrollback
        // (i.e. past the last source line, even before wrapping
        // expands them), the user has definitely scrolled past the
        // bottom; flip follow back on.
        let upper = self
            .scrollback
            .iter()
            .map(|e| e.text.lines().count().max(1))
            .sum::<usize>()
            .saturating_sub(area.height as usize) as u16;
        offset >= upper
    }
}

// ---------------------------------------------------------------------
// Worker task: owns the Vivado backend, serializes evals.
// ---------------------------------------------------------------------

async fn worker_task(
    mut rx: mpsc::Receiver<WorkerCmd>,
    tx: mpsc::UnboundedSender<WorkerEvent>,
    verbose: bool,
    verbose_log: Option<std::path::PathBuf>,
) {
    let backend = vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
        verbose,
        verbose_log,
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

    // Stream chunks to the UI as they arrive. The closure
    // captures the unbounded sender so it can fire without
    // awaiting. The kind tag (`StreamKind::Stdout` for user `puts`
    // output, `Warning`/`Error`/`Info` for Vivado's own message
    // lines harvested from the PTY) flows through unchanged so
    // the UI can colour them appropriately.
    let stdout_tx = tx.clone();
    backend.set_stdout_sink(move |kind, chunk: &str| {
        let _ = stdout_tx.send(WorkerEvent::Stream {
            kind,
            data: chunk.to_string(),
        });
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
            app.push(ScrollbackKind::Error, format!("{other}"));
            return;
        }
    };
    if let Some(info) = info.as_deref() {
        for tcl_frame in parse_tcl_proc_frames(info) {
            // Check the in-flight batch first (the lowering that
            // just ran), then fall back to prior session batches.
            // This is what gives wrappers declared in earlier
            // inputs a real `.htcl` path in the drill-down trace
            // instead of an `(input):N` line in a vanished scratch.
            let loc = app
                .pending_batch
                .as_ref()
                .and_then(|b| b.procs.get(&tcl_frame.proc))
                .or_else(|| app.session.lookup_proc(&tcl_frame.proc));
            let Some(loc) = loc else { continue };
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
        let Some(num) = rest.strip_suffix(')') else {
            continue;
        };
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

/// Rewrite `<input>:N in ::procname` frames in a Vivado message to
/// point at the actual htcl source file and line. The shim appends
/// a stack trace below WARNING / ERROR lines with one
/// `  at <location> in <proc>` entry per frame; when the proc was
/// declared in user htcl we know its body's absolute
/// `(file, body_start_line)`, so we can map the `<input>:body-line`
/// Tcl reported back to a concrete file location. Frames we can't
/// resolve (Vivado builtins, anonymous `uplevel`, etc.) pass through
/// unchanged.
///
/// Also folds consecutive frames pointing at the same proc into a
/// single entry — Tcl reports the proc-decl line AND the in-body
/// call line as separate frames, but they're the same call from
/// the user's perspective.
fn resolve_stack_frames(
    msg: &str,
    session: &Session,
    pending: Option<&SessionBatch>,
    input_file: Option<&std::path::Path>,
) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut last_resolved_key: Option<(String, u32)> = None;
    for (i, line) in msg.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let Some(rewritten) =
            rewrite_stack_line(line, session, pending, input_file)
        else {
            out.push_str(line);
            last_resolved_key = None;
            continue;
        };
        let key = (rewritten.proc.clone(), rewritten.line);
        if last_resolved_key.as_ref() == Some(&key) {
            if out.ends_with('\n') {
                out.pop();
            }
            continue;
        }
        last_resolved_key = Some(key);
        out.push_str(&rewritten.formatted);
    }
    out
}

/// One stack-frame line in a Vivado message, after we've mapped
/// `<input>:body-line in ::procname` back to the absolute htcl
/// `(file, line)` of that proc declaration.
struct RewrittenFrame {
    proc: String,
    line: u32,
    formatted: String,
}

/// Parse a single line like `  at <input>:14 in ::configure_cips`
/// and rewrite it to point at the user's actual htcl source.
/// Returns `None` when the line isn't a stack frame in that shape
/// (just regular message text) or when the proc isn't one we know
/// about (Vivado builtins, dynamic procs, etc.) — caller passes
/// such lines through unchanged.
fn rewrite_stack_line(
    line: &str,
    session: &Session,
    pending: Option<&SessionBatch>,
    input_file: Option<&std::path::Path>,
) -> Option<RewrittenFrame> {
    // Grammar emitted by `vw::format_frame`:
    //   "  at <input>:N in ::procname"  ← lookup ProcLocation by name
    //   "  at <file>:N in ::procname"   ← already absolute
    //   "  at <file>:N"                 ← anonymous eval / top-level
    //   "  at <procname>"               ← location-less
    let rest = line.strip_prefix("  at ")?;
    // Split into "<location>" and optional " in <proc>" tail.
    let (loc, proc_part) = match rest.split_once(" in ") {
        Some((l, p)) => (l, Some(p.trim().to_string())),
        None => (rest, None),
    };
    let (file_part, line_part) = loc.rsplit_once(':')?;
    let body_line: u32 = line_part.parse().ok()?;

    // Top-level `<input>:N` frame (no proc). In `--load` mode the
    // scratch contains the load file verbatim, so scratch:N maps
    // 1:1 to the user's path — substitute it.
    let Some(proc) = proc_part else {
        if file_part != "<input>" {
            return None;
        }
        let path = input_file?;
        return Some(RewrittenFrame {
            proc: String::new(),
            line: body_line,
            formatted: format!("  at {}:{body_line}", display_path(path)),
        });
    };

    // Already-absolute frames don't need rewriting, but we still
    // want them deduped — return them with the parsed proc/line.
    if file_part != "<input>" {
        return Some(RewrittenFrame {
            proc,
            line: body_line,
            formatted: line.to_string(),
        });
    }
    // `<input>:N` with a proc — Tcl reports "line N of the proc
    // body." Resolve through the proc table. Tcl always reports
    // fully-qualified names (leading `::`); the proc table indexes
    // them without (see `lower::qualify`), so strip before lookup.
    let lookup_name = proc.strip_prefix("::").unwrap_or(&proc);
    let loc = pending
        .and_then(|b| b.procs.get(lookup_name))
        .or_else(|| session.lookup_proc(lookup_name))?;
    let (abs_line, _content) = loc.resolve_body_line(body_line)?;
    let path_str = match loc.file.as_deref() {
        Some(p) => display_path(p),
        None => match input_file {
            Some(p) => display_path(p),
            None => "<input>".to_string(),
        },
    };
    Some(RewrittenFrame {
        proc: proc.clone(),
        line: abs_line,
        formatted: format!("  at {path_str}:{abs_line} in {proc}"),
    })
}

/// If `text` looks like a Tcl key-value list (an even number of
/// elements where the keys are property-name-shaped), reformat it
/// one pair per line. Returns `None` to mean "leave the original
/// output alone" — the caller falls back to the raw string for
/// scalars, odd-length lists, lists of non-key-shaped tokens, etc.
///
/// We do this on Tcl return values, where `report_property`-style
/// dicts (`KEY1 VAL1 KEY2 VAL2 …`) are common and unreadable as a
/// single wrapped line.
fn pretty_kv_list(text: &str) -> Option<String> {
    let elements = tcl_list_split(text.trim())?;
    // Heuristic: at least 2 pairs, even count, keys look like
    // property names. Two pairs is the minimum where the
    // one-per-line layout actually helps — a single pair is fine
    // as-is.
    if elements.len() < 4 || elements.len() % 2 != 0 {
        return None;
    }
    for chunk in elements.chunks(2) {
        if !is_propname_like(&chunk[0]) {
            return None;
        }
    }
    let mut out = String::with_capacity(text.len() + elements.len());
    for (i, chunk) in elements.chunks(2).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&chunk[0]);
        out.push(' ');
        // Re-brace values that contain whitespace or are empty so
        // the displayed line is itself valid Tcl — the user can
        // copy any line straight back into a `set` / `dict set`
        // call.
        let val = &chunk[1];
        if val.is_empty()
            || val.chars().any(char::is_whitespace)
            || val.contains('"')
        {
            out.push('{');
            out.push_str(val);
            out.push('}');
        } else {
            out.push_str(val);
        }
    }
    Some(out)
}

/// Minimal Tcl-list tokenizer: split on whitespace at the top
/// level, honoring `{…}` grouping with nesting and `\<char>`
/// escapes. Returns `None` on unbalanced braces — caller falls
/// back to the raw string when this happens (better to show
/// something than nothing). Doesn't handle `"…"` grouping because
/// Vivado's list returns never use it; if that changes, add a
/// branch mirroring the brace one.
fn tcl_list_split(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '{' {
            chars.next();
            let mut depth = 1usize;
            let mut buf = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(esc) = chars.next() {
                            buf.push(c);
                            buf.push(esc);
                        }
                    }
                    '{' => {
                        depth += 1;
                        buf.push(c);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        buf.push(c);
                    }
                    _ => buf.push(c),
                }
            }
            if depth != 0 {
                return None;
            }
            out.push(buf);
        } else {
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                if c == '\\' {
                    chars.next();
                    if let Some(esc) = chars.next() {
                        buf.push(esc);
                    }
                    continue;
                }
                buf.push(c);
                chars.next();
            }
            out.push(buf);
        }
    }
    Some(out)
}

/// "Looks like a property name": ASCII alphanumeric with `_`, `.`,
/// `-`. Used by `pretty_kv_list` to filter out lists-of-arbitrary-
/// strings that just happen to be even-length. Empty strings fail
/// (would render `  ` and look broken).
fn is_propname_like(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn render_origin_path(file: Option<&std::path::Path>, line: u32) -> String {
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
/// Write an OSC 52 set-clipboard escape to stdout, base64-encoding
/// `text` per the protocol. The terminal puts the decoded text on
/// the system clipboard — no DISPLAY/Wayland-socket/pbcopy
/// dependency, and the same code path works over SSH.
///
/// Some terminals cap the payload size at ~74KB (the original xterm
/// limit) or somewhere similar; selections larger than that may be
/// truncated by the terminal. Encoding/IO errors are swallowed —
/// the caller has nowhere useful to surface them, since OSC 52 is
/// fire-and-forget (the terminal doesn't ack).
fn send_osc52(text: &str) {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use std::io::Write;
    let encoded = STANDARD.encode(text.as_bytes());
    let payload = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(payload.as_bytes());
    let _ = stdout.flush();
}

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

    // --- stack-frame resolution ---------------------------------

    use crate::lower::ProcLocation;
    use crate::session::SessionBatch;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use vw_htcl::{parse, LoadedProgram};

    fn session_with_proc(
        proc: &str,
        file: PathBuf,
        body_start_line: u32,
        body_lines: Vec<String>,
    ) -> Session {
        // Session stores proc names without the leading `::` —
        // see `lower::qualify`.
        let key = proc.strip_prefix("::").unwrap_or(proc);
        let src = format!("proc {key} {{}} {{}}\n");
        let parsed = parse(&src);
        let mut procs = HashMap::new();
        procs.insert(
            key.to_string(),
            ProcLocation {
                file: Some(file),
                body_start_line,
                body_lines,
            },
        );
        let batch = SessionBatch {
            program: LoadedProgram {
                source: src,
                files: Vec::new(),
                regions: Vec::new(),
            },
            document: parsed.document,
            procs,
        };
        let mut s = Session::new();
        s.commit(batch);
        s
    }

    #[test]
    fn rewrite_resolves_input_line_to_absolute_file_line() {
        let session = session_with_proc(
            "::configure_cips",
            "ip/cips.htcl".into(),
            95,
            (0..30).map(|i| format!("body line {i}")).collect(),
        );
        let frame = rewrite_stack_line(
            "  at <input>:14 in ::configure_cips",
            &session,
            None,
            None,
        )
        .expect("should resolve");
        // body line 14 = body_start_line (95) + (14 - 1) = 108
        assert!(
            frame.formatted.contains("ip/cips.htcl:108"),
            "got {:?}",
            frame.formatted
        );
        assert_eq!(frame.line, 108);
    }

    #[test]
    fn rewrite_resolves_namespaced_proc() {
        // Tcl reports `::port::plumb_if_pin` (with leading `::`)
        // but the session indexes it as `port::plumb_if_pin`.
        let session = session_with_proc(
            "::port::plumb_if_pin",
            "vivado-cmd/port.htcl".into(),
            70,
            (0..10).map(|i| format!("line {i}")).collect(),
        );
        let frame = rewrite_stack_line(
            "  at <input>:5 in ::port::plumb_if_pin",
            &session,
            None,
            None,
        )
        .expect("should resolve namespaced proc");
        assert!(
            frame.formatted.contains("vivado-cmd/port.htcl:74"),
            "got {:?}",
            frame.formatted
        );
    }

    #[test]
    fn rewrite_passes_unknown_proc_through() {
        let session = Session::new();
        assert!(rewrite_stack_line(
            "  at <input>:14 in ::vivado_builtin_thing",
            &session,
            None,
            None,
        )
        .is_none());
    }

    #[test]
    fn rewrite_skips_non_frame_lines() {
        let session = Session::new();
        assert!(rewrite_stack_line(
            "WARNING: [Common 17-1] something",
            &session,
            None,
            None,
        )
        .is_none());
        assert!(rewrite_stack_line("", &session, None, None).is_none());
    }

    #[test]
    fn resolve_dedupes_adjacent_same_proc_frames() {
        // Two consecutive `<input>:N in ::port::plumb_if_pin` frames
        // resolving to the same absolute line should collapse to one.
        let session = session_with_proc(
            "::port::plumb_if_pin",
            "vivado-cmd/port.htcl".into(),
            70,
            (0..10).map(|i| format!("line {i}")).collect(),
        );
        let msg = "\
WARNING: [port::plumb_if_pin-1] skipping foo
  at <input>:5 in ::port::plumb_if_pin
  at <input>:5 in ::port::plumb_if_pin";
        let out = resolve_stack_frames(msg, &session, None, None);
        // Only one resolved frame line should remain.
        let count = out
            .lines()
            .filter(|l| l.contains("port::plumb_if_pin"))
            .count();
        assert_eq!(count, 2, "got:\n{out}"); // header + 1 frame
    }

    // --- pretty kv list -----------------------------------------

    #[test]
    fn tcl_list_split_handles_braces_and_nesting() {
        assert_eq!(
            tcl_list_split("a b c d").unwrap(),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(
            tcl_list_split("KEY {nested value} OTHER 1").unwrap(),
            vec!["KEY", "nested value", "OTHER", "1"]
        );
        assert_eq!(
            tcl_list_split("OUTER {INNER {DEEP value}} END 2").unwrap(),
            vec!["OUTER", "INNER {DEEP value}", "END", "2"]
        );
        // Unbalanced braces → None.
        assert!(tcl_list_split("a {b c").is_none());
    }

    #[test]
    fn pretty_kv_list_breaks_pairs_onto_lines() {
        let s = "CLASS bd_cell NAME cips PATH /cips";
        let out = pretty_kv_list(s).unwrap();
        assert_eq!(out, "CLASS bd_cell\nNAME cips\nPATH /cips");
    }

    #[test]
    fn pretty_kv_list_rebraces_values_with_whitespace() {
        let s = "ALLOWED_SIM_MODELS {tlm rtl} CLASS bd_cell COMBINED rtl_tlm";
        let out = pretty_kv_list(s).unwrap();
        assert_eq!(
            out,
            "ALLOWED_SIM_MODELS {tlm rtl}\nCLASS bd_cell\nCOMBINED rtl_tlm"
        );
    }

    #[test]
    fn pretty_kv_list_declines_non_kv_lists() {
        // Odd-length: not a dict.
        assert!(pretty_kv_list("a b c").is_none());
        // Two elements: declined (single pair gains nothing from
        // reflow).
        assert!(pretty_kv_list("a b").is_none());
        // Non-propname keys: looks more like prose than a dict.
        assert!(pretty_kv_list("hello world foo bar").is_some());
        // … but the same elements with one non-propname key fail.
        assert!(pretty_kv_list("hello world foo! bar").is_none());
    }
}
