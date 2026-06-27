// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! ratatui rendering for the REPL.
//!
//! Layout (top-to-bottom):
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ scrollback                              ▲│
//! │ (eval log, oldest top, newest bottom)   ║│
//! │                                         ▼│
//! ├──────────────────────────────────────────┤
//! │ input (multi-line, tui-textarea owned)   │
//! ├──────────────────────────────────────────┤
//! │ status: vivado state | hints             │
//! └──────────────────────────────────────────┘
//! ```
//!
//! When Ctrl-R is active, a centered overlay replaces the input area
//! with the search query and the matching history entry.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_textarea::TextArea;

use crate::app::{App, ReverseSearch};

pub fn draw(f: &mut Frame, app: &mut App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                    // scrollback (fills)
            Constraint::Length(input_height(app)), // input
            Constraint::Length(1),                 // status bar
        ])
        .split(f.area());

    draw_scrollback(f, layout[0], app);
    draw_input(f, layout[1], app);
    draw_status(f, layout[2], app);

    if let Some(rs) = app.reverse_search() {
        draw_reverse_search(f, layout[1], rs);
    }
}

fn input_height(app: &App) -> u16 {
    // Start at a 5-line minimum so the user has room to draft a
    // multi-statement entry without the input box flickering taller
    // mid-typing. Grows past 5 with the buffer, capped at 12 so
    // very long entries don't squeeze the scrollback out.
    let lines = app.input_line_count().clamp(5, 12) as u16;
    lines + 2 // +2 for the top/bottom block border
}

fn draw_scrollback(f: &mut Frame, area: Rect, app: &mut App) {
    // Hand the area back to App so mouse-event handlers can
    // translate screen coords into scrollback rows.
    app.set_scrollback_area(area);

    let mut lines: Vec<Line> = Vec::new();
    for entry in app.scrollback() {
        for line in crate::render::entry_lines(entry) {
            lines.push(line);
        }
    }
    // Pre-wrap to area width so screen-row maps 1:1 to a `Line` in
    // the output Vec — that's what makes selection extraction
    // straightforward (vs replaying ratatui's word-wrap to recover
    // the same mapping).
    let mut wrapped = crate::render::wrap_lines(lines, area.width);
    if let Some(sel) = app.selection() {
        let (start, end) = sel.ordered();
        crate::render::apply_selection_highlight(&mut wrapped, start, end);
    }
    // Tail-follow: in follow mode the effective scroll is
    // computed each frame from the wrapped row total — free here
    // because `wrapped` is already built — rather than recomputing
    // it on every `push()`. The renderer also writes back the
    // chosen offset so the manual scroll handlers can anchor their
    // deltas off the actually-rendered position.
    let max_scroll = wrapped.len().saturating_sub(area.height as usize) as u16;
    let scroll_offset = if app.scrollback_follow() {
        max_scroll
    } else {
        app.scrollback_scroll().min(max_scroll)
    };
    app.set_last_rendered_scroll(scroll_offset);
    // No surrounding block: the scrollback's main job is to be
    // copy-pastable. A box-drawing border around each visible row
    // means any selection that spans full lines pulls in `│` chars
    // at the start and end of every line, which is what the user
    // reads. The input box below the scrollback still has its own
    // border, which provides enough visual separation between the
    // two regions.
    let paragraph = Paragraph::new(wrapped).scroll((scroll_offset, 0));
    f.render_widget(paragraph, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    // tui-textarea renders itself with its current cursor; the
    // surrounding block provides a visual frame.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(input_title(app));
    let ta: &mut TextArea<'static> = app.input_mut();
    ta.set_block(block);
    f.render_widget(&*ta, area);
}

fn input_title(app: &App) -> String {
    if app.eval_in_flight() {
        " input — vivado: running ".to_string()
    } else if app.input_is_complete() {
        " input — Enter to run ".to_string()
    } else {
        " input — Enter for newline (parse incomplete) ".to_string()
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let (label, bg) = match app.worker_state() {
        // Indigo when Vivado is sitting idle, ready for input —
        // the "you can interact" steady state.
        WorkerStatusView::Ready => (" vivado: ready ", Color::Rgb(75, 0, 130)),
        // Orange whenever Vivado is anything but ready — starting
        // up, mid-eval, or dead. Catches the eye so the user
        // notices they can't (yet, or any longer) drive the
        // session.
        WorkerStatusView::Starting => {
            (" vivado: starting ", Color::Rgb(255, 140, 0))
        }
        WorkerStatusView::Running => {
            (" vivado: running ", Color::Rgb(255, 140, 0))
        }
        WorkerStatusView::Down => (" vivado: down ", Color::Rgb(255, 140, 0)),
    };
    let hint = if app.reverse_search().is_some() {
        "Esc cancel · Enter accept · Ctrl-R older".to_string()
    } else {
        let mouse = if app.mouse_capture() {
            "F2 terminal-sel"
        } else {
            "F2 mouse-on"
        };
        format!(
            "Ctrl-D exit · Ctrl-P/N history · Ctrl-R search · \
             Ctrl-K/J or wheel scroll · \
             drag to copy · {mouse} · :load <file> · :quit"
        )
    };
    // Split the status bar into [hint (left, fills) | status
    // indicator (right, fixed width)] so the status badge always
    // anchors to the bottom-right corner.
    let badge_width = label.chars().count() as u16;
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(badge_width)])
        .split(area);
    f.render_widget(
        Paragraph::new(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )),
        layout[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default()
                .bg(bg)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        layout[1],
    );
}

fn draw_reverse_search(f: &mut Frame, anchor: Rect, rs: &ReverseSearch) {
    let area = centered_rect(80, 5, f.area(), anchor);
    f.render_widget(Clear, area);
    let title = format!(
        " reverse-i-search ({}) ",
        if rs.match_index.is_some() {
            "match"
        } else if rs.query.is_empty() {
            "type to search"
        } else {
            "no match"
        }
    );
    let body = vec![
        Line::from(vec![
            Span::styled("query: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                rs.query.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::raw("")),
        Line::from(vec![
            Span::styled("match: ", Style::default().fg(Color::DarkGray)),
            Span::raw(rs.match_text.clone()),
        ]),
    ];
    let para = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().bg(Color::Black)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

/// Compute a centered rectangle for a popup. `anchor` is the area the
/// popup is logically attached to (the input area); we expand around
/// the screen center but never overflow the parent.
fn centered_rect(
    percent_x: u16,
    height_lines: u16,
    full: Rect,
    _anchor: Rect,
) -> Rect {
    let popup_w = full.width.saturating_mul(percent_x) / 100;
    let popup_h = height_lines.min(full.height);
    let x = (full.width.saturating_sub(popup_w)) / 2;
    let y = (full.height.saturating_sub(popup_h)) / 2;
    Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    }
}

/// Worker status as the UI sees it. Lives here (not in `app`) so the
/// renderer doesn't have to know about the worker's internal state
/// machine.
pub enum WorkerStatusView {
    Starting,
    Ready,
    Running,
    Down,
}
