// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Diagnostics fuzzy-finder — Ctrl-F opens a centered modal listing
//! every diagnostic entry (Error / Warning / Notice) currently in the
//! scrollback. Typing filters via [`nucleo_matcher`]; ↑/↓ navigates;
//! Enter closes the modal and jumps the scrollback viewport to the
//! chosen entry, dropping a persistent left-gutter marker so the user
//! can spot it in a busy log. Alt-C clears the marker.
//!
//! Kind-filter checkboxes at the top let the user toggle inclusion of
//! each severity independently — Ctrl-E, Ctrl-W, Ctrl-N.  Defaults
//! are Error+Warning on, Notice off (chatty INFO messages usually
//! aren't what someone reaches for `find diagnostic` to see).
//!
//! Modeled on [`crate::symbol_search`] — same nucleo-matcher engine,
//! same modal shape (70% × 75% of frame).

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use crate::app::{ScrollbackEntry, ScrollbackKind};

/// One scrollback entry surfaced in the picker. `scrollback_idx` is
/// stable relative to the App's `scrollback` Vec at open time — the
/// picker holds a snapshot, and while it's open new appends land at
/// higher indices without perturbing existing ones (scrollback is
/// append-only), so accepting an item still jumps to the right row.
#[derive(Clone, Debug)]
pub struct DiagItem {
    pub scrollback_idx: usize,
    pub kind: ScrollbackKind,
    /// True when the source entry was a Vivado CRITICAL WARNING
    /// (kind is still `Error` — CW and plain Error share the same
    /// scrollback visual bucket). The picker uses this to answer
    /// the `Critical` filter checkbox; a critical entry also
    /// passes the `Error` filter, so turning on both is fine and
    /// won't duplicate rows.
    pub is_critical_warning: bool,
    /// First non-empty line of the entry — what shows in the result
    /// list. Truncated to a sensible width by the renderer.
    pub preview: String,
    /// Full text used as the fuzzy-match haystack. Multi-line
    /// diagnostics (WARNING body + attached stack) match on any
    /// content, so a query for a function name in the trace still
    /// finds the top-level warning.
    pub full: String,
    /// Scrollback index of the [`ScrollbackKind::Input`] entry
    /// this diagnostic was emitted under, or `None` when the
    /// diagnostic predates any input (startup notices, e.g.
    /// `vivado ready`). Used by the picker to group results by
    /// the command that produced them.
    pub parent_input_idx: Option<usize>,
    /// Single-line preview of the parent input's text —
    /// captured at snapshot time so the picker's header rows
    /// don't need to walk back into the App's scrollback. `None`
    /// when `parent_input_idx` is `None`.
    pub parent_preview: Option<String>,
}

/// Scored entry after applying the current query + kind filters.
/// Sorted by descending `score` at the end of `recompute`.
#[derive(Clone, Copy, Debug)]
pub struct Scored {
    pub item_idx: usize,
    pub score: u32,
}

/// Diagnostic-picker overlay state.
#[derive(Debug)]
pub struct DiagnosticPicker {
    /// Snapshot of Error/Warning/Notice entries from the scrollback,
    /// taken at open time. Fixed for the picker's lifetime — new
    /// scrollback appends aren't reflected until the user reopens.
    pub items: Vec<DiagItem>,
    pub query: String,
    pub results: Vec<Scored>,
    pub selected: usize,
    /// Which kinds pass the filter row's checkboxes.
    pub filter_error: bool,
    pub filter_warning: bool,
    pub filter_notice: bool,
    /// Critical-warning subset filter. CW entries carry
    /// `kind == Error` (shared visual bucket) — this checkbox
    /// lets the user surface JUST the criticals within that
    /// bucket. An item passes when either its kind matches an
    /// enabled kind filter OR its CW flag matches `filter_critical`.
    pub filter_critical: bool,
}

impl DiagnosticPicker {
    /// Build the picker from an App scrollback slice. Filters the
    /// slice down to diagnostic-kind entries; entries with empty
    /// text are dropped (nothing to preview or match against).
    pub fn from_scrollback(scrollback: &[ScrollbackEntry]) -> Self {
        let items: Vec<DiagItem> = scrollback
            .iter()
            .enumerate()
            .filter(|(_, e)| is_diagnostic(e.kind))
            .filter_map(|(idx, e)| {
                let preview = e.text.lines().next().unwrap_or("").to_string();
                if preview.is_empty() && e.text.is_empty() {
                    return None;
                }
                let parent_preview = e.parent_input_idx.and_then(|pidx| {
                    scrollback.get(pidx).map(|p| {
                        p.text.lines().next().unwrap_or("").to_string()
                    })
                });
                Some(DiagItem {
                    scrollback_idx: idx,
                    kind: e.kind,
                    is_critical_warning: e.is_critical_warning,
                    preview,
                    full: e.text.clone(),
                    parent_input_idx: e.parent_input_idx,
                    parent_preview,
                })
            })
            .collect();
        let mut p = Self {
            items,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            // Defaults: Error+Warning+Critical on, Notice off.
            // Notice buckets INFO-severity Vivado messages, which
            // are chatty and usually not what someone reaches for
            // a diagnostics finder to see — but it's one keystroke
            // (Ctrl-N) away. Critical is on by default because
            // it's a subset of Error, so having it enabled changes
            // nothing until the user turns Error off to isolate
            // criticals.
            filter_error: true,
            filter_warning: true,
            filter_notice: false,
            filter_critical: true,
        };
        p.recompute();
        p
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.recompute();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.recompute();
    }

    /// Toggle inclusion of a specific kind. Ignores kinds outside
    /// the picker's diagnostic set — passing `Chatter` is a no-op.
    pub fn toggle_kind(&mut self, kind: ScrollbackKind) {
        match kind {
            ScrollbackKind::Error => self.filter_error = !self.filter_error,
            ScrollbackKind::Warning => {
                self.filter_warning = !self.filter_warning;
            }
            ScrollbackKind::Notice => self.filter_notice = !self.filter_notice,
            _ => return,
        }
        self.recompute();
    }

    /// Toggle the Critical-warning subset filter. Independent of
    /// the kind filters — with `filter_critical` on and every
    /// kind off, the picker shows just critical warnings.
    pub fn toggle_critical(&mut self) {
        self.filter_critical = !self.filter_critical;
        self.recompute();
    }

    /// Currently-selected item, if any. Returns the item struct
    /// (contains `scrollback_idx` and `kind`) rather than just the
    /// index so callers can pass kind into the marker-styling code
    /// without a second lookup.
    pub fn current(&self) -> Option<&DiagItem> {
        let scored = self.results.get(self.selected)?;
        self.items.get(scored.item_idx)
    }

    /// Whether `item` passes the current filter row. An entry
    /// passes when EITHER its kind's checkbox is on OR (for
    /// critical warnings) the Critical checkbox is on — so a CW
    /// entry surfaces under Error, under Critical, or both,
    /// without appearing twice.
    fn item_allowed(&self, item: &DiagItem) -> bool {
        let kind_pass = match item.kind {
            ScrollbackKind::Error => self.filter_error,
            ScrollbackKind::Warning => self.filter_warning,
            ScrollbackKind::Notice => self.filter_notice,
            _ => false,
        };
        let critical_pass = item.is_critical_warning && self.filter_critical;
        kind_pass || critical_pass
    }

    /// Rebuild `results` from current query + filter state. O(N)
    /// over the snapshot. Called from `new`, `push_char`,
    /// `pop_char`, `toggle_kind`, `toggle_critical`.
    fn recompute(&mut self) {
        let allowed: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| self.item_allowed(it))
            .map(|(i, _)| i)
            .collect();

        if self.query.is_empty() {
            // No query: show all kind-allowed items in scrollback
            // order (== descending scrollback_idx would be reverse-
            // chronological; ascending == chronological, which is
            // what appears in the log itself — pick the latter so
            // the picker order matches what the user's eye scanned).
            self.results = allowed
                .into_iter()
                .map(|item_idx| Scored { item_idx, score: 0 })
                .collect();
            self.clamp_selected();
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
        );
        let mut scored: Vec<Scored> = allowed
            .iter()
            .filter_map(|&item_idx| {
                let it = &self.items[item_idx];
                let hay = Utf32String::from(it.full.as_str());
                let score = pattern.score(hay.slice(..), &mut matcher)?;
                if score == 0 {
                    None
                } else {
                    Some(Scored { item_idx, score })
                }
            })
            .collect();
        scored.sort_by_key(|s| std::cmp::Reverse(s.score));
        scored.truncate(500);
        self.results = scored;
        self.clamp_selected();
    }

    fn clamp_selected(&mut self) {
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }
}

/// True when `kind` is one of the diagnostic kinds the picker
/// surfaces. Broken out so `from_scrollback` and `kind_allowed`
/// stay in sync — adding a new diagnostic kind means updating
/// this predicate alone.
fn is_diagnostic(kind: ScrollbackKind) -> bool {
    matches!(
        kind,
        ScrollbackKind::Error
            | ScrollbackKind::Warning
            | ScrollbackKind::Notice
    )
}

/// Render the diagnostic picker as a centered modal. Sized to
/// match [`crate::symbol_search::draw_symbol_picker`] so the two
/// finders feel like siblings.
pub fn draw_diagnostic_picker(f: &mut Frame, picker: &DiagnosticPicker) {
    let frame = f.area();
    let width = (frame.width as f32 * 0.7) as u16;
    let height = (frame.height as f32 * 0.75) as u16;
    let x = frame.x + (frame.width.saturating_sub(width)) / 2;
    let y = frame.y + (frame.height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width: width.min(frame.width),
        height: height.min(frame.height),
    };

    f.render_widget(Clear, area);
    // Title shows a live match / snapshot-size count so the user
    // knows how selective their filters + query are. `snapshot`
    // is the total diagnostics captured when the picker opened;
    // `matches` is the count after filter+query. When the two
    // are equal the count reads as e.g. `(12/12)` = "everything
    // in scrollback passes".
    let title = format!(
        " find diagnostic ({}/{}) — Esc to close ",
        picker.results.len(),
        picker.items.len(),
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    // Vertical layout: filter row (1) + query row (1) + separator
    // (1) + result list (remaining).
    let filter_row = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    let query_row = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: 1,
    };
    let sep_row = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: 1,
    };
    let list_area = Rect {
        x: inner.x,
        y: inner.y + 3,
        width: inner.width,
        height: inner.height.saturating_sub(3),
    };

    draw_filter_row(f, picker, filter_row);
    draw_query_row(f, picker, query_row);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().add_modifier(Modifier::DIM),
        ))),
        sep_row,
    );
    draw_result_list(f, picker, list_area);
}

fn draw_filter_row(f: &mut Frame, picker: &DiagnosticPicker, area: Rect) {
    let cell = |on: bool, glyph: &str, label: &str, key: &str, color: Color| {
        let box_style = if on {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        vec![
            Span::styled(glyph.to_string(), box_style),
            Span::raw(" "),
            Span::styled(label.to_string(), box_style),
            Span::raw(" "),
            Span::styled(
                format!("({key})"),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw("   "),
        ]
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        " filter: ".to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    spans.extend(cell(
        picker.filter_error,
        if picker.filter_error { "[x]" } else { "[ ]" },
        "Error",
        "^E",
        Color::Red,
    ));
    spans.extend(cell(
        picker.filter_critical,
        if picker.filter_critical { "[x]" } else { "[ ]" },
        "Critical",
        "^K",
        Color::Rgb(255, 90, 90),
    ));
    spans.extend(cell(
        picker.filter_warning,
        if picker.filter_warning { "[x]" } else { "[ ]" },
        "Warning",
        "^W",
        Color::Rgb(255, 140, 0),
    ));
    spans.extend(cell(
        picker.filter_notice,
        if picker.filter_notice { "[x]" } else { "[ ]" },
        "Info",
        "^N",
        Color::Gray,
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_query_row(f: &mut Frame, picker: &DiagnosticPicker, area: Rect) {
    let spans = vec![
        Span::styled(
            " › ".to_string(),
            Style::default()
                .fg(Color::Rgb(180, 130, 220))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(picker.query.clone(), Style::default().fg(Color::White)),
        // Cursor position — a solid block on the end of the query
        // makes the input row read as an editable field even
        // without a real terminal cursor there (ratatui's cursor
        // is bound to the input editor below the popup).
        Span::styled(
            "▏".to_string(),
            Style::default().fg(Color::Rgb(180, 130, 220)),
        ),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_result_list(f: &mut Frame, picker: &DiagnosticPicker, area: Rect) {
    if picker.results.is_empty() {
        let msg = if picker.items.is_empty() {
            "no diagnostics in scrollback"
        } else if picker.query.is_empty() {
            "no diagnostics match current filters"
        } else {
            "no diagnostics match query"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().add_modifier(Modifier::DIM),
            ))),
            area,
        );
        return;
    }

    // Reserve the rightmost column for a scrollbar when the
    // result list overflows the visible area. Matches the main
    // scrollback pane's auto-hide behavior — no wasted column
    // when everything fits.
    let needs_scrollbar = picker.results.len() > area.height as usize;
    let (list_area, scrollbar_area) = if needs_scrollbar && area.width >= 2 {
        let narrower = Rect {
            width: area.width - 1,
            ..area
        };
        (narrower, Some(area))
    } else {
        (area, None)
    };

    let max_preview_width = (list_area.width as usize)
        .saturating_sub(6 /* kind badge + space */);

    // Results grouped by parent input. Walk results in order,
    // emit a header row whenever the parent_input_idx changes,
    // then emit the item row itself. Track selected_visual_row
    // so ratatui's list highlight lands on the actual item (not
    // a header) even though `picker.selected` indexes into
    // `picker.results`.
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_visual_row: usize = 0;
    let mut prev_parent: Option<Option<usize>> = None;
    for (row_idx, s) in picker.results.iter().enumerate() {
        let Some(it) = picker.items.get(s.item_idx) else {
            continue;
        };
        // Group header — emitted when the parent changes vs the
        // previous item. `Some(None)` (rendered as "before any
        // command") differs from an actual command's group, so
        // we key on the Option<Option<usize>> pair.
        if prev_parent != Some(it.parent_input_idx) {
            let header_text = match (&it.parent_preview, it.parent_input_idx) {
                (Some(preview), _) => preview.clone(),
                (None, _) => "(before any command)".to_string(),
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    "▼ ".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    header_text,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ])));
            prev_parent = Some(it.parent_input_idx);
        }
        // Item row. Indented 2 cells to visually nest under the
        // group header.
        let (badge_str, badge_style) = match it.kind {
            ScrollbackKind::Error if it.is_critical_warning => (
                "C ",
                Style::default()
                    .fg(Color::Rgb(255, 90, 90))
                    .add_modifier(Modifier::BOLD),
            ),
            ScrollbackKind::Error => (
                "E ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            ScrollbackKind::Warning => (
                "W ",
                Style::default()
                    .fg(Color::Rgb(255, 140, 0))
                    .add_modifier(Modifier::BOLD),
            ),
            ScrollbackKind::Notice => (
                "i ",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ),
            _ => ("? ", Style::default()),
        };
        // 2-cell indent for nesting under the group header;
        // room in the preview budget shrinks accordingly.
        let preview_budget = max_preview_width.saturating_sub(2);
        let preview = truncate_chars(&it.preview, preview_budget);
        let is_selected = row_idx == picker.selected;
        let preview_style = if is_selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let row_style = if is_selected {
            Style::default().bg(Color::Rgb(40, 40, 60))
        } else {
            Style::default()
        };
        if is_selected {
            selected_visual_row = items.len();
        }
        let spans = vec![
            Span::raw("  "),
            Span::styled(badge_str.to_string(), badge_style),
            Span::styled(preview, preview_style),
        ];
        items.push(ListItem::new(Line::from(spans)).style(row_style));
    }

    let mut list_state = ListState::default();
    list_state.select(Some(selected_visual_row));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)));
    f.render_stateful_widget(list, list_area, &mut list_state);

    // Scrollbar reflects the selection's position within the
    // full result set (not just the visible viewport) — that's
    // what the user cares about when they've paged Down deep
    // into hits.
    if let Some(sb_area) = scrollbar_area {
        let mut sb_state = ScrollbarState::new(picker.results.len())
            .position(picker.selected)
            .viewport_content_length(list_area.height as usize);
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
    }
}

/// Truncate `s` to `max_chars` display cells, appending `…` when
/// clipped. Character-count based (not byte-based) so multi-byte
/// content — non-ASCII proc names, stack-frame arrows — clip
/// cleanly.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars < 2 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let take = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ScrollbackKind, text: &str) -> ScrollbackEntry {
        ScrollbackEntry {
            kind,
            text: text.to_string(),
            started_at: None,
            completed_at: None,
            collapse_state: None,
            is_critical_warning: false,
            parent_input_idx: None,
            group_collapsed: false,
            error_child_count: 0,
            warning_child_count: 0,
        }
    }

    fn critical_entry(text: &str) -> ScrollbackEntry {
        ScrollbackEntry {
            kind: ScrollbackKind::Error,
            text: text.to_string(),
            started_at: None,
            completed_at: None,
            collapse_state: None,
            is_critical_warning: true,
            parent_input_idx: None,
            group_collapsed: false,
            error_child_count: 0,
            warning_child_count: 0,
        }
    }

    #[test]
    fn snapshot_filters_to_diagnostics() {
        let sb = vec![
            entry(ScrollbackKind::Input, "some input"),
            entry(ScrollbackKind::Error, "unresolved symbol foo"),
            entry(ScrollbackKind::Stdout, "hello"),
            entry(ScrollbackKind::Warning, "deprecated call"),
            entry(ScrollbackKind::Chatter, "banner"),
            entry(ScrollbackKind::Notice, "vivado: ready"),
        ];
        let picker = DiagnosticPicker::from_scrollback(&sb);
        assert_eq!(picker.items.len(), 3);
        // scrollback indices preserved
        assert_eq!(picker.items[0].scrollback_idx, 1);
        assert_eq!(picker.items[1].scrollback_idx, 3);
        assert_eq!(picker.items[2].scrollback_idx, 5);
    }

    #[test]
    fn default_filter_hides_notice() {
        let sb = vec![
            entry(ScrollbackKind::Error, "e1"),
            entry(ScrollbackKind::Warning, "w1"),
            entry(ScrollbackKind::Notice, "n1"),
        ];
        let picker = DiagnosticPicker::from_scrollback(&sb);
        // 3 items in the snapshot but only 2 pass default filters.
        assert_eq!(picker.items.len(), 3);
        assert_eq!(picker.results.len(), 2);
    }

    #[test]
    fn toggle_notice_shows_it() {
        let sb = vec![
            entry(ScrollbackKind::Error, "e1"),
            entry(ScrollbackKind::Notice, "n1"),
        ];
        let mut picker = DiagnosticPicker::from_scrollback(&sb);
        assert_eq!(picker.results.len(), 1);
        picker.toggle_kind(ScrollbackKind::Notice);
        assert_eq!(picker.results.len(), 2);
    }

    #[test]
    fn critical_shows_under_error_and_critical() {
        let sb = vec![
            entry(ScrollbackKind::Error, "plain error"),
            critical_entry("critical warning body"),
        ];
        let mut picker = DiagnosticPicker::from_scrollback(&sb);
        // Both Error+Critical filters on by default → both entries.
        assert_eq!(picker.results.len(), 2);
        // Error off, Critical on → just the CW (it still passes
        // via the critical predicate even though its kind is
        // Error).
        picker.toggle_kind(ScrollbackKind::Error);
        assert_eq!(picker.results.len(), 1);
        assert!(picker.current().unwrap().is_critical_warning);
        // Critical off too → nothing (both filter paths off).
        picker.toggle_critical();
        assert_eq!(picker.results.len(), 0);
        // Error back on, Critical still off → BOTH entries show:
        // CW passes via its kind (Error) even without the
        // critical checkbox. This is the "if they show up when
        // error is selected that's fine" behavior — the Critical
        // filter is additive, not exclusive.
        picker.toggle_kind(ScrollbackKind::Error);
        assert_eq!(picker.results.len(), 2);
    }

    #[test]
    fn fuzzy_query_narrows() {
        let sb = vec![
            entry(ScrollbackKind::Error, "unresolved symbol foo"),
            entry(ScrollbackKind::Error, "cannot open file bar"),
            entry(ScrollbackKind::Warning, "deprecated call foo"),
        ];
        let mut picker = DiagnosticPicker::from_scrollback(&sb);
        assert_eq!(picker.results.len(), 3);
        picker.push_char('f');
        picker.push_char('o');
        picker.push_char('o');
        // Both `foo`-mentioning entries survive; the `bar` one doesn't.
        assert_eq!(picker.results.len(), 2);
        for r in &picker.results {
            assert!(picker.items[r.item_idx].full.contains("foo"));
        }
    }

    #[test]
    fn move_updown_clamps() {
        let sb = vec![
            entry(ScrollbackKind::Error, "a"),
            entry(ScrollbackKind::Error, "b"),
        ];
        let mut picker = DiagnosticPicker::from_scrollback(&sb);
        assert_eq!(picker.selected, 0);
        picker.move_up();
        assert_eq!(picker.selected, 0);
        picker.move_down();
        assert_eq!(picker.selected, 1);
        picker.move_down();
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn current_maps_to_scrollback_idx() {
        let sb = vec![
            entry(ScrollbackKind::Input, "ignored"),
            entry(ScrollbackKind::Warning, "hit"),
        ];
        let picker = DiagnosticPicker::from_scrollback(&sb);
        let current = picker.current().unwrap();
        assert_eq!(current.scrollback_idx, 1);
        assert_eq!(current.kind, ScrollbackKind::Warning);
    }

    #[test]
    fn empty_scrollback_no_panic() {
        let picker = DiagnosticPicker::from_scrollback(&[]);
        assert_eq!(picker.results.len(), 0);
        assert!(picker.current().is_none());
    }
}
