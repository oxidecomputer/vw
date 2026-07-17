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
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

use crate::config::{self, CollapseMode};
use crate::history::History;
use crate::lower::Origin;
use crate::session::{Session, SessionBatch};
use crate::ui::{self, WorkerStatusView};
use crate::{ReplError, ReplOptions};

/// A multi-line entry with more than this many source lines
/// auto-collapses into a `▶`-marked placeholder at push time,
/// Mathematica-notebook style. Shift-click still toggles the
/// state, so the raw content is one gesture away — the threshold
/// just keeps a wall of text from dominating scrollback when a
/// large `list<T>` or `Properties` value comes back. Chosen at 100
/// because that's roughly the transition point where a viewport-
/// filling block stops being scannable and starts being intrusive.
pub const COLLAPSE_AUTO_THRESHOLD: usize = 100;

/// Decide the initial [`ScrollbackEntry::collapse_state`] for an
/// entry with `text`. Every multi-line entry gets a `Some(bool)`
/// so it's toggleable via Shift+click; single-line entries get
/// `None` because a placeholder around one row of content adds
/// affordance without meaningful eliding. Above the auto-threshold
/// the initial state is collapsed.
fn compute_collapse_state(text: &str, mode: CollapseMode) -> Option<bool> {
    let lines = text.lines().count();
    if lines < 2 {
        return None;
    }
    match mode {
        // Aggressive: every multi-line entry starts collapsed —
        // `▶`-marked placeholders that expand on demand. Turns
        // scrollback into a compact index. See
        // [`crate::config::CollapseMode`].
        CollapseMode::Aggressive => Some(true),
        // Normal: only wall-of-text entries auto-collapse; smaller
        // multi-line output stays inline for at-a-glance scanning.
        CollapseMode::Normal => Some(lines > COLLAPSE_AUTO_THRESHOLD),
    }
}

/// Split a tagged-diagnostic message into `(leading, trailing)`.
///
/// A "tagged diagnostic line" has the shape
/// `<LEVEL>: [<DESIGNATOR>] <MESSAGE>` — e.g.
/// `ERROR: [Common 17-107] Cannot change read-only property …`
/// or `CRITICAL WARNING: [Project 1-486] Could not resolve …`.
/// Vivado (and our own eval-error renderer) sometimes appends
/// wrapped continuations, a `    Resolution: …` hint, or a
/// backtrace on the following lines — all valuable, but bulky.
///
/// `leading` is the tagged first line (returned even when the
/// message is a single line, in which case `trailing` is `None`).
/// `trailing` is everything after the first line, trailing
/// newlines stripped; `None` when the message is a single line
/// or the tail is whitespace-only.
///
/// Downstream, `leading` pushes as its OWN scrollback entry so
/// it stays at full brightness — one-liner, non-collapsible,
/// eye-catching gutter — while `trailing` pushes separately and
/// can auto-collapse / dim like any other multi-line entry.
/// This is the fix for the "critical error line got dimmed and
/// blended into the surrounding chatter" bug.
fn split_leading_diagnostic(text: &str) -> (String, Option<String>) {
    match text.split_once('\n') {
        Some((head, tail)) => {
            let tail = tail.trim_end_matches('\n');
            if tail.trim().is_empty() {
                (head.to_string(), None)
            } else {
                (head.to_string(), Some(tail.to_string()))
            }
        }
        None => (text.to_string(), None),
    }
}

/// Split a diagnostic's rendered text into `(body, stack)` at the
/// first line that looks like a stack frame — `  at <path>:<line>`.
/// The body is everything before that line (message + any wrapped
/// continuations); the stack is that line and everything after,
/// trailing newline stripped. `None` for stack means the diagnostic
/// carried no frames (traceless INFOs, some plain WARNINGs).
///
/// The two-space indent + literal `at ` prefix is the shape
/// [`install_proc_body_wrap`] attaches in `vivado-shim.tcl` — see
/// its `format_stack` helper. If that format ever drifts, both
/// sides must stay in sync.
fn split_body_and_stack(text: &str) -> (String, Option<String>) {
    let needle = "\n  at ";
    match text.find(needle) {
        // The `\n` at `idx` closes the body line; the stack begins
        // at `idx + 1` so the `  at ` prefix is preserved on the
        // first stack frame.
        Some(idx) => {
            let body = text[..idx].to_string();
            let stack = text[idx + 1..].trim_end_matches('\n').to_string();
            if stack.is_empty() {
                (body, None)
            } else {
                (body, Some(stack))
            }
        }
        None => (text.to_string(), None),
    }
}

/// What category an entry in the scrollback log belongs to. Drives
/// the per-line gutter prefix and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbackKind {
    /// Echo of an input the user submitted.
    Input,
    /// A return value from a successful eval.
    Result,
    /// Captured stdout from `puts` etc. during an eval.
    Stdout,
    /// An error — TCL-level or REPL-level. Also the visual bucket
    /// for Vivado CRITICAL WARNINGs, which the block classifier
    /// keeps semantically distinct for log-level filtering but
    /// which render with the same red ✗ treatment (per the
    /// stream handler's Severity match). Entries in this kind
    /// that came from a CW are additionally flagged via
    /// [`ScrollbackEntry::is_critical_warning`] so the
    /// diagnostics finder can offer a separate `Critical` filter
    /// checkbox on top of the Error filter.
    Error,
    /// A pre-flight warning the user should see before the
    /// underlying eval result — e.g. "this call uses keyword args
    /// but isn't a loaded htcl wrapper." Distinct color from
    /// notices so it actually pulls the eye.
    Warning,
    /// Internal notice (`vivado: ready`, `:load`, `:restart`, etc.).
    Notice,
    /// Non-diagnostic Vivado chatter that landed as its own single-
    /// line NONE block — the `----` divider after an INFO,
    /// `Attempting to get a license…` status echoes, single-line
    /// section banners. Rendered dimmed dark-gray with a plain
    /// `  ` prefix so it reads as "background noise" without
    /// pretending to be an INFO (which would carry `· `) or a
    /// user-facing Stdout entry (which would be bright white).
    /// Multi-line NONE blocks go through the collapsible path
    /// instead — see `push_none_block`.
    Chatter,
}

/// Tracks where each echoed top-level statement's Input entry
/// lives in scrollback AND which lowered command-index in the
/// batch is its last. When that command finishes evaluating, the
/// Input entry's timer freezes — giving accurate per-statement
/// durations in multi-statement load batches instead of all
/// entries sharing the whole-batch wall time.
#[derive(Clone, Debug)]
struct InputBoundary {
    /// Position in `scrollback` where this boundary's echo lives,
    /// or `None` when the echo is still queued. Non-first entries
    /// start `None` and get pushed by `advance_input_timers` when
    /// the prior boundary closes — so a `:load` batch prints
    /// linearly: command, its output, then the *next* command.
    scrollback_idx: Option<usize>,
    /// Snippet captured up front so the deferred push has the
    /// exact text `dispatch_eval_with_echo` would have used
    /// eagerly.
    snippet: String,
    /// Eval-index in `pending_origins` of the last lowered
    /// command that originated from this top-level statement.
    /// When `pending_eval_index` reaches this value (i.e. the
    /// command at this index has just finished), the entry's
    /// timer should freeze. `None` when no lowered command was
    /// attributed to this boundary — e.g. a `src` whose target
    /// file lowered to zero Tcl commands. Such boundaries are
    /// skipped when the prior boundary closes: nothing to wait
    /// for and nothing to echo.
    last_command_idx: Option<usize>,
    /// Set to true once we've stamped this entry's `completed_at`,
    /// so we don't re-stamp on subsequent EvalDones.
    completed: bool,
}

#[derive(Clone, Debug)]
pub struct ScrollbackEntry {
    pub kind: ScrollbackKind,
    pub text: String,
    /// When this entry was pushed. Only set for `Input` entries
    /// — used by the renderer to right-justify a `Ns` /
    /// `M:SS` / `H:MM:SS` elapsed-time marker on the first
    /// line. Non-input entries don't get timed and leave this
    /// `None`.
    pub started_at: Option<std::time::Instant>,
    /// When the corresponding eval finished. `None` while the
    /// eval is still running (renderer shows live-updating
    /// elapsed time from `started_at`); `Some(t)` freezes the
    /// timer at the final duration once the batch completes.
    pub completed_at: Option<std::time::Instant>,
    /// Non-`None` marks this entry as a collapsible NONE-severity
    /// block (Vivado's non-diagnostic output: tables, banners,
    /// section headers). `Some(true)` = collapsed (renderer shows
    /// a single `▶` placeholder with a preview + hidden-line
    /// count); `Some(false)` = expanded (all lines render dimmed
    /// with a `▼` marker on the first line). `None` = normal
    /// entry, no collapse handling.
    ///
    /// Only NONE blocks get this treatment — diagnostics are
    /// always shown at full fidelity so a user scanning
    /// scrollback for a WARNING never has to expand anything to
    /// find it.
    pub collapse_state: Option<bool>,
    /// True when this entry originated from a Vivado CRITICAL
    /// WARNING (not a plain ERROR). The two share
    /// [`ScrollbackKind::Error`] for rendering — same red gutter
    /// — but the diagnostics finder uses this flag to let the
    /// user filter Critical warnings independently of plain
    /// Errors. Default `false` for entries pushed by paths that
    /// don't know severity (Input echoes, Result returns,
    /// synthetic Notices).
    pub is_critical_warning: bool,
    /// Index in `scrollback` of the [`ScrollbackKind::Input`]
    /// entry this row belongs to, or `None` when the row
    /// predates any input (startup notices) or IS itself an
    /// Input. Set once at push time — the "current input" is
    /// the last `Input` pushed, and every subsequent
    /// non-`Input` entry inherits that idx as its parent.
    ///
    /// Drives the Mathematica-style "collapse everything under
    /// this command" grouping. Renderer skips rows whose
    /// parent's `group_collapsed` is `true`; diagnostic search
    /// uses this to group results by command AND to expand the
    /// parent group when the user jumps to a hidden result.
    pub parent_input_idx: Option<usize>,
    /// Only meaningful for [`ScrollbackKind::Input`] entries.
    /// When `true`, every subsequent entry whose
    /// `parent_input_idx` points at this input is hidden from
    /// the renderer and skipped by mouse / diagnostic-jump
    /// math. Toggled by Shift-click on the Input row itself.
    /// Defaults to `false` — a freshly-pushed command shows its
    /// output live.
    pub group_collapsed: bool,
    /// Only meaningful for [`ScrollbackKind::Input`] entries.
    /// Number of `Error` / `CriticalWarning` children currently
    /// attributed to this input's group. Renderer shows a red
    /// `✗` badge on the collapsed Input row when this is >0,
    /// so users can see something went wrong without expanding.
    /// Bumped by `push` / `push_diag` when they attribute a
    /// child to a parent input; never decremented (a scrollback
    /// entry is never re-classified after push).
    pub error_child_count: u32,
    /// Only meaningful for [`ScrollbackKind::Input`] entries.
    /// Sibling of [`error_child_count`] for `Warning` children.
    /// Renderer shows an orange `⚠` badge — same glyph and
    /// color as the [`ScrollbackKind::Warning`] gutter — on
    /// the collapsed Input row when this is >0, so users can
    /// see there are non-fatal issues without expanding.
    pub warning_child_count: u32,
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

/// REPL meta-command catalog. Each entry is `(label, hint)` where
/// `label` is the full `:command` token (including the leading
/// colon, since that's what the user types and what Tab-completion
/// replaces) and `hint` is a one-line description shown in the
/// completion popup.
///
/// Keep in sync with the `match` in [`App::run_meta_command`] and
/// with the cheat-sheet rows in [`crate::popup::HELP_ROWS`].
pub const META_COMMANDS: &[(&str, &str)] = &[
    (":load", "evaluate a file's contents in this session"),
    (":libs", "list loaded libraries + symbol counts"),
    (":quit", "exit the REPL"),
    (":exit", "exit the REPL (alias of :quit)"),
    (":q", "exit the REPL (alias of :quit)"),
    (
        ":restart",
        "restart the Vivado worker (not yet implemented)",
    ),
];

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
    /// Session state, shareable across threads so background
    /// prepare tasks (`dispatch_eval_with_echo`) can hold a read
    /// guard for the ~seconds-to-minutes of a large `src` import
    /// while the main event loop's frequent lookups (Ctrl-P
    /// history walk, tab-completion, signature-help refresh,
    /// input-completeness check) grab their own concurrent read
    /// guards without contention.
    ///
    /// `RwLock` — not `Mutex` — because the main thread's typing-
    /// time reads MUST run in parallel with a long-running
    /// background prepare's read. A single writer (`commit` on a
    /// successful prepare) briefly acquires the write lock; that
    /// happens on the main task after the prepare returns, so
    /// there's no active reader to wait for.
    session: std::sync::Arc<std::sync::RwLock<Session>>,
    /// Shared "already loaded in Vivado" map. Handed to the RPC
    /// handler at spawn time; refreshed from
    /// `session.loaded_paths()` after every `commit` so
    /// `compile_htcl_module` skips re-shipping files whose
    /// procs are already installed. Correctness invariant lives
    /// with [`vw_vivado::SharedPreload`].
    preload: vw_vivado::SharedPreload,
    scrollback: Vec<ScrollbackEntry>,
    /// Segments Vivado's classified stream into per-chunk Diagnostic
    /// entries and grouped NONE-block collapsibles before pushing
    /// into scrollback. Same accumulator vw-cli uses, but here it
    /// lives on `App` because Stream events flow through the main
    /// task's single event loop — no cross-thread sharing needed.
    /// Diagnostic blocks and NONE blocks land in scrollback via
    /// [`Self::push`] and [`Self::push_none_block`] respectively.
    block_acc: vw_vivado::BlockAccumulator,
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
    /// The `max_scroll` (= wrapped rows − viewport height) the
    /// renderer computed on the most recent frame. Written by
    /// `ui::draw_scrollback`; consulted by [`Self::scroll_by`] so a
    /// downward scroll that lands at (or past) the bottom re-engages
    /// tail-follow. This is safe now that the wrapped-row count is
    /// exact — the old auto-re-engage misfired on large output
    /// because it compared raw `text.lines().count()` against a
    /// heuristic threshold.
    last_max_scroll: u16,
    /// Scrollback entry index the user last jumped to via the
    /// diagnostic-finder popup (Ctrl-F → Enter). While `Some`, the
    /// renderer paints a persistent left-gutter marker on every
    /// wrapped row of that entry so the user can spot it in a
    /// busy log. Alt-C clears it. `None` on startup and after
    /// clear; not affected by scrolling or new entries appending.
    marker_entry: Option<usize>,
    /// One-shot request from the popup layer to scroll the
    /// specified scrollback entry into view on the next frame.
    /// Consumed by `ui::draw_scrollback` — it computes the
    /// wrapped-row offset of that entry (from the same per-entry
    /// count pass it already does) and writes it into
    /// `scrollback_scroll` + disengages `scrollback_follow`.
    /// Kept as a `usize` scrollback-idx rather than a pre-computed
    /// row offset because the offset depends on area.width, which
    /// the popup key handler doesn't know.
    pending_jump: Option<usize>,
    reverse_search: Option<ReverseSearch>,
    /// Active LSP-style popup over the input editor (completion,
    /// signature help, hover). When `Some`, the key handler routes
    /// navigation / dismissal keys to the popup BEFORE the catch-all
    /// editor handoff. See [`crate::popup`].
    popup: Option<crate::popup::PopupState>,
    worker_state: WorkerState,
    /// Vivado child's OS pid, cached from `WorkerEvent::Started`.
    /// Consulted by the Ctrl-C handler to send SIGINT for eval
    /// cancellation without going through `worker_tx` (which is
    /// blocked by the in-flight `backend.eval` we'd be trying to
    /// cancel). `None` before Started, or when the PTY layer
    /// couldn't report a pid — in either case Ctrl-C falls back
    /// to its non-eval behavior (clear the current input).
    child_pid: Option<u32>,
    worker_tx: mpsc::Sender<WorkerCmd>,
    eval_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    /// Sender-side of the worker-event channel, kept on App so
    /// spawned background tasks (currently: `prepare` on a
    /// blocking thread) can post their completion back to the
    /// event loop without a separate channel. The receiver
    /// (`eval_rx`) is drained inside the main select loop.
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
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
    /// Parallel to `pending_return_types`, one entry per lowered
    /// command. True when the command is a top-level `set VAR
    /// <expr>` — the app suppresses the Result echo for those:
    /// binding is a plumbing operation, not a display, and
    /// echoing the raw value can leak the internal tagged Tcl
    /// list form (`{Scalar x}` rather than the parens repr).
    pending_is_set_binding: Vec<bool>,
    /// For per-Input-entry timer freezing: one entry per
    /// echoed top-level statement in the current batch, in
    /// source order. Each carries the scrollback index of its
    /// Input entry and the eval-index of the LAST lowered
    /// command that came from that statement. When EvalDone
    /// fires for that command index, we freeze the entry's
    /// timer AND start the next entry's timer (so per-statement
    /// durations in a multi-statement load batch are accurate
    /// instead of all reading the whole-batch wall time).
    pending_input_boundaries: Vec<InputBoundary>,
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
    /// Auto-collapse policy for multi-line scrollback entries.
    /// Loaded from `<workspace>/.vw/repl.toml` at startup and
    /// consulted by [`compute_collapse_state`] on every `push`.
    collapse_mode: CollapseMode,
    /// Index in `scrollback` of the most recently pushed
    /// [`ScrollbackKind::Input`] entry, or `None` when no user
    /// input has been submitted yet (startup notices only).
    /// Every non-Input push after an Input records this index
    /// as its `parent_input_idx`, forming a
    /// Mathematica-notebook-style group under each command.
    /// Renderer + click handler use these parent pointers to
    /// hide / reveal a whole group as a unit.
    current_input_idx: Option<usize>,
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
    /// Vivado spawn succeeded. Carries the child's OS pid so the
    /// UI can send SIGINT for eval cancellation without going
    /// through the worker channel (which is blocked by the
    /// in-flight eval it would need to cancel). `None` when the
    /// PTY layer couldn't report a pid — cancellation degrades
    /// to a no-op in that case.
    Started {
        child_pid: Option<u32>,
    },
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
    /// The background `prepare` (parse + validate + lower)
    /// finished. `dispatch_eval_with_echo` spawns prepare on a
    /// blocking thread so the event loop can render the input
    /// echo + tick the timer while the (potentially minute-scale)
    /// work runs. When this arrives, `handle_prepare_done` takes
    /// over: surfaces warnings, commits or ships to the worker.
    ///
    /// `text` and `echo` are threaded through so the completion
    /// handler has everything it needs without re-consulting the
    /// input state (which may have moved on).
    PrepareDone {
        text: String,
        echo: bool,
        result: Result<crate::lower::Prepared, crate::lower::LowerError>,
    },
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
    // Bracketed paste — the terminal wraps pasted content in
    // sentinel byte sequences so we can distinguish it from
    // manually-typed input. Without this, a pasted multi-line
    // block delivers embedded `\n`s as raw Enter events, each of
    // which the app treats as "submit" — the exact bug this
    // enables us to fix.
    stdout.execute(EnableBracketedPaste)?;
    // Kitty keyboard protocol (minimal set) — asks the terminal
    // to disambiguate keys that would otherwise collide (Ctrl+I
    // vs. Tab, etc.). Doesn't help with Shift+Enter — this
    // terminal (and many others) sends Shift+Enter as Ctrl+J
    // instead of a distinct CSI-u sequence, so the outer key
    // handler binds Ctrl+J to `insert_newline`. Unsupported
    // flags are ignored, so this is safe everywhere.
    let _ = stdout.execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    ));
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal, opts).await;

    disable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // No-op if capture was already disabled via F2.
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.execute(DisableBracketedPaste);
    let _ = stdout.execute(PopKeyboardEnhancementFlags);
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
    let info_with_stack = opts.info_with_stack;
    // At `--log-level=debug` the user wants the unclassified PTY
    // firehose (banners, source echo, idle chatter) too; anything
    // less filters it out. The firehose can't go to stderr under
    // the TUI's alternate screen — that fd is the render surface —
    // so route it to a per-process tempfile the user can `tail -f`
    // from another terminal. Distinct from the raw byte-log wired
    // downstream: this file holds only the leftover unclassified
    // lines, whereas the raw log holds every byte.
    let verbose = matches!(opts.log_level, vw_vivado::LogLevel::Debug);
    let verbose_log_path = if verbose {
        Some(
            std::env::temp_dir()
                .join(format!("vw-repl-vivado-{}.log", std::process::id())),
        )
    } else {
        None
    };
    // Clone before moving into worker_task — App keeps its own
    // handle so background prepare tasks can post PrepareDone
    // events back onto the same channel the worker uses.
    let event_tx_for_app = event_tx.clone();
    // Workspace-root discovery mirrors `vw run` / `vw check`:
    // walk up from the initial-load file if provided, else from
    // the current cwd, looking for the nearest `vw.toml`. Used
    // to answer the htcl `vw::workspace_root` RPC — served
    // through the Vivado shim's rpc_call primitive at eval time.
    let rpc_workspace_root: Option<std::path::PathBuf> = {
        let start_dir = opts
            .initial_load
            .as_ref()
            .and_then(|p| {
                p.as_std_path().parent().map(std::path::Path::to_path_buf)
            })
            .or_else(|| std::env::current_dir().ok());
        start_dir
            .and_then(|d| vw_lib::find_workspace_dir(&d))
            .map(|p| p.into_std_path_buf())
    };
    // Load `<ws>/.vw/repl.toml` — currently just the `[ui] collapse`
    // knob. Absent / malformed / no-workspace all fall back to
    // defaults; a config file is optional infrastructure, not a
    // startup dependency.
    let repl_config = config::load(rpc_workspace_root.as_deref());
    // Shared preload map — grows as batches commit. Both the RPC
    // handler (inside worker_task) and App hold clones of the same
    // Arc so `compile_htcl_module` sees loaded-file updates as
    // soon as `App::sync_preload_from_session` publishes them.
    // See `vw_vivado::SharedPreload` for the correctness invariant.
    let preload: vw_vivado::SharedPreload = std::sync::Arc::new(
        std::sync::RwLock::new(std::collections::HashMap::new()),
    );
    tokio::spawn(worker_task(
        worker_rx,
        event_tx,
        verbose,
        verbose_log_path.clone(),
        info_with_stack,
        opts.part.clone(),
        opts.variant.clone(),
        rpc_workspace_root,
        preload.clone(),
    ));

    let mut app = App::new(
        opts,
        worker_tx,
        eval_rx,
        event_tx_for_app,
        repl_config.ui.collapse,
        preload,
    );
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
            // Bias toward user input over worker events. Without
            // this, a streaming eval (hundreds of stream chunks per
            // second from Vivado) drowns out individual keystrokes:
            // tokio::select! picks randomly when multiple branches
            // are ready, and the worker channel is ready far more
            // often. Result: Tab during eval looks dead because the
            // keypress queues behind a long run of worker events.
            // Biased order guarantees a ready crossterm event always
            // wins, then the drain phase below catches up on worker
            // events before the next draw.
            biased;
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
        // Drain additional pending events from BOTH streams
        // before the next draw. Without this, a burst of N
        // mouse-wheel events or worker stream chunks each
        // triggers its own draw — even though only the final
        // state matters visually — and the queue bloats faster
        // than draws can keep up.
        //
        // The biased `select!` plus a wildcard always-ready
        // branch acts as a non-blocking "is anything pending?"
        // check: if neither real branch is immediately ready,
        // the wildcard wins and we break out. Capped at 256
        // events per cycle so a sustained event firehose still
        // yields back to drawing periodically (the user sees
        // forward progress instead of "frozen until the whole
        // burst is processed").
        for _ in 0..256 {
            let made_progress = tokio::select! {
                biased;
                Some(maybe_event) = crossterm_events.next() => {
                    match maybe_event {
                        Ok(ev) => app.handle_terminal_event(ev).await,
                        Err(e) => app.push(
                            ScrollbackKind::Error,
                            format!("terminal: {e}"),
                        ),
                    }
                    true
                }
                Some(event) = app.eval_rx.recv() => {
                    app.handle_worker_event(event).await;
                    true
                }
                _ = std::future::ready(()) => false,
            };
            if !made_progress {
                break;
            }
        }
    }
}

impl App {
    fn new(
        opts: ReplOptions,
        worker_tx: mpsc::Sender<WorkerCmd>,
        eval_rx: mpsc::UnboundedReceiver<WorkerEvent>,
        event_tx: mpsc::UnboundedSender<WorkerEvent>,
        collapse_mode: CollapseMode,
        preload: vw_vivado::SharedPreload,
    ) -> Self {
        let mut input = TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            opts,
            input,
            history: History::load_default(),
            history_cursor: None,
            history_draft: String::new(),
            session: std::sync::Arc::new(
                std::sync::RwLock::new(Session::new()),
            ),
            preload,
            scrollback: Vec::new(),
            block_acc: vw_vivado::BlockAccumulator::new(),
            scrollback_scroll: 0,
            mouse_capture: true,
            scrollback_area: None,
            selection: None,
            scrollback_follow: true,
            last_rendered_scroll: 0,
            last_max_scroll: 0,
            marker_entry: None,
            pending_jump: None,
            reverse_search: None,
            popup: None,
            worker_state: WorkerState::Starting,
            child_pid: None,
            worker_tx,
            eval_rx,
            event_tx,
            pending_batch: None,
            pending_origins: Vec::new(),
            pending_return_types: Vec::new(),
            pending_is_set_binding: Vec::new(),
            pending_input_boundaries: Vec::new(),
            pending_eval_index: 0,
            exit: false,
            collapse_mode,
            current_input_idx: None,
        }
    }

    /// Refresh the shared preload map from the current session's
    /// `loaded_paths()`. Called immediately after every
    /// `session.commit(...)` so the next `compile_htcl_module`
    /// RPC sees the latest set of files installed in the Vivado
    /// interpreter.
    ///
    /// Called AFTER commit (not before / concurrently) so the map
    /// only names files whose lowered Tcl has been eval'd by
    /// Vivado — the safety rule spelled out on
    /// `vw_vivado::SharedPreload`. Wholesale replace (not merge)
    /// so a file removed from the session — e.g. via a hot-edit
    /// path in the future — drops out of the preload set too.
    fn sync_preload_from_session(&self) {
        let paths = {
            let s = self.session.read().unwrap();
            s.loaded_paths()
        };
        if let Ok(mut g) = self.preload.write() {
            *g = paths;
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

    /// Renderer-side writeback for the current frame's `max_scroll`
    /// (wrapped rows − viewport height). Consulted by
    /// [`Self::scroll_by`] so a downward wheel/PageDown that lands
    /// at the bottom re-engages tail-follow.
    pub fn set_last_max_scroll(&mut self, max_scroll: u16) {
        self.last_max_scroll = max_scroll;
    }

    /// Renderer-invoked writeback used by the pending-jump path —
    /// the popup handler doesn't know area.width so it can't
    /// compute the target scroll offset itself, and instead
    /// stashes a `pending_jump` scrollback index that the
    /// renderer translates into an offset and writes back here.
    /// General-purpose scroll changes go through
    /// [`Self::scroll_by`]; this setter is not a substitute.
    pub fn set_scrollback_scroll(&mut self, offset: u16) {
        self.scrollback_scroll = offset;
    }

    /// Scrollback entry index the user last jumped to (or `None`
    /// if the marker has been cleared or was never set). Consulted
    /// by `ui::draw_scrollback` to paint the persistent gutter
    /// marker on that entry's wrapped rows.
    pub fn marker_entry(&self) -> Option<usize> {
        self.marker_entry
    }

    /// Consume any pending "scroll this entry into view" request
    /// from the popup layer. Returns `Some(idx)` exactly once per
    /// jump request; subsequent frames return `None`. The renderer
    /// uses the per-entry wrapped-row count it computes anyway to
    /// translate `idx` into an absolute scroll offset — that
    /// translation needs area.width, which is why the popup can't
    /// pre-compute it.
    pub fn take_pending_jump(&mut self) -> Option<usize> {
        self.pending_jump.take()
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
        let session = self.session.read().unwrap();
        is_buffer_complete(&buf, &session.signature_table())
    }

    fn current_input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    /// Translate the input editor's `(row, col)` cursor into a byte
    /// offset within `current_input_text()`. Returns `None` when the
    /// cursor is past EOF (shouldn't happen — TextArea keeps it in
    /// bounds — but defensive).
    fn cursor_byte_offset(&self) -> Option<u32> {
        let (row, col) = self.input.cursor();
        let buffer = self.current_input_text();
        let line_idx = vw_htcl::line_index::LineIndex::new(&buffer);
        Some(line_idx.offset_of(vw_htcl::line_index::LineCol {
            line: row as u32,
            character: col as u32,
        }))
    }

    /// Map an input-buffer (row, col) to a screen cell within the
    /// input editor's rendered area. Used to anchor popups (slice 4+)
    /// just below the cursor. Returns `None` when no scrollback area
    /// has been captured yet (shouldn't happen post-first-render but
    /// defensive against early key events).
    fn cursor_screen_cell(&self) -> Option<(u16, u16)> {
        // We don't have direct access to the input area Rect here
        // (only the scrollback's). The popup anchor instead uses a
        // best-effort approximation: assume the input area starts
        // just below the scrollback and the popup will clamp to the
        // frame in the renderer. Concretely: row = bottom of the
        // visible scrollback (where the input border sits) + cursor
        // row in the editor, col = cursor col + the input area's
        // left edge (we use scrollback's x which they share).
        let area = self.scrollback_area?;
        let (row, col) = self.input.cursor();
        // +1 to step past the input box's top border, +area.y to land
        // inside the input region. The renderer's popup positioning
        // does additional clamping so over- and under-shoots are safe.
        let screen_y = area.y + area.height + 1 + row as u16;
        let screen_x = area.x + 1 + col as u16;
        Some((screen_x, screen_y))
    }

    /// Trigger a completion popup at the current cursor position. No-op
    /// when there are no completions to show. Called from the Tab key
    /// handler.
    fn trigger_completion(&mut self) {
        let input = self.current_input_text();
        let Some(offset) = self.cursor_byte_offset() else {
            return;
        };
        // Parse ONLY the in-flight input. Earlier we tried merging
        // session.merged_source() (~6MB after `src @vivado-cmd`)
        // into the analysis source so `util::<Tab>` would see
        // session-known procs. That had two fatal problems:
        //
        //  1. Per-Tab cost was a multi-MB parse + a multi-MB
        //     `cmdline::analyze` walk-back. The UI froze for
        //     seconds; queued keypresses (ctrl-D, backspace)
        //     drained after the parse finished.
        //  2. `cmdline::analyze` balances `[` / `]` but doesn't
        //     know about `#` comments. The auto-generated Vivado
        //     docs contain `[get_hw_sysmons]`, `[Common 17-39]`,
        //     etc. inside `## doc-comment` blocks; an unmatched
        //     bracket in those docs put the analyzer in
        //     "inside-a-substitution" state forever, blowing past
        //     every newline and never finding the command
        //     boundary. End result: `partial="util::"` but
        //     `head_words` was the entire 6 MB session.
        //
        // Cheaper, correct approach: analyze only the in-flight
        // input (small, fast, no rogue brackets), then pull
        // candidate proc names directly out of
        // `Session::signature_table()` — that's a HashMap built
        // from already-parsed batches; O(N) iteration over
        // existing data instead of an MB-scale reparse + walk.
        let parsed = vw_htcl::parser::parse(&input);
        let cmd_line = vw_htcl::cmdline::analyze(&input, offset);
        let session_guard = self.session.read().unwrap();
        let session_sigs = session_guard.signature_table();
        let input_sigs = vw_htcl::signature_table(&parsed.document);
        // The currently-shipping batch hasn't committed yet — session
        // commit only happens on EvalDone(last_in_batch=true). During
        // the prime.htcl load (e.g. `src @vivado-cmd` taking 50+
        // seconds), every proc the user wants to complete on (util::*,
        // create_*, …) lives in `pending_batch.document` but NOT in
        // session.signature_table(). Surface those too — they're
        // already parsed; cost is one `signature_table` walk over the
        // in-flight document's stmts.
        let pending_sigs: std::collections::HashMap<
            String,
            &vw_htcl::ProcSignature,
        > = self
            .pending_batch
            .as_ref()
            .map(|b| vw_htcl::signature_table(&b.document))
            .unwrap_or_default();
        let mut items: Vec<vw_htcl::complete::Completion> = Vec::new();
        // Meta-command branch: `:load`, `:quit`, etc. Detected by
        // a leading `:` on the partial — these are App-side
        // commands, not htcl, so they live above the cmdline
        // analyzer's notion of command position.
        if cmd_line.partial.starts_with(':') {
            for (label, hint) in META_COMMANDS {
                if label.starts_with(cmd_line.partial) {
                    items.push(vw_htcl::complete::Completion {
                        label: label.to_string(),
                        kind: vw_htcl::complete::CompletionKind::Proc,
                        detail: Some(hint.to_string()),
                        documentation: None,
                        replace: cmd_line.partial_span,
                        insert_text: None,
                        snippet: false,
                    });
                }
            }
        } else if cmd_line.in_command_position() {
            // Proc-name completion: union of session + pending +
            // in-flight proc names, filtered by the partial prefix.
            let mut names: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for name in session_sigs.keys() {
                names.insert(name.clone());
            }
            for name in pending_sigs.keys() {
                names.insert(name.clone());
            }
            for name in input_sigs.keys() {
                names.insert(name.clone());
            }
            for name in names {
                if name.starts_with(cmd_line.partial) {
                    items.push(vw_htcl::complete::Completion {
                        label: name,
                        kind: vw_htcl::complete::CompletionKind::Proc,
                        detail: None,
                        documentation: None,
                        replace: cmd_line.partial_span,
                        insert_text: None,
                        snippet: false,
                    });
                }
            }
        } else if let Some(cmd_name) = cmd_line.command_name() {
            // Flag completion: look up the called proc's signature
            // in either source and emit its flag args. Matches the
            // shape of `vw_htcl::complete::complete_at`'s flag path
            // but uses our union of session+input signatures.
            let sig = session_sigs
                .get(cmd_name)
                .copied()
                .or_else(|| pending_sigs.get(cmd_name).copied())
                .or_else(|| input_sigs.get(cmd_name).copied());
            if let Some(sig) = sig {
                let used: Vec<&str> = cmd_line.used_flags().collect();
                let needle_no_dash = cmd_line
                    .partial
                    .strip_prefix('-')
                    .unwrap_or(cmd_line.partial);
                // Required (no @default) flags first, then optional,
                // alphabetical within each group. Matches the
                // signature-help popup's ordering so the user sees
                // the same priority across surfaces.
                for &i in &sorted_arg_indices(sig) {
                    let arg = &sig.args[i];
                    let label = format!("-{}", arg.name);
                    if used.contains(&label.as_str()) {
                        continue;
                    }
                    if arg.name.starts_with(needle_no_dash) {
                        // Detail shows the type + default when
                        // available, so the completion popup row
                        // hints at what each flag expects without
                        // requiring the user to open hover. Format
                        // mirrors the sig-help line: `type = value`.
                        let detail = build_flag_detail(arg);
                        items.push(vw_htcl::complete::Completion {
                            label,
                            kind: vw_htcl::complete::CompletionKind::Flag,
                            detail,
                            documentation: None,
                            replace: cmd_line.partial_span,
                            insert_text: None,
                            snippet: false,
                        });
                    }
                }
            }
        }
        let anchor = self.cursor_screen_cell().unwrap_or((0, 0));
        if let Some(popup) = crate::popup::CompletionPopup::new(items, anchor) {
            self.popup = Some(crate::popup::PopupState::Completion(popup));
        }
    }

    /// Route a key event to the active popup. Returns `true` when the
    /// key was consumed (navigation / accept / dismiss); `false` lets
    /// the key fall through to the rest of the handler.
    fn handle_popup_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        let Some(popup) = self.popup.as_mut() else {
            return false;
        };
        match popup {
            crate::popup::PopupState::Completion(comp) => {
                match key.code {
                    KeyCode::Up => {
                        comp.move_up();
                        true
                    }
                    KeyCode::Down => {
                        comp.move_down();
                        true
                    }
                    KeyCode::Esc => {
                        self.popup = None;
                        true
                    }
                    KeyCode::Enter if !key.modifiers.is_empty() => {
                        // Any modifier on Enter (Shift, Alt, Ctrl,
                        // combos) is the "keep typing" escape
                        // hatch. Don't let the popup consume it —
                        // dismiss the popup so the outer handler
                        // can insert a newline. Terminals differ
                        // on which modifier they attach; accept
                        // any of them.
                        self.popup = None;
                        false
                    }
                    KeyCode::Char('j')
                        if key.modifiers.contains(
                            crossterm::event::KeyModifiers::CONTROL,
                        ) =>
                    {
                        // Shift+Enter on legacy terminals arrives
                        // as Ctrl+J — see the outer handler's
                        // matching branch for the rationale. Let
                        // it through so the outer handler can
                        // insert a newline.
                        self.popup = None;
                        false
                    }
                    KeyCode::Enter => {
                        if let Some(item) = comp.current().cloned() {
                            self.apply_completion(&item);
                        }
                        self.popup = None;
                        true
                    }
                    KeyCode::Tab => {
                        // Tab re-triggers — just cycle for now.
                        comp.move_down();
                        true
                    }
                    _ => {
                        // Any other key dismisses the popup and falls
                        // through to the editor — typical IDE
                        // behavior where you can keep typing past the
                        // popup to refine your input.
                        self.popup = None;
                        false
                    }
                }
            }
            crate::popup::PopupState::Help(_) => {
                // Any keystroke dismisses the help modal. We CONSUME
                // the dismissing key (return true) so it doesn't
                // also act on the input — pressing Ctrl-H to open
                // then any other key to close shouldn't accidentally
                // type the close key into the editor.
                self.popup = None;
                true
            }
            crate::popup::PopupState::SignatureHelp(sig) => {
                // Signature help is the background auto-show; it
                // doesn't consume keys, falls through to the editor.
                // Esc lets users hide it without clearing input.
                if key.code == KeyCode::Esc {
                    self.popup = None;
                    return true;
                }
                // Shift-↑ / Shift-↓: scroll through args when the
                // signature is too tall to fit. Picked over
                // Ctrl-↑/Ctrl-↓ because macOS reserves those for
                // Mission Control. We consume these chords (return
                // true) so they don't ALSO scroll the scrollback.
                // Step by 1 — fine-grained because each arg is a
                // self-contained row.
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    match key.code {
                        KeyCode::Up => {
                            sig.scroll_offset =
                                sig.scroll_offset.saturating_sub(1);
                            return true;
                        }
                        KeyCode::Down => {
                            sig.scroll_offset =
                                sig.scroll_offset.saturating_add(1);
                            return true;
                        }
                        _ => {}
                    }
                }
                false
            }
            crate::popup::PopupState::Hover(_) => {
                // Hover dismisses on any keystroke. We consume the
                // key so the dismissing keystroke doesn't also act
                // on the input — Ctrl-Y to open + any key to close
                // shouldn't smuggle that key into the buffer.
                self.popup = None;
                true
            }
            crate::popup::PopupState::SymbolSearch(picker) => {
                use crate::symbol_search::PickerView;
                match key.code {
                    KeyCode::Esc => {
                        self.popup = None;
                        true
                    }
                    KeyCode::Up => {
                        picker.move_up();
                        true
                    }
                    KeyCode::Down => {
                        picker.move_down();
                        true
                    }
                    KeyCode::Tab => {
                        picker.toggle_view();
                        true
                    }
                    KeyCode::Backspace => {
                        if picker.view == PickerView::Symbols {
                            picker.pop_char();
                        }
                        true
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && picker.view == PickerView::Symbols =>
                    {
                        picker.push_char(c);
                        true
                    }
                    KeyCode::Enter => {
                        match picker.view {
                            PickerView::Symbols => {
                                if let Some(sym) =
                                    picker.current_symbol().cloned()
                                {
                                    self.popup = None;
                                    self.insert_at_cursor_replacing_word(
                                        &sym.name,
                                    );
                                } else {
                                    self.popup = None;
                                }
                            }
                            PickerView::Libraries => {
                                picker.apply_library_filter();
                            }
                        }
                        true
                    }
                    _ => true, // swallow other keys; popup stays open
                }
            }
            crate::popup::PopupState::DiagnosticSearch(picker) => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {
                        self.popup = None;
                        true
                    }
                    (KeyCode::Up, _) => {
                        picker.move_up();
                        true
                    }
                    (KeyCode::Down, _) => {
                        picker.move_down();
                        true
                    }
                    (KeyCode::Char('e'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        picker.toggle_kind(ScrollbackKind::Error);
                        true
                    }
                    (KeyCode::Char('w'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        picker.toggle_kind(ScrollbackKind::Warning);
                        true
                    }
                    (KeyCode::Char('n'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        picker.toggle_kind(ScrollbackKind::Notice);
                        true
                    }
                    (KeyCode::Char('k'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        // Ctrl-K toggles the Critical-warning
                        // subset filter. Only meaningful inside
                        // the popup — outside, Ctrl-K is
                        // scrollback-up, which the popup handler
                        // shadows while active.
                        picker.toggle_critical();
                        true
                    }
                    (KeyCode::Backspace, _) => {
                        picker.pop_char();
                        true
                    }
                    (KeyCode::Char(c), m)
                        if !m.contains(KeyModifiers::CONTROL) =>
                    {
                        picker.push_char(c);
                        true
                    }
                    (KeyCode::Enter, _) => {
                        // Snapshot the target BEFORE dropping the
                        // popup — `picker` borrows through
                        // `self.popup`; setting `self.popup = None`
                        // invalidates it.
                        let target = picker
                            .current()
                            .map(|it| (it.scrollback_idx, it.kind));
                        self.popup = None;
                        if let Some((idx, kind)) = target {
                            self.jump_to_scrollback_entry(idx, kind);
                        }
                        true
                    }
                    _ => true,
                }
            }
        }
    }

    /// Insert / replace text from a chosen completion. Replaces the
    /// byte range `item.replace` (the partial word under the cursor,
    /// or a zero-width insertion point) with `item.label`.
    fn apply_completion(&mut self, item: &vw_htcl::complete::Completion) {
        let buffer = self.current_input_text();
        let start = item.replace.start as usize;
        let end = (item.replace.end as usize).min(buffer.len());
        if start > buffer.len() {
            return;
        }
        let mut new_buffer = String::with_capacity(
            buffer.len() - (end - start) + item.label.len(),
        );
        new_buffer.push_str(&buffer[..start]);
        new_buffer.push_str(&item.label);
        new_buffer.push_str(&buffer[end..]);
        // Place cursor just after the inserted label.
        let new_cursor_byte = start + item.label.len();
        self.replace_input_with_cursor(new_buffer, new_cursor_byte);
    }

    /// Replace the input buffer with `text` and move the cursor to
    /// the byte offset `cursor_byte`. Cursor offset is translated to
    /// (row, col) via `LineIndex`. Used by completion accept.
    fn replace_input_with_cursor(&mut self, text: String, cursor_byte: usize) {
        use tui_textarea::TextArea;
        let line_idx = vw_htcl::line_index::LineIndex::new(&text);
        // Build the textarea fresh from the new content (tui-textarea
        // doesn't offer a "replace everything" API; recreating is the
        // documented way per its issue tracker).
        let lines: Vec<String> =
            text.split('\n').map(|s| s.to_string()).collect();
        let mut ta = TextArea::new(lines);
        ta.set_cursor_line_style(ratatui::style::Style::default());
        // Position cursor.
        let lc = line_idx.position(cursor_byte as u32);
        let target_row = lc.line as usize;
        let target_col = lc.character as usize;
        // Use the textarea's `move_cursor` API.
        while ta.cursor().0 < target_row {
            ta.move_cursor(tui_textarea::CursorMove::Down);
        }
        while ta.cursor().0 > target_row {
            ta.move_cursor(tui_textarea::CursorMove::Up);
        }
        while ta.cursor().1 < target_col {
            ta.move_cursor(tui_textarea::CursorMove::Forward);
        }
        while ta.cursor().1 > target_col {
            ta.move_cursor(tui_textarea::CursorMove::Back);
        }
        self.input = ta;
        self.history_cursor = None;
    }

    /// Whether a popup is currently open (renderer queries this to
    /// decide whether to draw the popup overlay layer).
    pub fn popup_state(&self) -> Option<&crate::popup::PopupState> {
        self.popup.as_ref()
    }

    /// Refresh signature help after a buffer-mutating keystroke.
    /// Looks up the proc the cursor sits in by name across session,
    /// pending batch, and in-flight input; populates a
    /// `PopupState::SignatureHelp` with its args + active parameter.
    /// Dismisses any existing sig-help popup when nothing matches.
    ///
    /// Coexistence rule: never displaces a Completion or Help popup
    /// (the user explicitly opened those; sig-help is the background
    /// auto-show).
    fn refresh_signature_help(&mut self) {
        // Don't fight explicit popups.
        if matches!(
            self.popup,
            Some(crate::popup::PopupState::Completion(_))
                | Some(crate::popup::PopupState::Help(_))
        ) {
            return;
        }
        let input = self.current_input_text();
        let Some(offset) = self.cursor_byte_offset() else {
            self.dismiss_signature_help();
            return;
        };
        let cmd_line = vw_htcl::cmdline::analyze(&input, offset);
        let Some(name) = cmd_line.command_name() else {
            self.dismiss_signature_help();
            return;
        };
        // Find the proc's signature + doc comments. Signatures live
        // in session.signature_table() / pending / in-flight; doc
        // comments require walking the source document (kept on the
        // Command, not the ProcSignature).
        let parsed = vw_htcl::parser::parse(&input);
        // Arc-clone the session handle first — this drops the
        // borrow on `self` immediately, so we can call `self.push` /
        // `self.dismiss_signature_help` etc. later without the
        // read guard blocking the mutable borrow of self.
        let session = std::sync::Arc::clone(&self.session);
        let session_guard = session.read().unwrap();
        let session_sigs = session_guard.signature_table();
        let input_sigs = vw_htcl::signature_table(&parsed.document);
        let pending_doc = self.pending_batch.as_ref().map(|b| &b.document);
        let pending_sigs = pending_doc
            .map(|d| vw_htcl::signature_table(d))
            .unwrap_or_default();
        let sig = session_sigs
            .get(name)
            .copied()
            .or_else(|| pending_sigs.get(name).copied())
            .or_else(|| input_sigs.get(name).copied());
        let Some(sig) = sig else {
            self.dismiss_signature_help();
            return;
        };
        // Look up doc comments by walking docs in priority order
        // (input → pending → session). Most recent wins, which
        // matches Tcl's "later proc shadows earlier" semantics that
        // the lowerer already uses.
        let mut doc_comments: &[String] = &[];
        if let Some(d) = lookup_proc_doc_comments(&parsed.document, name) {
            doc_comments = d;
        } else if let Some(doc) = pending_doc {
            if let Some(d) = lookup_proc_doc_comments(doc, name) {
                doc_comments = d;
            }
        } else {
            // Walk session batches in reverse (newest first).
            for batch in session_guard.batches_for_doc_search() {
                if let Some(d) = lookup_proc_doc_comments(&batch.document, name)
                {
                    doc_comments = d;
                    break;
                }
            }
        }
        // Build the display permutation (required → optional,
        // alphabetical within each group) and reorder args + the
        // active-parameter index accordingly.
        let display_order = sorted_arg_indices(sig);
        let active_decl = compute_active_parameter(sig, &cmd_line);
        let active = active_decl.and_then(|orig| {
            display_order
                .iter()
                .position(|&i| i == orig as usize)
                .map(|i| i as u32)
        });
        let args: Vec<crate::popup::SigHelpArg> = display_order
            .iter()
            .map(|&i| {
                let a = &sig.args[i];
                crate::popup::SigHelpArg {
                    name: a.name.clone(),
                    type_str: a.type_annotation.as_ref().map(render_type),
                    default_str: format_default_value(a),
                }
            })
            .collect();
        let return_type = sig.return_type.as_ref().map(render_type);
        let doc_brief = vw_htcl::doc::brief(doc_comments);
        let anchor = self.cursor_screen_cell().unwrap_or((0, 0));
        // Preserve any user-set scroll offset across refreshes —
        // manual Ctrl-↑/↓ scrolling shouldn't be reset by every
        // keystroke. The renderer clamps the offset to a valid
        // range, so an offset that's stale for the new arg list
        // (e.g. switching to a smaller proc) silently snaps back.
        let prev_scroll = match self.popup.as_ref() {
            Some(crate::popup::PopupState::SignatureHelp(p)) => p.scroll_offset,
            _ => 0,
        };
        let popup = crate::popup::SignatureHelpPopup {
            proc_name: name.to_string(),
            args,
            return_type,
            doc_brief,
            active: active.map(|a| a as usize),
            anchor,
            scroll_offset: prev_scroll,
        };
        self.popup = Some(crate::popup::PopupState::SignatureHelp(popup));
    }

    /// Open the fuzzy symbol picker (Ctrl-T). Builds a fresh
    /// `SymbolIndex` snapshot at open time — it stays stable while
    /// the popup is alive so the result-row indices don't shift
    /// out from under the user. The index is small to build
    /// (walks already-parsed Documents) so per-open cost is fine.
    fn trigger_symbol_search(&mut self) {
        let input = self.current_input_text();
        let parsed = vw_htcl::parser::parse(&input);
        let session_guard = self.session.read().unwrap();
        let index =
            std::sync::Arc::new(crate::symbol_index::SymbolIndex::build(
                &session_guard,
                self.pending_batch.as_ref(),
                Some(&parsed.document),
            ));
        let picker = crate::symbol_search::SymbolPicker::new(index);
        self.popup = Some(crate::popup::PopupState::SymbolSearch(picker));
    }

    /// Ctrl-F opens the diagnostics finder. Snapshots the current
    /// scrollback (Error/Warning/Notice only) and hands it to the
    /// picker. Snapshot semantics: rows appended after open aren't
    /// visible in this session of the picker; user reopens to see
    /// them. Prevents result-index churn while typing a query
    /// against a still-streaming scrollback.
    fn trigger_diagnostic_search(&mut self) {
        let picker = crate::diag_search::DiagnosticPicker::from_scrollback(
            &self.scrollback,
        );
        self.popup = Some(crate::popup::PopupState::DiagnosticSearch(picker));
    }

    /// Set the marker on `idx`, request the next render to scroll
    /// that entry into view, and disengage tail-follow. Called
    /// when the diagnostics-finder popup Accept fires; the
    /// scrolling itself happens in the renderer next frame
    /// (needs area.width to translate entry-idx → row offset).
    /// `_kind` is captured for future use — right now the marker
    /// styling is kind-agnostic (fixed color), but a per-kind
    /// tint would use it.
    fn jump_to_scrollback_entry(&mut self, idx: usize, _kind: ScrollbackKind) {
        if idx >= self.scrollback.len() {
            return;
        }
        // Expand the containing input group if it's collapsed.
        // Without this, jumping to a diagnostic scrolls to a row
        // that renders as 0 rows (child of a collapsed group), so
        // the marker lands on empty space and the user sees
        // nothing near the target. Expanding first makes the
        // target row actually visible.
        let parent_idx = self.scrollback[idx].parent_input_idx;
        if let Some(pidx) = parent_idx {
            if let Some(parent) = self.scrollback.get_mut(pidx) {
                if matches!(parent.kind, ScrollbackKind::Input) {
                    parent.group_collapsed = false;
                }
            }
        }
        self.marker_entry = Some(idx);
        self.pending_jump = Some(idx);
        self.scrollback_follow = false;
    }

    /// Alt-C clears the persistent marker. No-op when no marker
    /// is set; harmless to press repeatedly.
    fn clear_marker(&mut self) {
        self.marker_entry = None;
    }

    /// Insert `text` at the current cursor position, replacing the
    /// identifier-under-cursor (if any). Used by the symbol-picker
    /// Enter handler to insert a chosen symbol name in place of the
    /// partial word the user is typing.
    fn insert_at_cursor_replacing_word(&mut self, text: &str) {
        let buffer = self.current_input_text();
        let Some(offset) = self.cursor_byte_offset() else {
            return;
        };
        let off = offset as usize;
        // Find word boundaries around the cursor (same rule as
        // `ident_under_cursor`).
        let bytes = buffer.as_bytes();
        let is_word_byte = |b: u8| -> bool {
            b.is_ascii_alphanumeric() || b == b'_' || b == b':'
        };
        let mut start = off;
        while start > 0 && is_word_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = off;
        while end < bytes.len() && is_word_byte(bytes[end]) {
            end += 1;
        }
        let mut new_buffer =
            String::with_capacity(buffer.len() - (end - start) + text.len());
        new_buffer.push_str(&buffer[..start]);
        new_buffer.push_str(text);
        new_buffer.push_str(&buffer[end..]);
        let new_cursor = start + text.len();
        self.replace_input_with_cursor(new_buffer, new_cursor);
    }

    /// Hover-under-cursor: open a popup showing the proc /
    /// variable / enum the cursor is on. Tries
    /// [`vw_htcl::hover_at`] on the in-flight input first (catches
    /// local vars, in-buffer proc decls, enum decls). Falls back to
    /// a session-aware lookup: extracts the identifier under the
    /// cursor and resolves it against
    /// `Session::signature_table()` / pending / in-flight, so
    /// `Ctrl-Y` on a `util::props` call surfaces the library's docs
    /// even though the proc was defined in a separate session
    /// batch.
    fn trigger_hover(&mut self) {
        let input = self.current_input_text();
        let Some(offset) = self.cursor_byte_offset() else {
            return;
        };
        let parsed = vw_htcl::parser::parse(&input);
        let anchor = self.cursor_screen_cell().unwrap_or((0, 0));
        // Pass 1: in-document hover. Handles ProcDef, ProcArgDef,
        // CallSite when the proc is in the buffer, CallArg,
        // LocalVar, EnumDef.
        if let Some(target) =
            vw_htcl::hover_at(&parsed.document, &input, offset)
        {
            if let Some(popup) = hover_target_to_popup(target, anchor) {
                self.popup = Some(crate::popup::PopupState::Hover(popup));
                return;
            }
        }
        // Pass 2: session-aware proc lookup by identifier under
        // cursor. Covers the common REPL case: cursor on a name
        // referencing a session-loaded library proc.
        let Some(name) = ident_under_cursor(&input, offset) else {
            return;
        };
        let session = std::sync::Arc::clone(&self.session);
        let session_guard = session.read().unwrap();
        let session_sigs = session_guard.signature_table();
        let pending_doc = self.pending_batch.as_ref().map(|b| &b.document);
        let pending_sigs = pending_doc
            .map(|d| vw_htcl::signature_table(d))
            .unwrap_or_default();
        let input_sigs = vw_htcl::signature_table(&parsed.document);
        let sig = session_sigs
            .get(name)
            .copied()
            .or_else(|| pending_sigs.get(name).copied())
            .or_else(|| input_sigs.get(name).copied());
        let Some(sig) = sig else { return };
        // Doc comments: walk newest source first (input → pending →
        // session newest-first).
        let mut doc_comments: &[String] = &[];
        if let Some(d) = lookup_proc_doc_comments(&parsed.document, name) {
            doc_comments = d;
        } else if let Some(doc) = pending_doc {
            if let Some(d) = lookup_proc_doc_comments(doc, name) {
                doc_comments = d;
            }
        } else {
            for batch in session_guard.batches_for_doc_search() {
                if let Some(d) = lookup_proc_doc_comments(&batch.document, name)
                {
                    doc_comments = d;
                    break;
                }
            }
        }
        let title = render_proc_title(name, sig);
        let body = vw_htcl::doc::reflow_doc_comments(doc_comments);
        self.popup =
            Some(crate::popup::PopupState::Hover(crate::popup::HoverPopup {
                title,
                body,
                anchor,
            }));
    }

    /// Dismiss the signature-help popup if one is active. Leaves
    /// Completion / Help popups alone.
    fn dismiss_signature_help(&mut self) {
        if matches!(
            self.popup,
            Some(crate::popup::PopupState::SignatureHelp(_))
        ) {
            self.popup = None;
        }
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
                // Auto-scroll when the drag wanders past the
                // top or bottom edge of the scrollback area so
                // selections can extend beyond the current
                // viewport. Crossterm fires drag events per
                // cell of mouse movement, so the user wiggles
                // the mouse at the edge to keep scrolling;
                // simpler than tracking a "held at edge" timer
                // and good enough for selection-extension UX.
                let bottom = area.y + area.height;
                if mouse.row >= bottom {
                    self.scroll_by(3);
                } else if mouse.row < area.y {
                    self.scroll_by(-3);
                }
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
                    // Shift + pure click (no drag) on a collapsible
                    // block toggles its expanded state. Detect "pure
                    // click" by comparing anchor == cursor: Drag
                    // events are the only path that moves `cursor`
                    // off `anchor`, so any drag at all falls through
                    // to the copy path. Drag-select inside an
                    // expanded block still works because the
                    // anchor/cursor pair diverges as soon as the
                    // first Drag event fires.
                    // Two gestures for toggling a collapsible /
                    // input group:
                    //
                    // 1. Shift-click on the row body — works on
                    //    terminals that forward Shift+mouse to
                    //    the app. Many (iTerm2, GNOME Terminal,
                    //    macOS Terminal.app) reserve Shift+click
                    //    for their OWN text-selection override
                    //    and swallow the event before the app
                    //    ever sees it — those events never reach
                    //    here.
                    //
                    // 2. Plain click on the marker column (0-1)
                    //    of the row that carries the ▶/▼ glyph
                    //    — works everywhere, no modifier fights,
                    //    matches the "click the arrow" gesture
                    //    file explorers and Vivado's own GUI
                    //    use. Guarded by `same` (no drag) so
                    //    starting a text selection near the left
                    //    edge doesn't accidentally toggle.
                    let same = sel.anchor == sel.cursor;
                    let shift_toggle =
                        mouse.modifiers.contains(KeyModifiers::SHIFT)
                            && same
                            && self.toggle_collapsible_at(sel.anchor.0);
                    let marker_toggle = !shift_toggle
                        && same
                        && sel.anchor.1 < 2
                        && self.toggle_collapsible_at(sel.anchor.0);
                    if shift_toggle || marker_toggle {
                        return;
                    }
                    self.copy_selection_to_clipboard(sel);
                }
            }
            _ => {}
        }
    }

    /// Map a wrapped-row index (0-based, spans all of `scrollback`)
    /// to the entry that occupies it, and if that entry is a
    /// collapsible NONE block, toggle its expand state. Returns
    /// `true` when a toggle happened so the caller can suppress the
    /// fallthrough "copy empty selection" path — a Shift-click on a
    /// diagnostic line should still do nothing (not clobber the
    /// clipboard with an empty string, not act on the diagnostic
    /// entry), so a `false` here means "not our gesture, keep
    /// falling through."
    ///
    /// Walks entries left-to-right, summing wrapped-row counts until
    /// we find the entry the row lives in. O(N) per click — cheap at
    /// scrollback sizes we care about.
    fn toggle_collapsible_at(&mut self, wrapped_row: usize) -> bool {
        let width = self.scrollback_area.map(|a| a.width).unwrap_or(0);
        // First pass: figure out which entry index owns
        // `wrapped_row`. Row math mirrors what
        // `ui::compute_visible_counts` does — hidden children of
        // a collapsed group contribute 0 rows, so a shift-click
        // near the top of scrollback lands on the right entry
        // regardless of what's collapsed above it.
        let mut cursor: usize = 0;
        let mut hit: Option<usize> = None;
        for (idx, entry) in self.scrollback.iter().enumerate() {
            let hidden = entry
                .parent_input_idx
                .and_then(|p| self.scrollback.get(p))
                .map(|p| p.group_collapsed)
                .unwrap_or(false);
            let rows = if hidden {
                0
            } else {
                crate::render::count_wrapped_rows(entry, width) as usize
            };
            let end = cursor.saturating_add(rows);
            if (cursor..end).contains(&wrapped_row) {
                hit = Some(idx);
                break;
            }
            cursor = end;
        }
        let Some(idx) = hit else { return false };
        let target = &mut self.scrollback[idx];
        // Two toggle behaviors:
        //   * Input rows: flip the whole group's visibility
        //     via `group_collapsed`.
        //   * Non-Input collapsible blocks: flip the entry's
        //     own multi-line body via `collapse_state`.
        if matches!(target.kind, ScrollbackKind::Input) {
            target.group_collapsed = !target.group_collapsed;
            return true;
        }
        match target.collapse_state {
            Some(expanded) => {
                target.collapse_state = Some(!expanded);
                true
            }
            None => false,
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
        // Skip children of collapsed input groups when building
        // the flat list — same visibility rule
        // `ui::compute_visible_counts` uses. Selection row
        // indices are in VISIBLE-row space (that's what the
        // renderer draws and what mouse cell → row math
        // produces), so if this build path included hidden
        // entries the row indices would map to the wrong lines
        // and the clipboard would get chunks of hidden output
        // instead of what the user selected.
        let mut flat: Vec<ratatui::text::Line<'static>> = Vec::new();
        for entry in &self.scrollback {
            let hidden = entry
                .parent_input_idx
                .and_then(|p| self.scrollback.get(p))
                .map(|p| p.group_collapsed)
                .unwrap_or(false);
            if hidden {
                continue;
            }
            for line in crate::render::entry_lines(entry, area.width) {
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
        if let Event::Paste(data) = ev {
            self.handle_paste(data);
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

        // Popup navigation / dismissal takes precedence over both
        // app-level chords AND the catch-all editor handoff, so
        // Up/Down/Enter/Esc go to the popup when one is open.
        if self.popup.is_some() && self.handle_popup_key(key) {
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                // Exit unconditionally. The original behavior required
                // the input to be empty (readline convention), but
                // for a REPL it's strictly an annoyance — you can't
                // exit a misformed in-progress command without
                // clearing it first. Ctrl-C is the right key to
                // discard the current input, and we already bind it
                // to that.
                self.push(ScrollbackKind::Notice, "exit".to_string());
                self.exit = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Two modes for Ctrl-C:
                //
                // - **Eval in flight** (`WorkerState::Running`):
                //   send SIGINT to the Vivado child, which its Tcl
                //   runtime traps into `interp cancel` — the
                //   current eval aborts and returns a "interrupted"
                //   error through the shim protocol, without
                //   killing the Vivado session. Full mechanism +
                //   design assumptions on
                //   `vw_vivado::VivadoBackend::interrupt`. No fall-
                //   through to the input clear; the input line is
                //   probably empty during an eval anyway, and
                //   preserving whatever the user typed lets them
                //   resubmit / edit after the cancel lands.
                //
                // - **No eval running**: clear the current input
                //   (reedline / readline convention).
                if matches!(self.worker_state, WorkerState::Running) {
                    if let Some(pid) = self.child_pid {
                        // Signal the process GROUP, not the process.
                        // Vivado's on-disk binary is a bash wrapper
                        // that forks loader+Vivado as children without
                        // `exec`, so `kill(pid, SIGINT)` would only
                        // hit bash and bash won't forward it. See
                        // `VivadoBackend::interrupt` for the full
                        // writeup — we mirror the same `-pid`
                        // negation here rather than round-tripping
                        // through the worker channel (which is
                        // blocked on the very eval we're cancelling).
                        //
                        // SAFETY: kill on a pgid we spawned. ESRCH
                        // (empty group / race with child exit) is a
                        // silent no-op.
                        unsafe {
                            libc::kill(-(pid as libc::pid_t), libc::SIGINT);
                        }
                        self.push(
                            ScrollbackKind::Notice,
                            "interrupt sent — Vivado will abort the \
                             current eval and return"
                                .into(),
                        );
                    } else {
                        self.push(
                            ScrollbackKind::Warning,
                            "no pid cached; can't interrupt eval — \
                             restart the REPL to recover"
                                .into(),
                        );
                    }
                } else {
                    self.input = TextArea::default();
                    self.history_cursor = None;
                    self.history_draft.clear();
                }
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
            (KeyCode::Tab, _) => {
                self.trigger_completion();
            }
            (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                self.popup = Some(crate::popup::PopupState::Help(
                    crate::popup::HelpPopup,
                ));
            }
            (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                // Hover under cursor. We use Ctrl-Y (not Ctrl-K,
                // which is already scrollback-up) — picked because
                // Ctrl-K is also commonly conflated with
                // "kill-line" elsewhere. Y for "your symbol's docs."
                self.trigger_hover();
            }
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                // Fuzzy symbol picker over the session + pending +
                // in-flight input. Opens centered; Tab toggles to
                // the libraries view. (Ctrl-S over Ctrl-T because
                // the latter often gets eaten by terminal
                // multiplexers, and "S" for "search" is the more
                // discoverable mnemonic.)
                self.trigger_symbol_search();
            }
            (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                // Fuzzy diagnostics finder. Snapshots the current
                // scrollback's Error/Warning/Notice entries. Enter
                // jumps the viewport to the chosen entry and drops
                // a persistent left-gutter marker (Alt-C clears).
                self.trigger_diagnostic_search();
            }
            (KeyCode::Char('c'), KeyModifiers::ALT) => {
                // Clear the diagnostics-finder jump marker. No-op
                // when nothing is marked. Picked Alt-C over Ctrl-L
                // (which many terminals eat for "clear screen") and
                // Ctrl-K (already bound to scroll-up).
                self.clear_marker();
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
            // Vim-style scroll: Alt+K up, Alt+J down. Alt (not
            // Ctrl) because Ctrl+J is claimed by Shift+Enter on
            // legacy-encoding terminals (see the Ctrl+J-as-
            // newline arm below), and single-modifier consistency
            // beats splitting the pair across two modifiers.
            (KeyCode::PageUp, _) | (KeyCode::Char('k'), KeyModifiers::ALT) => {
                self.scroll_by(-5);
            }
            (KeyCode::PageDown, _)
            | (KeyCode::Char('j'), KeyModifiers::ALT) => {
                self.scroll_by(5);
            }
            // Snap to bottom + re-engage tail-follow. Use End
            // (when available) or Ctrl-G as the compact-keyboard
            // alternative. After scrolling up to inspect old
            // output the user explicitly requests "back to live"
            // here; we no longer auto-re-engage on every scroll
            // (that auto-engage was firing spuriously due to a
            // raw-vs-wrapped line-count mismatch, making scroll
            // appear dead on large outputs).
            (KeyCode::End, _) | (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                self.scrollback_follow = true;
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.on_submit().await;
            }
            (KeyCode::Enter, m) if !m.is_empty() => {
                // Enter with ANY modifier is a "keep typing"
                // escape hatch — Ctrl+Enter and Alt+Enter both
                // arrive here as `Enter + CTRL/ALT` under the
                // kitty protocol.
                self.insert_newline_preserving_indent();
            }
            // Shift+Enter on legacy-encoding terminals (macOS
            // Terminal.app, GNOME Terminal without kitty
            // protocol, and many tmux configurations) arrives
            // as Ctrl+J — because ASCII 0x0A (linefeed) IS what
            // the shifted-Enter physically produces, and raw
            // mode disables the `\n` → Enter auto-translation.
            // Bind it to newline directly so the user gets the
            // expected behavior everywhere.
            (KeyCode::Char('j'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.insert_newline_preserving_indent();
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
                // Auto-trigger signature help on every buffer-
                // mutating keystroke. Costs one parse of the (small)
                // in-flight input + a HashMap lookup; bails when the
                // cursor isn't in a known call. Respects existing
                // Completion / Help popups (won't displace them).
                self.refresh_signature_help();
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

    /// Insert bracketed-paste content into the input area, one
    /// line at a time. Embedded newlines become real newlines in
    /// the buffer, NOT Enter events — that's the whole reason
    /// bracketed paste exists: without it, a pasted multi-line
    /// block delivers each `\n` as a submit trigger and every
    /// intermediate line runs as its own command.
    ///
    /// Also drops the history walk (any typed edit — paste
    /// included — starts a fresh history search on the next
    /// Ctrl-P) and turns off scrollback follow so the user can
    /// still scroll up during the paste render.
    /// Insert a newline and re-emit the CURRENT line's leading
    /// whitespace on the new line. Matches how editors (and
    /// Claude Code's TUI) behave on Shift+Enter — hitting it
    /// inside a `-flag`-continued command keeps you at the same
    /// column so `-foo\n  -bar` becomes `-foo\n  -bar\n  |cursor`
    /// instead of `-foo\n  -bar\n|cursor`. Only whitespace is
    /// copied (spaces + tabs) — never the actual line content.
    ///
    /// Applies at the CURRENT cursor row, not the top row: if the
    /// user is mid-line and hits Ctrl+J, they see indent-copied
    /// behavior on the CURRENT line's indent, matching every
    /// other editor's rule.
    fn insert_newline_preserving_indent(&mut self) {
        let (row, _) = self.input.cursor();
        let indent: String = self.input.lines()[row]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        self.input.insert_newline();
        for ch in indent.chars() {
            self.input.insert_char(ch);
        }
    }

    fn handle_paste(&mut self, data: String) {
        self.history_cursor = None;
        let mut first = true;
        for line in data.split('\n') {
            if !first {
                self.input.insert_newline();
            }
            first = false;
            // Strip carriage returns some terminals prepend (CRLF
            // sources on Windows / some remote sessions).
            for ch in line.chars().filter(|&c| c != '\r') {
                self.input.insert_char(ch);
            }
        }
    }

    async fn on_submit(&mut self) {
        let text = self.current_input_text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if !is_buffer_complete(
            &text,
            &self.session.read().unwrap().signature_table(),
        ) {
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
        let session_guard = self.session.read().unwrap();
        resolve_stack_frames(
            msg,
            &session_guard,
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

        // Lower htcl → Tcl on a blocking thread so the event
        // loop can render the input echo + tick the per-input
        // timer during the (potentially minute-scale) parse +
        // validate + lower work. `prepare` is fully synchronous
        // CPU + I/O; spawn_blocking is the right primitive.
        //
        // The main task returns immediately after spawning —
        // `handle_worker_event` picks up `WorkerEvent::PrepareDone`
        // when the background thread finishes, and continues the
        // dispatch pipeline from there.
        // Show a "preparing…" sentinel only when the input will
        // actually pull new files through the loader — i.e., the
        // top-level parse of `text` contains a `src` command.
        // A one-liner like `bd::clobber -name txr0` doesn't touch
        // disk and prepare finishes in low double-digit ms; a
        // notice per submit for that case is just noise.
        //
        // The parse itself is cheap (input is usually a single
        // line); prepare will re-parse the flat post-load source
        // internally either way. False negatives are impossible
        // — `src` is the only mechanism that adds files — and
        // false positives are bounded to "user typed `src`
        // without triggering big work" (already-preloaded
        // target), where a brief notice does no harm.
        let will_load = {
            let parsed = vw_htcl::parse(&text);
            parsed.document.stmts.iter().any(|s| {
                matches!(
                    s,
                    vw_htcl::Stmt::Command(c)
                        if matches!(c.kind, vw_htcl::CommandKind::Src(_))
                )
            })
        };
        if will_load {
            self.push(
                ScrollbackKind::Notice,
                "preparing… (parsing + validating imports)".into(),
            );
        }

        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let session = std::sync::Arc::clone(&self.session);
        let event_tx = self.event_tx.clone();
        let text_moved = text;
        tokio::task::spawn_blocking(move || {
            // Catch panics on the blocking thread so a bug in
            // prepare surfaces as an ERROR row rather than a
            // silent stall. Without this the JoinHandle we drop
            // absorbs the panic and the UI sits waiting forever
            // for a PrepareDone that will never come.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let session_guard = session.read().unwrap();
                    crate::lower::prepare(&text_moved, &cwd, &session_guard)
                }));
            match result {
                Ok(r) => {
                    let _ = event_tx.send(WorkerEvent::PrepareDone {
                        text: text_moved,
                        echo,
                        result: r,
                    });
                }
                Err(payload) => {
                    // Turn the panic message into a LowerError so
                    // the existing PrepareDone/handle_prepare_done
                    // path renders it as an ERROR row.
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic in prepare".to_string()
                    };
                    let _ = event_tx.send(WorkerEvent::PrepareDone {
                        text: text_moved,
                        echo,
                        result: Err(crate::lower::LowerError::Parse(format!(
                            "prepare panicked: {msg}",
                        ))),
                    });
                }
            }
        });
    }

    /// Continuation of `dispatch_eval_with_echo` — receives the
    /// completed `Prepared` (or error) from the background prepare
    /// task and finishes the dispatch pipeline: surfaces warnings,
    /// commits pure-`src` imports directly, otherwise builds the
    /// per-input timer boundaries and ships commands to the worker.
    async fn handle_prepare_done(
        &mut self,
        _text: String,
        echo: bool,
        result: Result<crate::lower::Prepared, crate::lower::LowerError>,
    ) {
        let lowered = match result {
            Ok(l) => l,
            Err(e) => {
                self.push(ScrollbackKind::Error, format!("ERROR: {e}"));
                // Prepare failed — no eval will run, so the Input
                // entry's timer would otherwise tick forever. Freeze
                // it at the "prepare failed" wall time. Same freeze
                // path the empty-batch case (line ~2234) uses.
                self.mark_inputs_completed();
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
            self.session.write().unwrap().commit(lowered.batch);
            self.sync_preload_from_session();
            self.push(ScrollbackKind::Notice, "(no Tcl to evaluate)".into());
            // The per-input timer was ticking through prepare;
            // freeze it now — this batch has nothing to eval.
            self.mark_inputs_completed();
            return;
        }

        // Build per-Input-entry timer boundaries. Empty when not
        // in echo mode (the non-echo single-Input case uses the
        // existing `mark_inputs_completed` end-of-batch path).
        //
        // Echo model: strictly linear. The batch's FIRST
        // statement is echoed to scrollback now; every subsequent
        // statement is registered as a boundary with
        // `scrollback_idx: None` and echoed lazily by
        // `advance_input_timers` when the prior boundary closes.
        // That way a `:load prime.htcl` run reads like an
        // interactive session — each command appears, its output
        // and messages follow, then the next command appears.
        let mut input_boundaries: Vec<InputBoundary> = Vec::new();
        if echo {
            for origin in &lowered.entry_top_level {
                input_boundaries.push(InputBoundary {
                    scrollback_idx: None,
                    snippet: origin.snippet.clone(),
                    last_command_idx: None, // filled below
                    completed: false,
                });
            }
            // For each entry-top-level Origin, find the LAST
            // lowered command whose ultimate entry-file line
            // matches it. A command's "entry line" is the line
            // in the entry file it came from: directly when
            // `origin.via` is empty (the command lives in the
            // entry), or the bottom of the `via` chain (which
            // lower.rs documents as "the last frame is the
            // entry file / user input").
            for (cmd_idx, cmd) in lowered.commands.iter().enumerate() {
                let entry_line = match cmd.origin.via.last() {
                    Some(f) => f.line,
                    None => cmd.origin.line,
                };
                // Find which entry_top_level Origin this matches
                // (linear scan — at most a handful of top-level
                // statements per batch).
                for (j, top) in lowered.entry_top_level.iter().enumerate() {
                    if top.line == entry_line {
                        if let Some(b) = input_boundaries.get_mut(j) {
                            b.last_command_idx = Some(cmd_idx);
                        }
                        break;
                    }
                }
            }
        }
        self.pending_input_boundaries = input_boundaries;
        if echo {
            // Push the first non-empty boundary's echo NOW so the
            // user sees `› <first statement>` before the batch
            // starts producing output. `activate_next_boundary`
            // also handles the edge case of a leading empty
            // boundary (a `src` whose target file lowered to zero
            // commands): it echoes, freezes, and cascades.
            self.activate_next_boundary(0);
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
        self.pending_is_set_binding =
            lowered.commands.iter().map(|c| c.is_set_binding).collect();
        self.pending_eval_index = 0;

        // Seed the shared preload map with this batch's file
        // list BEFORE dispatching. Commands ship in document
        // order, so any RPC that fires from a command MID-batch
        // (currently only `compile_htcl_module` from
        // `vw::configure_ip`) can trust that every proc from
        // every file preceding it in the same batch is already
        // installed in Vivado. Without this, the batch's own
        // `src @vw` recursion doesn't reach the preload until
        // AFTER the whole batch completes — which means the
        // first `vw::configure_ip` call re-parses + re-lowers
        // + re-ships @vw + @vivado-cmd (~10MB of Tcl,
        // multiple minutes). Preload-then-dispatch turns that
        // into "just workspace-local files" and shrinks the
        // compile output by ~100×.
        //
        // Safety: the invariant on `SharedPreload` is "files
        // whose Tcl has been eval'd by Vivado". Populating from
        // this batch's file list before eval TECHNICALLY breaks
        // the letter of that rule for the window between
        // dispatch and completion — but for the specific caller
        // that uses the preload (`compile_htcl_module`, invoked
        // from mid-batch procs), the OR pending commands run
        // strictly BEFORE the invocation, so the corresponding
        // procs ARE installed by the time the RPC fires.
        if let Ok(mut g) = self.preload.write() {
            for f in &lowered.batch.program.files {
                if let Some(t) = f.mtime {
                    g.insert(f.path.clone(), t);
                }
            }
        }
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
        // Note for completion: keep [`META_COMMANDS`] in sync with
        // the arms of this `match`.
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
            "libs" => {
                // List every library the session knows about + its
                // symbol count, sorted by descending count. Built
                // from the same SymbolIndex the Ctrl-T picker uses,
                // so the totals stay consistent across surfaces.
                let parsed_input =
                    vw_htcl::parser::parse(&self.current_input_text());
                let session = std::sync::Arc::clone(&self.session);
                let session_guard = session.read().unwrap();
                let index = crate::symbol_index::SymbolIndex::build(
                    &session_guard,
                    self.pending_batch.as_ref(),
                    Some(&parsed_input.document),
                );
                let libs = index.libraries();
                if libs.is_empty() {
                    self.push(
                        ScrollbackKind::Notice,
                        "no libraries loaded".to_string(),
                    );
                } else {
                    // Column widths: count gets 5 cells, library
                    // name takes the max of its actual lengths.
                    let max_name = libs
                        .iter()
                        .map(|l| l.library.display().chars().count())
                        .max()
                        .unwrap_or(8);
                    let mut out = String::new();
                    out.push_str(&format!(
                        "{:>5}  {:<width$}  path\n",
                        "syms",
                        "library",
                        width = max_name
                    ));
                    out.push_str(&"─".repeat(max_name + 20));
                    out.push('\n');
                    for info in &libs {
                        let name = info.library.display();
                        let path = match &info.library {
                            crate::symbol_index::LibraryRef::Entry => {
                                "<typed at REPL>".to_string()
                            }
                            crate::symbol_index::LibraryRef::Import {
                                path,
                                ..
                            } => path.display().to_string(),
                        };
                        out.push_str(&format!(
                            "{:>5}  {:<width$}  {}\n",
                            info.symbol_count,
                            name,
                            path,
                            width = max_name
                        ));
                    }
                    self.push(
                        ScrollbackKind::Notice,
                        out.trim_end().to_string(),
                    );
                }
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
            WorkerEvent::Started { child_pid } => {
                self.worker_state = WorkerState::Ready;
                self.child_pid = child_pid;
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
            WorkerEvent::PrepareDone { text, echo, result } => {
                self.handle_prepare_done(text, echo, result).await;
            }
            WorkerEvent::Stream { kind, data } => {
                // Feed the chunk into the block accumulator: NONE
                // blocks (Vivado tables / banners / license chatter)
                // group into one collapsible scrollback entry per
                // run of Stdout chunks, while Diagnostic chunks
                // (INFO / WARNING / CRITICAL / ERROR) flush any
                // pending NONE and emit themselves at full fidelity.
                for block in self.block_acc.push(kind, &data) {
                    match block {
                        vw_vivado::Block::None { lines } => {
                            self.push_none_block(lines);
                        }
                        vw_vivado::Block::Diagnostic { severity, lines } => {
                            let is_critical = matches!(
                                severity,
                                vw_vivado::Severity::CriticalWarning
                            );
                            let scrollback_kind = match severity {
                                vw_vivado::Severity::Info => {
                                    ScrollbackKind::Notice
                                }
                                vw_vivado::Severity::Warning => {
                                    ScrollbackKind::Warning
                                }
                                // Critical warnings render with the
                                // same red ✗ treatment as ERROR in
                                // scrollback — the block classifier
                                // keeps them semantically distinct
                                // for log-level filtering, but the
                                // visual severity is the same.
                                // `is_critical` above carries the
                                // discriminator through to the
                                // scrollback entry for the finder.
                                vw_vivado::Severity::CriticalWarning
                                | vw_vivado::Severity::Error => {
                                    ScrollbackKind::Error
                                }
                                // Segmenter guarantees Diagnostic
                                // blocks are never Severity::None,
                                // but the arm has to exist for the
                                // match to be exhaustive.
                                vw_vivado::Severity::None => {
                                    ScrollbackKind::Stdout
                                }
                            };
                            let joined = lines.join("\n");
                            let resolved = self.resolve_stack_frames(&joined);
                            let tagged = self.tag_streamed_message(
                                scrollback_kind,
                                resolved,
                            );
                            let (body, stack) = split_body_and_stack(&tagged);
                            self.push_diag(scrollback_kind, body, is_critical);
                            if let Some(stack) = stack {
                                self.push_stack_trace(stack);
                            }
                        }
                    }
                }
            }
            WorkerEvent::EvalDone {
                origin,
                result,
                last_in_batch,
            } => {
                // Flush any pending NONE-block content the segmenter
                // has been holding. Pure `puts` output arrives as
                // Stdout chunks (Severity::None) and never emits from
                // the accumulator on its own — the accumulator only
                // flushes when a *classified* chunk (INFO/WARNING/
                // ERROR) arrives after it. Without this flush, plain
                // `puts "muffins"` output sits invisible in
                // `pending_none` until the next diagnostic — which
                // for a quiet REPL never comes. Flushing at
                // EvalDone is the right boundary: every eval's own
                // output should surface before the next input echo.
                for block in self.block_acc.flush() {
                    match block {
                        vw_vivado::Block::None { lines } => {
                            self.push_none_block(lines);
                        }
                        vw_vivado::Block::Diagnostic { severity, lines } => {
                            // Diagnostic-in-flush is unexpected —
                            // the accumulator emits diagnostics
                            // immediately on push, never holds them
                            // pending. Route through the same
                            // scrollback-kind mapping as the
                            // Stream-event path so future accumulator
                            // changes don't silently lose messages.
                            let is_critical = matches!(
                                severity,
                                vw_vivado::Severity::CriticalWarning
                            );
                            let scrollback_kind = match severity {
                                vw_vivado::Severity::Info => {
                                    ScrollbackKind::Notice
                                }
                                vw_vivado::Severity::Warning => {
                                    ScrollbackKind::Warning
                                }
                                vw_vivado::Severity::CriticalWarning
                                | vw_vivado::Severity::Error => {
                                    ScrollbackKind::Error
                                }
                                vw_vivado::Severity::None => {
                                    ScrollbackKind::Stdout
                                }
                            };
                            let joined = lines.join("\n");
                            let resolved = self.resolve_stack_frames(&joined);
                            let tagged = self.tag_streamed_message(
                                scrollback_kind,
                                resolved,
                            );
                            let (body, stack) = split_body_and_stack(&tagged);
                            self.push_diag(scrollback_kind, body, is_critical);
                            if let Some(stack) = stack {
                                self.push_stack_trace(stack);
                            }
                        }
                    }
                }
                // Grab the return type + set-binding flag for
                // THIS command (the one that just finished)
                // before we advance the index and possibly clear
                // the buffer.
                let finished_is_set_binding = self
                    .pending_is_set_binding
                    .get(self.pending_eval_index)
                    .copied()
                    .unwrap_or(false);
                let finished_return_type = self
                    .pending_return_types
                    .get(self.pending_eval_index)
                    .cloned()
                    .flatten();
                // Capture the just-finished command's eval-index
                // before we advance — used to freeze any Input
                // entry whose last-command boundary matches it.
                let just_finished_idx = self.pending_eval_index;
                // Advance past the command that just finished — the
                // stream-tagging path uses `pending_origins[index]`
                // to label warnings emitted by the *currently*
                // executing command, so the index should always
                // point at "in-flight," not "just done."
                self.pending_eval_index =
                    self.pending_eval_index.saturating_add(1);
                // Per-statement timer freezing: if any echoed
                // Input entry's `last_command_idx` matches the
                // just-finished command, stamp its
                // `completed_at`. On success we cascade — activate
                // (echo + stamp `started_at` for) the next
                // uncompleted boundary. On failure we do NOT
                // cascade: the batch aborts here, and echoing the
                // NEXT statement's `› …` line before the error
                // trace we're about to render would make the trace
                // look like it belonged to that statement. The
                // ordering guarantee we're preserving is: an
                // eval's output — including its error trace when
                // the eval fails — appears between its own echo
                // and the next one, if any next one appears at all.
                match &result {
                    Ok(_) => self.advance_input_timers(just_finished_idx),
                    Err(_) => {
                        self.freeze_input_boundary(just_finished_idx);
                    }
                }
                if last_in_batch {
                    self.pending_origins.clear();
                    self.pending_return_types.clear();
                    self.pending_input_boundaries.clear();
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
                            //   - Untyped expressions we can't repr
                            //     through the type's proc: skip the
                            //     push. `out.value` in this case
                            //     is the raw Tcl representation
                            //     (`{Scalar x}` tagged-list form
                            //     for our nested Properties trees)
                            //     — displaying that leaks the
                            //     internal encoding rather than
                            //     the compiler-emitted
                            //     `Variant(payload)` repr shape.
                            //     If a caller wants to inspect an
                            //     untyped value, `puts` is the
                            //     explicit form.
                            //   - `set VAR <expr>` is a binding,
                            //     not a display. The value the
                            //     user asked to name is now bound;
                            //     showing it isn't part of what
                            //     they wrote. Same suppression
                            //     applies for consistency with
                            //     the "no unrepr'd values leak"
                            //     rule above.
                            let suppress_unit = matches!(
                                finished_return_type.as_ref(),
                                Some(vw_htcl::TypeExpr::Named { name, .. })
                                    if name == "unit"
                            );
                            let suppress_untyped =
                                finished_return_type.is_none();
                            let suppress = suppress_unit
                                || suppress_untyped
                                || finished_is_set_binding;
                            if !suppress && !out.value.is_empty() {
                                // finished_return_type is Some
                                // here (untyped is suppressed
                                // above), so the wrap_with_repr
                                // path has already rendered
                                // through the type's `repr`
                                // proc. Push verbatim.
                                let text = out.value.clone();
                                self.push(ScrollbackKind::Result, text);
                            }
                            if let Some(batch) = self.pending_batch.take() {
                                self.session.write().unwrap().commit(batch);
                                self.sync_preload_from_session();
                            }
                            self.worker_state = WorkerState::Ready;
                            // Freeze per-input timers at their
                            // final duration now that the batch
                            // has finished evaluating.
                            self.mark_inputs_completed();
                        }
                    }
                    Err(err) => {
                        self.worker_state = WorkerState::Ready;
                        // Hold the pending batch for the renderer
                        // — drill-down lookups need its proc map.
                        // Cleared below once the trace is emitted.
                        render_eval_error(self, &origin, err);
                        // Commit the parsed batch to the session
                        // even though the eval failed. The batch's
                        // `Document` carries every proc, type, and
                        // enum decl that the parser saw — including
                        // whatever `src @vivado-cmd`, `src project`,
                        // `src ip/cips`, etc. brought in *before*
                        // the failing user command ran. Tab
                        // completion, hover, and signature help all
                        // query session-wide symbols, and dropping
                        // the batch strands them with an empty
                        // symbol table until the next successful
                        // eval. The runtime state in Vivado may be
                        // partial or wrong; that's a separate
                        // concern from what the analyzer sees.
                        if let Some(batch) = self.pending_batch.take() {
                            self.session.write().unwrap().commit(batch);
                            self.sync_preload_from_session();
                        }
                        // Failed evals also freeze their per-input
                        // timer — otherwise the live counter would
                        // tick forever on an error result.
                        self.mark_inputs_completed();
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
        //
        // Input entries get a start timestamp so the renderer can
        // show a per-input timer (live while running, frozen on
        // batch completion). Other kinds leave timing unset.
        let started_at = if matches!(kind, ScrollbackKind::Input) {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // Expand tabs to spaces at push time so BOTH `entry_lines_
        // windowed` (which builds rendered spans) and `count_wrapped_
        // rows` (which does the outer viewport math) see the same
        // character content. If a `\t` survives into the render
        // pipeline, ratatui's Buffer writes a `\t` cell that
        // iTerm2 (and any real terminal) interprets as "move
        // cursor to next tab stop" — a control code, not a
        // printable glyph. Cells between the cursor's start
        // column and the tab stop stay whatever they were in the
        // previous frame, producing the `728_` / `FO:` / `.v:`
        // leftover fragments Vivado's parameter-dump output was
        // showing. Four spaces per tab matches typical editor
        // defaults and keeps the expansion fixed-count so
        // downstream width math (`chars().count()`) stays honest.
        let text = if text.contains('\t') {
            text.replace('\t', "    ")
        } else {
            text
        };
        // Uniform Mathematica-notebook-style collapsibility: every
        // multi-line entry is toggleable (Shift+click), and
        // anything larger than COLLAPSE_AUTO_THRESHOLD lines starts
        // collapsed so a wall of text doesn't dominate the
        // scrollback. Single-line entries get `None` — a placeholder
        // for something that fits in one row is worse UX than just
        // showing the row itself.
        let collapse_state = compute_collapse_state(&text, self.collapse_mode);
        self.scrollback.push(ScrollbackEntry {
            kind,
            text,
            started_at,
            completed_at: None,
            collapse_state,
            is_critical_warning: false,
            parent_input_idx: if matches!(kind, ScrollbackKind::Input) {
                None
            } else {
                self.current_input_idx
            },
            group_collapsed: matches!(kind, ScrollbackKind::Input),
            error_child_count: 0,
            warning_child_count: 0,
        });
        if matches!(kind, ScrollbackKind::Input) {
            self.current_input_idx = Some(self.scrollback.len() - 1);
        }
        // Bump the parent input's severity tally so the
        // collapsed header can render ✗ / ⚠ badges without
        // expanding. Only the two "actionable" kinds count —
        // Notices, Stdout, Result, Chatter don't warrant a
        // header badge.
        match kind {
            ScrollbackKind::Error => {
                if let Some(pidx) = self.current_input_idx {
                    if let Some(p) = self.scrollback.get_mut(pidx) {
                        p.error_child_count =
                            p.error_child_count.saturating_add(1);
                    }
                }
            }
            ScrollbackKind::Warning => {
                if let Some(pidx) = self.current_input_idx {
                    if let Some(p) = self.scrollback.get_mut(pidx) {
                        p.warning_child_count =
                            p.warning_child_count.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }

    /// Push a diagnostic that came from the Vivado stream, tagging
    /// it as a CRITICAL WARNING when the source severity was
    /// `Severity::CriticalWarning`. CW entries share
    /// [`ScrollbackKind::Error`]'s red gutter but carry the
    /// [`ScrollbackEntry::is_critical_warning`] flag so the
    /// diagnostics-finder popup can offer a `Critical` filter
    /// checkbox that surfaces just them.
    pub(crate) fn push_diag(
        &mut self,
        kind: ScrollbackKind,
        text: String,
        is_critical_warning: bool,
    ) {
        // Split the tagged first line off. For any diagnostic —
        // ERROR / CRITICAL WARNING / WARNING — the leading
        // `<LEVEL>: [<designator>] <message>` line MUST stay
        // full-brightness and never dim-collapse, or a real
        // problem in the middle of dozens of INFO lines
        // disappears. Trailing content (Resolution hints,
        // continuations) is fine to auto-collapse — that's what
        // `split_leading_diagnostic` gives us. Notice / Stdout
        // kinds don't tag their leading line, so the split just
        // returns `(text, None)` and the entry pushes as before.
        let (leading, trailing) = split_leading_diagnostic(&text);
        let leading = if leading.contains('\t') {
            leading.replace('\t', "    ")
        } else {
            leading
        };
        // The leading line pushes as its own entry with
        // `collapse_state = None` (via the <2-lines branch of
        // `compute_collapse_state`) so it renders at full
        // brightness. Only CW entries carry the flag — this is
        // what feeds the diagnostics-finder's `Critical` filter.
        let leading_collapse =
            compute_collapse_state(&leading, self.collapse_mode);
        self.scrollback.push(ScrollbackEntry {
            kind,
            text: leading,
            started_at: None,
            completed_at: None,
            collapse_state: leading_collapse,
            is_critical_warning,
            // Diagnostics always belong to the currently
            // executing input group. `kind` here is never
            // `Input` — the classifier routes Input echoes
            // through the plain `push` path.
            parent_input_idx: self.current_input_idx,
            group_collapsed: false,
            error_child_count: 0,
            warning_child_count: 0,
        });
        // Bump the parent input's severity tally. push_diag is
        // the only path that flags is_critical_warning, so the
        // Error / CW check has to live here alongside the
        // `push`-side check (the two deliberately don't share
        // code — push_diag has its own leading/trailing split).
        // CRITICAL WARNINGs count as errors — same red gutter,
        // same ✗ badge — matching how the diagnostics finder
        // buckets them.
        match kind {
            ScrollbackKind::Error => {
                if let Some(pidx) = self.current_input_idx {
                    if let Some(p) = self.scrollback.get_mut(pidx) {
                        p.error_child_count =
                            p.error_child_count.saturating_add(1);
                    }
                }
            }
            ScrollbackKind::Warning => {
                if let Some(pidx) = self.current_input_idx {
                    if let Some(p) = self.scrollback.get_mut(pidx) {
                        p.warning_child_count =
                            p.warning_child_count.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
        if is_critical_warning && !matches!(kind, ScrollbackKind::Error) {
            // Belt-and-suspenders: CriticalWarning severity
            // always classifies to `Error` kind at the stream
            // level, but keep the flag path independent so a
            // future rerouting doesn't silently drop the badge.
            if let Some(pidx) = self.current_input_idx {
                if let Some(p) = self.scrollback.get_mut(pidx) {
                    p.error_child_count = p.error_child_count.saturating_add(1);
                }
            }
        }
        if let Some(trailing) = trailing {
            let trailing = if trailing.contains('\t') {
                trailing.replace('\t', "    ")
            } else {
                trailing
            };
            let collapse_state =
                compute_collapse_state(&trailing, self.collapse_mode);
            self.scrollback.push(ScrollbackEntry {
                kind: ScrollbackKind::Chatter,
                text: trailing,
                started_at: None,
                completed_at: None,
                collapse_state,
                is_critical_warning: false,
                // Trailing content attaches to the diagnostic
                // just pushed above, which is a child of the
                // current input group — so this belongs to the
                // same group.
                parent_input_idx: self.current_input_idx,
                group_collapsed: false,
                error_child_count: 0,
                warning_child_count: 0,
            });
        }
    }

    /// Push a NONE-severity block: the accumulated non-diagnostic
    /// chatter between two classified messages (Vivado tables,
    /// section headers, banners, `VHDL Output written to …` lines).
    /// Always uses [`ScrollbackKind::Chatter`] — the "background
    /// noise" bucket that carries the dim dark-gray body style so
    /// non-diagnostic output visually reads as elidable. Whether
    /// the entry is collapsed / expanded / not-collapsible is
    /// decided by [`Self::push`]'s threshold logic (multi-line
    /// entries over the auto-collapse threshold start collapsed;
    /// smaller ones expand).
    pub(crate) fn push_none_block(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        self.push(ScrollbackKind::Chatter, lines.join("\n"));
    }

    /// Push a stack trace as its own entry, always Chatter-styled
    /// and always start-collapsed when it has 2+ lines. Used by the
    /// diagnostic stream path to split the `at <path>:<line>` tail
    /// off a WARNING/ERROR body so the human-readable message
    /// stays fully visible while the stack becomes a
    /// `▶ at <first-frame>` placeholder — Shift+click to expand.
    /// Bypasses [`Self::push`]'s auto-threshold: even a 3-frame
    /// stack collapses, which is the whole point of the split.
    /// Single-line stacks (one frame) fit in a row and stay
    /// non-collapsible — a placeholder for a 40-char line reads
    /// as noise.
    pub(crate) fn push_stack_trace(&mut self, stack: String) {
        let text = if stack.contains('\t') {
            stack.replace('\t', "    ")
        } else {
            stack
        };
        let collapse_state = if text.lines().count() < 2 {
            None
        } else {
            Some(true)
        };
        self.scrollback.push(ScrollbackEntry {
            kind: ScrollbackKind::Chatter,
            text,
            started_at: None,
            completed_at: None,
            collapse_state,
            is_critical_warning: false,
            // Stack traces always attach to the just-emitted
            // diagnostic; that diagnostic is a child of the
            // current input group, so the trace belongs there too.
            parent_input_idx: self.current_input_idx,
            group_collapsed: false,
            error_child_count: 0,
            warning_child_count: 0,
        });
    }

    /// Per-Input-entry timer advance triggered by an EvalDone.
    /// If `just_finished_idx` matches any uncompleted boundary's
    /// `last_command_idx`, freeze its scrollback entry's
    /// `completed_at` and anchor the next uncompleted boundary's
    /// `started_at` to NOW so its timer begins fresh rather than
    /// inheriting the elapsed time from earlier statements'
    /// commands.
    fn advance_input_timers(&mut self, just_finished_idx: usize) {
        if let Some(hit) = self.freeze_input_boundary(just_finished_idx) {
            // Activate the next uncompleted boundary — pushes its
            // echo and stamps `started_at`. Empty boundaries (no
            // lowered commands attributed) get echoed + frozen
            // instantly and we cascade to the next one; otherwise
            // no EvalDone would ever close them and the batch would
            // stall at that point in the visual trace.
            self.activate_next_boundary(hit + 1);
        }
    }

    /// Freeze the boundary whose `last_command_idx` matches
    /// `just_finished_idx` without cascading to the next
    /// statement. Used by the failing-eval path so an error trace
    /// isn't visually preceded by the next boundary's echo — the
    /// batch is aborting, the next echo would be misleading, and
    /// (worse) it would appear BEFORE the failure explanation the
    /// user needs to read. Returns the boundary's index in
    /// `pending_input_boundaries` when found, or `None` when this
    /// EvalDone doesn't close any boundary (e.g. a synthetic
    /// prelude command).
    fn freeze_input_boundary(
        &mut self,
        just_finished_idx: usize,
    ) -> Option<usize> {
        let now = std::time::Instant::now();
        // Find the first uncompleted boundary whose
        // last_command_idx matches. Multi-statement load
        // batches process commands in order, so the matching
        // boundary is always at the head of the uncompleted
        // run.
        let mut hit_position: Option<usize> = None;
        for (i, b) in self.pending_input_boundaries.iter().enumerate() {
            if b.completed {
                continue;
            }
            if b.last_command_idx == Some(just_finished_idx) {
                hit_position = Some(i);
            }
            break;
        }
        let hit = hit_position?;
        self.pending_input_boundaries[hit].completed = true;
        if let Some(idx) = self.pending_input_boundaries[hit].scrollback_idx {
            if let Some(entry) = self.scrollback.get_mut(idx) {
                if entry.completed_at.is_none() {
                    entry.completed_at = Some(now);
                }
            }
        }
        Some(hit)
    }

    /// Push the echo for the first uncompleted boundary starting
    /// at `start`, stamp its `started_at`, and freeze-and-cascade
    /// past any empty boundaries encountered along the way. Used
    /// both at batch dispatch (starting from index 0) and by
    /// `advance_input_timers` when the prior boundary closes.
    fn activate_next_boundary(&mut self, start: usize) {
        let now = std::time::Instant::now();
        let mut i = start;
        while i < self.pending_input_boundaries.len() {
            if self.pending_input_boundaries[i].completed {
                i += 1;
                continue;
            }
            let has_commands =
                self.pending_input_boundaries[i].last_command_idx.is_some();
            // Push echo lazily if we haven't already (the first
            // boundary at batch dispatch may already have a
            // scrollback_idx assigned).
            let idx = match self.pending_input_boundaries[i].scrollback_idx {
                Some(idx) => idx,
                None => {
                    let snippet =
                        self.pending_input_boundaries[i].snippet.clone();
                    let idx = self.scrollback.len();
                    self.push(ScrollbackKind::Input, snippet);
                    self.pending_input_boundaries[i].scrollback_idx = Some(idx);
                    idx
                }
            };
            if let Some(entry) = self.scrollback.get_mut(idx) {
                entry.started_at = Some(now);
            }
            if has_commands {
                // Real boundary — wait for its EvalDone to close it.
                return;
            }
            // Empty boundary: no lowered commands, so no EvalDone
            // will match. Freeze it now with a zero-second timer
            // and cascade to the next.
            self.pending_input_boundaries[i].completed = true;
            if let Some(entry) = self.scrollback.get_mut(idx) {
                entry.completed_at = Some(now);
            }
            i += 1;
        }
    }

    /// Stamp `completed_at` on every still-running Input entry
    /// from the most recent batch. Called from the `EvalDone`
    /// handler on `last_in_batch` so the per-input timers freeze
    /// at their final duration once the batch has finished
    /// evaluating. For `--load` echoed batches with multiple
    /// Input entries (one per top-level statement) all entries
    /// freeze at the same wall time — finer-grained per-statement
    /// timing would require carrying the eval-to-input mapping
    /// through the worker round-trip, which is more plumbing
    /// than the v1 timer needs.
    fn mark_inputs_completed(&mut self) {
        let now = std::time::Instant::now();
        for entry in self.scrollback.iter_mut().rev() {
            if matches!(entry.kind, ScrollbackKind::Input)
                && entry.completed_at.is_none()
            {
                entry.completed_at = Some(now);
            } else if entry.completed_at.is_some()
                && matches!(entry.kind, ScrollbackKind::Input)
            {
                // Already-completed Input from a prior batch —
                // we've walked past the current batch's inputs.
                break;
            }
        }
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
        } else if delta > 0 && new >= self.last_max_scroll {
            // Downward scroll that reaches (or passes) the bottom
            // re-engages tail-follow — standard terminal-emulator
            // behavior. Safe now that `last_max_scroll` is the
            // renderer's exact wrapped-row math, not the old
            // raw-line-count heuristic that misfired on wrapped
            // multi-MB entries (spuriously snapping back to bottom
            // on any scroll-up and making scroll appear dead).
            self.scrollback_follow = true;
        }
        self.scrollback_scroll = new;
        // Predictively mirror the new offset into
        // `last_rendered_scroll`. Without this, drag-to-select
        // auto-scrolls but the subsequent `cell_to_buffer` call
        // in the same event still uses the previously-rendered
        // value — so the selection cursor lags one drag event
        // behind the scroll. The renderer will write the
        // actually-rendered offset back next frame (which may
        // clamp to max_scroll), so this is at worst a one-frame
        // optimistic preview.
        self.last_rendered_scroll = new;
    }
}

// ---------------------------------------------------------------------
// Worker task: owns the Vivado backend, serializes evals.
// ---------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn worker_task(
    mut rx: mpsc::Receiver<WorkerCmd>,
    tx: mpsc::UnboundedSender<WorkerEvent>,
    verbose: bool,
    verbose_log: Option<std::path::PathBuf>,
    info_with_stack: bool,
    part: Option<String>,
    variant: Option<String>,
    rpc_workspace_root: Option<std::path::PathBuf>,
    preload: vw_vivado::SharedPreload,
) {
    // Auto-project bootstrap: same rule as `vw run`. If the
    // enclosing workspace declares `[[target-parts]]` or
    // `[[workspace.variants]]`, create an in-memory project up
    // front so `ip::check`, `get_ipdefs`, and every other
    // project-scoped call have a project to read at the first
    // user eval. `part` (part-mode workspaces) and `variant`
    // (variant-mode workspaces) come from `--part` / `--variant`
    // respectively; the workspace shape decides which applies.
    let ws_utf8 = rpc_workspace_root
        .as_deref()
        .and_then(camino::Utf8Path::from_path)
        .map(|p| p.to_path_buf());
    let (auto_project, active_variant) = match ws_utf8.as_deref() {
        Some(ws) => match resolve_worker_selection(
            ws,
            part.as_deref(),
            variant.as_deref(),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                let _ = tx.send(WorkerEvent::StartFailed(
                    vw_eda::BackendError::Worker(e),
                ));
                return;
            }
        },
        None => (None, None),
    };
    // RPC handler — mirrors `vw run`'s. `vw::workspace_root`
    // answers with the entry / cwd's nearest `vw.toml` parent;
    // unknown methods fail loudly so future htcl calls surface
    // a clear "unknown method" instead of hanging. The
    // session-scoped `active_variant` is the fallback the
    // `vhdl_design_sources` filter uses when no per-call kwarg
    // overrides it.
    // Raw byte-log for the session: <workspace>/target/logs/vivado-<ts>.log.
    // Under the REPL's alternate screen we can't safely eprintln! the
    // path (it'd race the TUI), so we swallow errors silently and note
    // the path once the terminal is restored via `info!`.
    let raw_log = rpc_workspace_root.as_deref().and_then(|ws| {
        match vw_vivado::raw_log_path_for_workspace(ws) {
            Ok(p) => {
                tracing::info!(path = %p.display(), "raw vivado log");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "raw log unavailable");
                None
            }
        }
    });
    // RPC handler with the shared preload map — App owns the
    // Arc's other end and updates the map after every
    // `session.commit()` from `session.loaded_paths()`. See the
    // `SharedPreload` docs in vw-vivado.
    //
    // Also carries a session-scoped CW counter. The sink below
    // bumps it on every `Severity::CriticalWarning` chunk, and
    // `vw::synth` / `vw::place` read it via
    // `vw::critical_warning_count` to gate their checkpoint
    // writes on a CW-clean phase. Same wiring as `vw run` — the
    // REPL must not diverge or the exact same htcl session would
    // persist a checkpoint here that `vw run` would refuse.
    let cw_count: vw_vivado::SharedCriticalWarningCount =
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rpc_handler = vw_vivado::make_handler_full(
        rpc_workspace_root,
        active_variant,
        preload,
        cw_count.clone(),
    );
    let backend = vw_vivado::VivadoBackend::spawn(vw_vivado::VivadoConfig {
        verbose,
        verbose_log,
        info_with_stack,
        rpc_handler: Some(rpc_handler),
        auto_project,
        raw_log,
        ..Default::default()
    })
    .await;
    let mut backend = match backend {
        Ok(b) => {
            let _ = tx.send(WorkerEvent::Started {
                child_pid: b.child_pid(),
            });
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
    let cw = cw_count.clone();
    backend.set_stdout_sink(move |kind, chunk: &str| {
        // Bump the CW counter exposed via the
        // `critical_warning_count` RPC. Only exact
        // CriticalWarning — errors already halt eval before any
        // htcl checkpoint-write branch runs, so counting them
        // here would double-report.
        if matches!(kind, vw_vivado::StreamKind::CriticalWarning) {
            cw.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
                    // Wrap each command's Tcl body with a
                    // shim-level origin marker so any traceless
                    // warning it emits stays tagged with THIS
                    // command's origin even when its PTY bytes lag
                    // the protocol response into the next eval's
                    // window. Without this the origin fallback in
                    // `tag_streamed_message` uses whatever
                    // `pending_eval_index` points at — which for a
                    // batch's synthetic prelude commands means
                    // `line=0` / `line=1` no-op tags.
                    let wrapped = crate::wrap_tcl_with_origin_marker(
                        &item.tcl,
                        &item.origin,
                    );
                    let result = backend.eval(&wrapped).await;
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
        let session_guard = app.session.read().unwrap();
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
                .or_else(|| session_guard.lookup_proc(&tcl_frame.proc));
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
    // Split `ERROR: [X] Msg\n    Resolution: …` so the tagged
    // first line pushes as its own single-line entry (full
    // brightness, no `▼` dimming) — see `split_leading_diagnostic`.
    let (leading, trailing) = split_leading_diagnostic(message.trim());
    app.push(ScrollbackKind::Error, leading);
    if let Some(trailing) = trailing {
        app.push(ScrollbackKind::Chatter, trailing);
    }
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
/// point at the actual htcl source file and line. Delegates the
/// per-line parsing + dedup to [`crate::trace`], which is shared
/// with the `vw run` CLI driver so both surfaces render the same.
/// This wrapper closes over the REPL's session+pending proc lookup.
fn resolve_stack_frames(
    msg: &str,
    session: &Session,
    pending: Option<&SessionBatch>,
    input_file: Option<&std::path::Path>,
) -> String {
    crate::trace::resolve_stack_frames_with(
        msg,
        |name| {
            pending
                .and_then(|b| b.procs.get(name))
                .or_else(|| session.lookup_proc(name))
                .cloned()
        },
        input_file,
    )
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
/// Walk a Document looking for `proc <name>` (recursing into
/// `namespace eval` blocks). Returns the `Command.doc_comments`
/// slice when found. Used by the signature-help lookup path to
/// surface the proc's `##` docs alongside its argument list.
///
/// Matches the recursion shape that `vw_htcl::signature_table`
/// uses — qualified names like `util::props` resolve to a proc
/// declared inside `namespace eval util { … }`.
/// Same rule as `vw-cli::resolve_workspace_selection`, mirrored
/// here so the REPL worker doesn't take a cross-crate dep on the
/// CLI. Returns `(auto_project, active_variant)` when the
/// workspace is happy with the flags, `Err(String)` for
/// mode-mismatch or bad selectors.
fn resolve_worker_selection(
    ws: &camino::Utf8Path,
    part: Option<&str>,
    variant: Option<&str>,
) -> Result<(Option<vw_vivado::AutoProject>, Option<String>), String> {
    let Ok(cfg) = vw_lib::load_workspace_config(ws) else {
        return Ok((None, None));
    };
    let ws_info = &cfg.workspace;
    if variant.is_some() && ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} has no `[[workspace.variants]]`; \
             remove `--variant` or add variants to vw.toml",
        ));
    }
    if part.is_some() && !ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} is variant-mode; use `--variant <name>` \
             instead of `--part`",
        ));
    }
    if !ws_info.variants.is_empty() {
        let Some(v) =
            ws_info.select_variant(variant).map_err(|e| e.to_string())?
        else {
            return Ok((None, None));
        };
        let persist_dir = prepare_repl_persist_dir(ws, &ws_info.name);
        return Ok((
            Some(vw_vivado::AutoProject {
                name: ws_info.name.clone(),
                part: v.part.clone(),
                persist_dir,
            }),
            Some(v.name.clone()),
        ));
    }
    let selected = ws_info
        .select_target_part(part)
        .map_err(|e| e.to_string())?;
    let persist_dir = prepare_repl_persist_dir(ws, &ws_info.name);
    Ok((
        selected.map(|p| vw_vivado::AutoProject {
            name: ws_info.name.clone(),
            part: p.to_string(),
            persist_dir: persist_dir.clone(),
        }),
        None,
    ))
}

/// REPL sibling of `vw-cli::prepare_persist_dir`. Same behavior:
/// runs Phase-6 legacy IP-cache cleanup + Phase-3 staleness wipe,
/// returns `Some(<ws>/target/vw-project)` for the worker to
/// `open_project`/`create_project -dir` into, or `None` if the
/// bootstrap fails (falling back to in-memory).
///
/// Kept mirrored (not shared through a common crate) for the same
/// reason `resolve_worker_selection` is duplicated: `vw-repl`
/// doesn't take a cross-crate dep on `vw-cli`.
fn prepare_repl_persist_dir(
    ws: &camino::Utf8Path,
    name: &str,
) -> Option<std::path::PathBuf> {
    match vw_lib::prepare_vw_project_dir(ws, name) {
        Ok(prep) => {
            if prep.legacy_cache_removed > 0 {
                tracing::info!(
                    "removed {} legacy IP cache entr{y} under \
                     {ws}/target/ip — replaced by on-disk Vivado project",
                    prep.legacy_cache_removed,
                    y = if prep.legacy_cache_removed == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                );
            }
            if let Some(wiped) = &prep.wiped_project {
                tracing::info!(
                    "wiped stale Vivado project at {wiped} \
                     (source fingerprint changed or manifest missing)"
                );
            }
            Some(prep.project_dir.into_std_path_buf())
        }
        Err(e) => {
            tracing::warn!(
                "failed to prepare on-disk Vivado project dir under \
                 {ws}/target/vw-project ({e}); falling back to in-memory \
                 project (state won't persist across sessions)"
            );
            None
        }
    }
}

fn lookup_proc_doc_comments<'a>(
    doc: &'a vw_htcl::Document,
    qualified_name: &str,
) -> Option<&'a [String]> {
    lookup_in_stmts(&doc.stmts, "", qualified_name)
}

fn lookup_in_stmts<'a>(
    stmts: &'a [vw_htcl::Stmt],
    prefix: &str,
    qualified_name: &str,
) -> Option<&'a [String]> {
    use vw_htcl::CommandKind;
    for stmt in stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(name) = proc.name.as_deref() {
                    let qualified = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}::{name}")
                    };
                    if qualified == qualified_name {
                        return Some(&cmd.doc_comments);
                    }
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(name) = ns.name.as_deref() {
                    let nested = if prefix.is_empty() {
                        name.to_string()
                    } else {
                        format!("{prefix}::{name}")
                    };
                    if let Some(d) =
                        lookup_in_stmts(&ns.body, &nested, qualified_name)
                    {
                        return Some(d);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Index of the parameter the cursor is on within `sig.args`.
/// Mirrors the logic in `vw_htcl::signature_help::active_parameter`:
/// the active arg is whichever `-flag` was most recently completed
/// on the line, or — when the partial is still being typed — the
/// first arg whose name has the partial as a prefix.
fn compute_active_parameter(
    sig: &vw_htcl::ProcSignature,
    line: &vw_htcl::cmdline::CmdLine<'_>,
) -> Option<u32> {
    let mut active = None;
    for word in line.words.iter().skip(1) {
        if let Some(flag) = word.strip_prefix('-') {
            if let Some(i) = sig.args.iter().position(|a| a.name == flag) {
                active = Some(i as u32);
            }
        }
    }
    if let Some(flag) = line.partial.strip_prefix('-') {
        if !flag.is_empty() {
            if let Some(i) =
                sig.args.iter().position(|a| a.name.starts_with(flag))
            {
                return Some(i as u32);
            }
        }
    }
    active
}

/// Identifier under the cursor — bare-word-shape including the `::`
/// namespace separator so `util::props` resolves as one symbol.
/// Used by the hover lookup path to find what the user is pointing
/// at when [`vw_htcl::hover_at`] can't see the proc (because it's
/// defined in a session batch, not the in-flight input).
fn ident_under_cursor(text: &str, offset: u32) -> Option<&str> {
    let bytes = text.as_bytes();
    let o = (offset as usize).min(bytes.len());
    let is_word_byte =
        |b: u8| -> bool { b.is_ascii_alphanumeric() || b == b'_' || b == b':' };
    let mut start = o;
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = o;
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    if start < end {
        Some(&text[start..end])
    } else {
        None
    }
}

/// Build the title line for a hover popup that points at a proc —
/// shows the proc's name plus its signature in compact one-line form
/// (`name -arg1: type -arg2: type → ret`). The body of the popup is
/// the reflowed doc-comment block.
fn render_proc_title(name: &str, sig: &vw_htcl::ProcSignature) -> String {
    let mut out = name.to_string();
    for arg in &sig.args {
        out.push_str(" -");
        out.push_str(&arg.name);
        if let Some(ty) = arg.type_annotation.as_ref() {
            out.push_str(": ");
            out.push_str(&render_type(ty));
        }
        if let Some(default) = format_default_value(arg) {
            out.push_str(" = ");
            out.push_str(&default);
        }
    }
    if let Some(ret) = sig.return_type.as_ref() {
        out.push_str(" → ");
        out.push_str(&render_type(ret));
    }
    out
}

/// Convert a [`vw_htcl::hover::HoverTarget`] into our owned
/// [`crate::popup::HoverPopup`]. Returns `None` when the target
/// can't be rendered usefully (anonymous procs, missing signatures).
fn hover_target_to_popup(
    target: vw_htcl::HoverTarget<'_>,
    anchor: (u16, u16),
) -> Option<crate::popup::HoverPopup> {
    use vw_htcl::HoverTarget;
    let (title, body) = match target {
        HoverTarget::ProcDef { proc, .. } => {
            let name = proc.name.clone()?;
            let sig = proc.signature.as_ref()?;
            // Doc comments live on the enclosing Command. The hover
            // module doesn't surface them here — accept that we
            // show only the signature for in-buffer proc decls;
            // the user can re-hover the call site for full docs.
            (render_proc_title(&name, sig), String::new())
        }
        HoverTarget::ProcArgDef { arg, .. } => {
            let mut title = format!("-{}", arg.name);
            if let Some(ty) = arg.type_annotation.as_ref() {
                title.push_str(": ");
                title.push_str(&render_type(ty));
            }
            let body = vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
            (title, body)
        }
        HoverTarget::CallSite {
            proc_name,
            signature,
            ..
        } => (render_proc_title(&proc_name, signature), String::new()),
        HoverTarget::CallArg { arg, .. } => {
            let mut title = format!("-{}", arg.name);
            if let Some(ty) = arg.type_annotation.as_ref() {
                title.push_str(": ");
                title.push_str(&render_type(ty));
            }
            let body = vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
            (title, body)
        }
        HoverTarget::LocalVar { name, .. } => {
            (format!("${name}"), String::from("local variable"))
        }
        HoverTarget::EnumDef { decl, .. } => {
            let name = decl.name.clone()?;
            let variants: Vec<String> = decl
                .variants
                .iter()
                .map(|v| {
                    if let Some(ty) = v.payload.as_ref() {
                        format!("{}: {}", v.name, render_type(ty))
                    } else {
                        v.name.clone()
                    }
                })
                .collect();
            let title = format!("enum {name}");
            let body = format!("{{ {} }}", variants.join("; "));
            (title, body)
        }
        HoverTarget::TypeDef { decl, .. } => {
            let name = decl.name.clone()?;
            let title = format!("type {name}");
            let body = match decl.underlying.as_ref() {
                Some(ty) => format!("= {}", render_type(ty)),
                None => String::new(),
            };
            (title, body)
        }
    };
    Some(crate::popup::HoverPopup {
        title,
        body,
        anchor,
    })
}

/// Return the indices of `sig.args` in completion-popup /
/// signature-help **display order**: required arguments (no
/// `@default(...)` attribute) first, then optional ones, both
/// groups sorted alphabetically within. Source declaration order
/// often follows IP-XACT or generator conventions that don't match
/// what users want to scan visually — surfacing required args at
/// the top makes "what MUST I supply?" answerable at a glance.
///
/// The display order is just a permutation of `sig.args` indices;
/// callers map the `active_parameter` (which is computed in
/// declaration-order space) into display space via `.position()`.
fn sorted_arg_indices(sig: &vw_htcl::ProcSignature) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..sig.args.len()).collect();
    indices.sort_by(|&a, &b| {
        let a_arg = &sig.args[a];
        let b_arg = &sig.args[b];
        let a_has_default = a_arg.attribute("default").is_some();
        let b_has_default = b_arg.attribute("default").is_some();
        // false < true → no-default (required) sorts before has-default.
        a_has_default
            .cmp(&b_has_default)
            .then_with(|| a_arg.name.cmp(&b_arg.name))
    });
    indices
}

/// Detail string shown next to a `-flag` row in the completion popup.
/// Combines the arg's type annotation (when present) with its
/// `@default(...)` value (when present). Returns `None` when the
/// arg has neither so the popup row stays compact for untyped
/// undefaulted args.
fn build_flag_detail(arg: &vw_htcl::ast::ProcArg) -> Option<String> {
    let ty = arg.type_annotation.as_ref().map(render_type);
    let default = format_default_value(arg);
    match (ty, default) {
        (None, None) => None,
        (Some(t), None) => Some(t),
        (None, Some(d)) => Some(format!("= {d}")),
        (Some(t), Some(d)) => Some(format!("{t} = {d}")),
    }
}

/// Extract a proc arg's `@default(...)` value, formatted as a short
/// display string. Returns `None` when the arg has no default. Long
/// values (multi-KB paired-dict literals from IP-XACT generators)
/// are truncated to the first ~32 chars with an ellipsis so the
/// signature-help / hover / completion popups don't blow wide.
pub fn format_default_value(arg: &vw_htcl::ast::ProcArg) -> Option<String> {
    let attr = arg.attribute("default")?;
    let first = attr.values.first()?;
    let raw = first.to_tcl_literal();
    const MAX: usize = 32;
    if raw.chars().count() > MAX {
        let truncated: String = raw.chars().take(MAX - 1).collect();
        Some(format!("{truncated}…"))
    } else {
        Some(raw)
    }
}

/// One-line rendering of a `TypeExpr` — `string`, `dict<K, V>`, etc.
/// Used by the signature-help popup to show arg + return types
/// alongside the names.
fn render_type(ty: &vw_htcl::TypeExpr) -> String {
    use vw_htcl::TypeExpr;
    match ty {
        TypeExpr::Named { name, .. } => name.clone(),
        TypeExpr::Generic { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{name}<{}>", inner.join(", "))
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => format!("{namespace}::{variant}"),
    }
}

/// Decide whether the current input buffer is ready to submit.
///
/// Two hard "no": unterminated brace/bracket errors from the parser,
/// or a "missing required argument" diagnostic from the validator.
/// The latter is the key ergonomic hook: when the user types
/// `vivado_cmd::assign_bd_address` alone and hits Enter, the
/// validator sees a call to a proc with required args that haven't
/// been supplied. Treat as incomplete → the REPL appends a newline
/// so the user can continue with `-offset …` on the next line.
/// Once every required arg is supplied, the diagnostic clears and
/// Enter submits.
///
/// The signature table comes from the app's session so procs
/// defined earlier in the same REPL run are visible; passing an
/// empty map degrades gracefully to "unterminated-only" behavior
/// (Slice 5's compositional constructors have required args but
/// aren't in the session table for unit tests).
fn is_buffer_complete(
    text: &str,
    sig_table: &std::collections::HashMap<String, &vw_htcl::ProcSignature>,
) -> bool {
    let parsed = vw_htcl::parse(text);
    if parsed
        .errors
        .iter()
        .any(|e| e.message.contains("unterminated"))
    {
        return false;
    }
    // Ask the validator whether required-arg gaps remain. Any
    // other kind of diagnostic (unknown proc, type error, etc.) is
    // a real error the user should see; incomplete is reserved
    // for the specific "waiting for more flags" state.
    let diags =
        vw_htcl::validate_with_signatures(&parsed.document, text, sig_table);
    let waiting = diags.iter().any(|d| {
        matches!(d.severity, vw_htcl::Severity::Error)
            && d.message.starts_with("missing required argument")
    });
    !waiting
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_sigs(
    ) -> std::collections::HashMap<String, &'static vw_htcl::ProcSignature>
    {
        std::collections::HashMap::new()
    }

    #[test]
    fn buffer_complete_for_simple_statement() {
        assert!(is_buffer_complete("set x 1", &empty_sigs()));
        assert!(is_buffer_complete("puts \"hi\"", &empty_sigs()));
    }

    #[test]
    fn buffer_incomplete_with_unterminated_brace() {
        assert!(!is_buffer_complete(
            "set x [\n  create_cpm5\n    -name cpm5",
            &empty_sigs()
        ));
        assert!(!is_buffer_complete("proc foo {", &empty_sigs()));
    }

    #[test]
    fn buffer_complete_for_multiline_well_formed_proc() {
        assert!(is_buffer_complete(
            "proc foo {\n  @default(1) x\n} {\n  puts $x\n}",
            &empty_sigs()
        ));
    }

    /// The core UX fix: when a proc call is missing required args,
    /// the buffer is considered incomplete so Enter appends a
    /// newline instead of submitting — the user can continue with
    /// `-flag value` continuations.
    #[test]
    fn buffer_incomplete_when_required_args_missing() {
        use vw_htcl::{parse, signature_table};
        let src = "\
proc greet {
  name
  msg
} unit {
  puts \"$msg $name\"
}
";
        let parsed = parse(src);
        let sigs = signature_table(&parsed.document);
        // Bare call — both required args missing.
        assert!(!is_buffer_complete("greet", &sigs));
        // One required arg missing — still incomplete.
        assert!(!is_buffer_complete("greet -name there", &sigs));
        // Both provided — complete.
        assert!(is_buffer_complete("greet -name there -msg hi", &sigs));
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
        let frame = crate::trace::rewrite_stack_line(
            "  at <input>:14 in ::configure_cips",
            |name| session.lookup_proc(name).cloned(),
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
        let frame = crate::trace::rewrite_stack_line(
            "  at <input>:5 in ::port::plumb_if_pin",
            |name| session.lookup_proc(name).cloned(),
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
        assert!(crate::trace::rewrite_stack_line(
            "  at <input>:14 in ::vivado_builtin_thing",
            |name| session.lookup_proc(name).cloned(),
            None,
        )
        .is_none());
    }

    #[test]
    fn rewrite_skips_non_frame_lines() {
        let session = Session::new();
        assert!(crate::trace::rewrite_stack_line(
            "WARNING: [Common 17-1] something",
            |name| session.lookup_proc(name).cloned(),
            None,
        )
        .is_none());
        assert!(crate::trace::rewrite_stack_line(
            "",
            |name| session.lookup_proc(name).cloned(),
            None,
        )
        .is_none());
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

    // --- request/response ordering ---------------------------------

    /// Regression: when a batch's Nth command fails, the (N+1)th
    /// boundary's echo must NOT be pushed to scrollback ahead of the
    /// error trace. The auto-load repro looked like this: `set cips
    /// [configure_cips]` failed, then `› set clk [configure_clock]`
    /// appeared, then the trace + error for `set cips`. The error
    /// belongs to `set cips` and has to land BEFORE the next
    /// statement's echo (or preferably: the next echo shouldn't
    /// appear at all, since the batch aborts on failure).
    #[tokio::test]
    async fn failed_eval_does_not_activate_next_boundary_before_error() {
        let (worker_tx, _worker_rx) =
            tokio::sync::mpsc::channel::<WorkerCmd>(8);
        let (event_tx, event_rx) =
            tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
        let mut app = App::new(
            ReplOptions::default(),
            worker_tx,
            event_rx,
            event_tx,
            CollapseMode::Normal,
            std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        );

        // Two-boundary batch. Only boundary 0's echo lives in
        // scrollback (deferred push means boundary 1 stays lazy
        // until its predecessor closes).
        app.push(
            ScrollbackKind::Input,
            "set cips [configure_cips]".to_string(),
        );
        app.pending_input_boundaries = vec![
            InputBoundary {
                scrollback_idx: Some(0),
                snippet: "set cips [configure_cips]".to_string(),
                last_command_idx: Some(0),
                completed: false,
            },
            InputBoundary {
                scrollback_idx: None,
                snippet: "set clk [configure_clock]".to_string(),
                last_command_idx: Some(1),
                completed: false,
            },
        ];
        let origin0 = crate::lower::Origin {
            file: None,
            line: 14,
            snippet: "set cips [configure_cips]".to_string(),
            via: Vec::new(),
        };
        app.pending_origins = vec![
            origin0.clone(),
            crate::lower::Origin {
                file: None,
                line: 15,
                snippet: "set clk [configure_clock]".to_string(),
                via: Vec::new(),
            },
        ];
        app.pending_return_types = vec![None, None];

        // Failed EvalDone for the first command, last_in_batch=true
        // (mirrors what worker_task sends when it breaks on error).
        let err = vw_eda::BackendError::Tcl {
            message: "[Common 17-163] Missing value for option 'objects'"
                .into(),
            code: None,
            info: None,
            stdout: String::new(),
        };
        app.handle_worker_event(WorkerEvent::EvalDone {
            origin: origin0,
            result: Err(err),
            last_in_batch: true,
        })
        .await;

        // Walk scrollback and find the two positions we care about:
        // the `set clk` Input entry (if any) and the Error entry
        // carrying the failure message.
        let clk_pos = app
            .scrollback()
            .iter()
            .position(|e| e.text.contains("set clk"));
        let err_pos = app.scrollback().iter().position(|e| {
            matches!(e.kind, ScrollbackKind::Error)
                && e.text.contains("Missing value")
        });
        assert!(
            err_pos.is_some(),
            "expected an Error entry, got scrollback: {:#?}",
            app.scrollback()
                .iter()
                .map(|e| (e.kind, e.text.clone()))
                .collect::<Vec<_>>()
        );
        if let Some(clk) = clk_pos {
            let err = err_pos.unwrap();
            assert!(
                err < clk,
                "error at {err} must land before `set clk` echo at \
                 {clk} — the trace belongs to `set cips` which came \
                 first. scrollback: {:#?}",
                app.scrollback()
                    .iter()
                    .map(|e| (e.kind, e.text.clone()))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Regression: a pure `puts` output (which classifies as
    /// `StreamKind::Stdout` → `Severity::None`) has to reach
    /// scrollback even when NO diagnostic follows it. The block
    /// segmenter only flushes pending NONE content when a
    /// classified chunk arrives; without an explicit flush on
    /// `EvalDone`, `puts "muffins"` on a quiet REPL sits invisible
    /// in `pending_none` until the next diagnostic (which for a
    /// small interactive session may never come). The fix: flush
    /// the accumulator at eval-done boundaries so every eval's own
    /// output surfaces before the next input echo.
    #[tokio::test]
    async fn plain_puts_output_flushes_at_eval_done() {
        let (worker_tx, _worker_rx) =
            tokio::sync::mpsc::channel::<WorkerCmd>(8);
        let (event_tx, event_rx) =
            tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
        let mut app = App::new(
            ReplOptions::default(),
            worker_tx,
            event_rx,
            event_tx,
            CollapseMode::Normal,
            std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
        );
        // Emulate the sequence a `puts "muffins"` eval produces:
        // one Stream chunk carrying the output, then EvalDone.
        // Both events land through `handle_worker_event`, same as
        // the real worker task drives them.
        app.handle_worker_event(WorkerEvent::Stream {
            kind: vw_vivado::StreamKind::Stdout,
            data: "muffins\n".to_string(),
        })
        .await;
        // Before EvalDone the accumulator is still holding the
        // chunk — nothing in scrollback yet.
        assert!(
            !app.scrollback().iter().any(|e| e.text.contains("muffins")),
            "muffins should still be in pending_none before EvalDone"
        );
        app.handle_worker_event(WorkerEvent::EvalDone {
            origin: crate::lower::Origin {
                file: None,
                line: 1,
                snippet: "puts \"muffins\"".to_string(),
                via: Vec::new(),
            },
            result: Ok(vw_eda::EvalOutput::default()),
            last_in_batch: true,
        })
        .await;
        // After EvalDone the pending NONE content flushes into
        // scrollback — the user actually sees their output.
        assert!(
            app.scrollback().iter().any(|e| e.text.contains("muffins")),
            "muffins should have flushed into scrollback on EvalDone. \
             scrollback: {:#?}",
            app.scrollback()
                .iter()
                .map(|e| (e.kind, e.text.clone()))
                .collect::<Vec<_>>()
        );
    }
}
