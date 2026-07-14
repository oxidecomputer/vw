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

use crate::app::{App, ReverseSearch};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;

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
    if let Some(popup) = app.popup_state() {
        match popup {
            crate::popup::PopupState::Completion(c) => {
                crate::popup::draw_completion_popup(f, c, f.area());
            }
            crate::popup::PopupState::SignatureHelp(s) => {
                crate::popup::draw_signature_help_popup(f, s, f.area());
            }
            crate::popup::PopupState::Hover(h) => {
                crate::popup::draw_hover_popup(f, h, f.area());
            }
            crate::popup::PopupState::Help(_) => {
                crate::popup::draw_help_popup(f, f.area());
            }
            crate::popup::PopupState::SymbolSearch(p) => {
                crate::symbol_search::draw_symbol_picker(f, p);
            }
        }
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
    if area.width == 0 || area.height == 0 {
        app.set_scrollback_area(area);
        return;
    }

    // Pass 1: cheap per-entry wrapped-row count, no allocations.
    // O(text length) for each, vs. the old approach which built
    // and wrapped every entry per draw — turning a single huge
    // entry into multi-MB of per-char allocation on every wheel
    // tick. With this pass, total work per draw is O(scrollback)
    // for counting + O(viewport) for actually wrapping the
    // visible window.
    //
    // Auto-hide scrollbar policy: count once at full width to
    // decide "does content fit?"; if it doesn't, narrow the
    // paragraph by one column to make room for the scrollbar and
    // recount. This costs a second pass ONLY on frames where the
    // scrollbar is visible — the common overflow-free case pays
    // just one pass.
    let counts_full: Vec<u32> = app
        .scrollback()
        .iter()
        .map(|e| crate::render::count_wrapped_rows(e, area.width))
        .collect();
    let total_full: u32 =
        counts_full.iter().fold(0u32, |a, b| a.saturating_add(*b));
    let needs_scrollbar = total_full > area.height as u32;

    // `paragraph_area` is where wrapped text actually renders.
    // When the scrollbar is on, it takes the rightmost column of
    // `area`, leaving `area.width - 1` for text. Also the
    // scrollback_area we hand back to App (for mouse coord
    // mapping) so a click on the scrollbar column doesn't get
    // interpreted as a text-selection click.
    let (paragraph_area, counts, total) = if needs_scrollbar && area.width >= 2
    {
        let narrower = Rect {
            width: area.width - 1,
            ..area
        };
        let cs: Vec<u32> = app
            .scrollback()
            .iter()
            .map(|e| crate::render::count_wrapped_rows(e, narrower.width))
            .collect();
        let t: u32 = cs.iter().fold(0u32, |a, b| a.saturating_add(*b));
        (narrower, cs, t)
    } else {
        (area, counts_full, total_full)
    };
    // Hand paragraph_area (not `area`) back to App: mouse-coord
    // translation now excludes the scrollbar column.
    app.set_scrollback_area(paragraph_area);

    let max_scroll = total.saturating_sub(paragraph_area.height as u32);
    let scroll_offset = if app.scrollback_follow() {
        max_scroll
    } else {
        u32::from(app.scrollback_scroll()).min(max_scroll)
    };
    app.set_last_rendered_scroll(scroll_offset.min(u32::from(u16::MAX)) as u16);
    app.set_last_max_scroll(max_scroll.min(u32::from(u16::MAX)) as u16);

    // Pass 2: walk entries; build wrapped lines only for those
    // intersecting the viewport. Entries entirely above viewport
    // are skipped (their row count contributes to the offset we
    // pass to ratatui's `Paragraph::scroll`). Entries entirely
    // below viewport stop the walk.
    let viewport_start = scroll_offset;
    let viewport_end =
        viewport_start.saturating_add(paragraph_area.height as u32);

    let mut visible: Vec<Line<'static>> =
        Vec::with_capacity(paragraph_area.height as usize + 16);
    let mut accumulated: u32 = 0;
    // `skipped_rows` counts wrapped rows preceding the first row we
    // actually emit into `visible`. For entries fully above the
    // viewport we add their whole `count`; for the FIRST partially-
    // visible entry we add whatever wrapped rows come between the
    // entry's start and the first source line the windowed slicer
    // emits. Combined, this keeps `local_scroll = viewport_start -
    // skipped_rows` correct even when we slice inside an entry.
    let mut skipped_rows: u32 = 0;
    {
        let scrollback = app.scrollback();
        for (entry, &count) in scrollback.iter().zip(counts.iter()) {
            let entry_end = accumulated.saturating_add(count);
            if entry_end <= viewport_start {
                // Entirely above viewport — count its rows toward
                // the local scroll offset and move on without
                // wrapping.
                skipped_rows = entry_end;
                accumulated = entry_end;
                continue;
            }
            if accumulated >= viewport_end {
                break;
            }
            // Windowed slice: emit only source lines whose wrapped-
            // row range overlaps the viewport. Local window is in
            // this entry's own row space (0-indexed), so subtract
            // `accumulated` first.
            let local_start = viewport_start.saturating_sub(accumulated);
            let local_end = viewport_end.saturating_sub(accumulated);
            let (lines, offset) = crate::render::entry_lines_windowed(
                entry,
                paragraph_area.width,
                local_start..local_end,
            );
            // Only the FIRST windowed entry contributes an intra-
            // entry offset; subsequent entries fall wholly within
            // the viewport and start rendering at row 0 locally.
            // For the first sliced entry, add the wrapped rows the
            // slicer skipped (source lines above the visible
            // window) to `skipped_rows` so downstream math stays
            // symmetric with the whole-entry-skipped case.
            if visible.is_empty() {
                skipped_rows =
                    skipped_rows.saturating_add(local_start - offset);
            }
            let wrapped =
                crate::render::wrap_lines(lines, paragraph_area.width);
            visible.extend(wrapped);
            accumulated = entry_end;
        }
    }

    // Selection highlight: coords are global wrapped-row indices.
    // Subtract `skipped_rows` so they index into the local
    // `visible` Vec instead.
    if let Some(sel) = app.selection() {
        let (start, end) = sel.ordered();
        let skipped = skipped_rows as usize;
        let local_start = (start.0.saturating_sub(skipped), start.1);
        let local_end = (end.0.saturating_sub(skipped), end.1);
        crate::render::apply_selection_highlight(
            &mut visible,
            local_start,
            local_end,
        );
    }

    // We've already skipped entries above viewport; ratatui only
    // needs to skip the remaining rows within the first visible
    // entry (i.e. the offset from where that entry started to
    // where the viewport actually begins).
    let local_scroll = viewport_start
        .saturating_sub(skipped_rows)
        .min(u32::from(u16::MAX)) as u16;

    // Blank the scrollback area first. ratatui's Paragraph doesn't
    // guarantee overwriting cells past its own content, so a frame
    // that emits shorter/fewer wrapped lines than the previous one
    // (very common under live streaming — a new INFO chunk shifts
    // the viewport and the tail cells of the prior frame stay
    // dirty) shows up as leftover fragments in the wrong color,
    // usually looking like `INFO:` / `.v:` / hex-digit tails
    // grafted onto the front of the current line. `Clear` writes
    // spaces with the default style over `area`, so subsequent
    // paragraph render lands on a clean slate. Clear covers the
    // full `area` (including any scrollbar column) — Scrollbar
    // will paint over that column below.
    f.render_widget(Clear, area);
    // No surrounding block: the scrollback's main job is to be
    // copy-pastable. A box-drawing border around each visible row
    // means any selection that spans full lines pulls in `│` chars
    // at the start and end of every line. The input box below the
    // scrollback still has its own border, which provides enough
    // visual separation between the two regions.
    let paragraph = Paragraph::new(visible).scroll((local_scroll, 0));
    f.render_widget(paragraph, paragraph_area);

    // Scrollbar overlay — only when content overflows the viewport.
    // ScrollbarState's `content_length` is the total scrollable
    // range (max_scroll + 1 so the thumb can reach the very
    // bottom); `position` is the current scroll offset. Rendered
    // on the rightmost column of `area`; the paragraph was drawn
    // in `paragraph_area` which excludes that column so there's
    // no overpaint on wrapped text.
    if needs_scrollbar && area.width >= 2 {
        let content_len = max_scroll.saturating_add(1) as usize;
        let mut sb_state = ScrollbarState::new(content_len)
            .position(scroll_offset as usize)
            .viewport_content_length(paragraph_area.height as usize);
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

fn draw_input(f: &mut Frame, area: Rect, app: &mut App) {
    // TextArea remains the editing model (every keystroke still flows
    // through `ta.input(key)` in `handle_terminal_event`'s catch-all
    // arm). We replace ONLY the visual rendering layer here so we can
    // paint per-token highlighter spans — tui-textarea's built-in
    // renderer is monochrome and its `line_spans` is `pub(crate)`, so
    // there's no hook for syntax styling without taking over the
    // viewport ourselves.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(input_title(app));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let ta = app.input_mut();
    let lines: Vec<String> = ta.lines().to_vec();
    let (cursor_row, cursor_col) = ta.cursor();

    // Run the htcl highlighter on the full buffer in one parse, then
    // slice per-line for rendering. Per-frame cost is one
    // `vw_htcl::parse` over the input text — fine for typical REPL
    // inputs (tens of lines); revisit if it shows up in a profile.
    let body_style =
        ratatui::style::Style::default().fg(ratatui::style::Color::White);
    let buffer = lines.join("\n");
    let highlighted =
        crate::highlight_htcl::highlight_per_line(&buffer, body_style);

    // Vertical viewport: anchor so the cursor stays visible. When the
    // buffer fits, anchor at top; when it overflows, scroll so cursor
    // is on the last visible row.
    let view_h = inner.height as usize;
    let scroll_top = if lines.len() <= view_h {
        0
    } else {
        cursor_row.saturating_sub(view_h.saturating_sub(1))
    };

    // Render each visible line as a single-row Paragraph.
    for (visible_idx, line_idx) in (scroll_top..lines.len()).enumerate() {
        if visible_idx >= view_h {
            break;
        }
        let row = inner.y + visible_idx as u16;
        let line_spans = highlighted.get(line_idx).cloned().unwrap_or_default();
        let line_widget = Paragraph::new(ratatui::text::Line::from(line_spans));
        let line_area = Rect {
            x: inner.x,
            y: row,
            width: inner.width,
            height: 1,
        };
        f.render_widget(line_widget, line_area);
    }

    // Position the terminal-native text cursor so the user sees their
    // edit point. Only set when the cursor row is visible — when
    // scrolled away (shouldn't happen given the viewport math above,
    // but defensive), we just leave the cursor hidden.
    if cursor_row >= scroll_top && cursor_row < scroll_top + view_h {
        let cursor_screen_row = inner.y + (cursor_row - scroll_top) as u16;
        // Horizontal: clamp to area width. Cursor positions past the
        // visible width get pinned to the last column rather than
        // drifting off the block border.
        let cursor_screen_col =
            inner.x + (cursor_col as u16).min(inner.width.saturating_sub(1));
        f.set_cursor_position((cursor_screen_col, cursor_screen_row));
    }
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
        // Single key-chord hint — the full cheat-sheet lives in
        // the Ctrl-H modal so we don't have to keep this row in
        // sync with every binding we add or change.
        "Ctrl-H for help".to_string()
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
