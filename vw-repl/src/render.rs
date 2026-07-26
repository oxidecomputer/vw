// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Scrollback rendering helpers shared between `ui::draw_scrollback`
//! and `App` mouse-selection. Both need the same view of "how does the
//! scrollback look on screen, row by row" — the UI to render it, the
//! App to map mouse clicks to text positions and extract the selected
//! substring on copy.
//!
//! The flow is: [`entry_lines`] turns each `ScrollbackEntry` into one
//! styled [`Line`] per source line; [`wrap_lines`] then breaks each of
//! those at the rendered column width into screen-row–sized chunks.
//! After wrapping, screen-row N is `wrapped[scroll + N]` — that 1:1
//! mapping is what makes mouse-cell → text-cell trivial. With
//! ratatui's built-in `Wrap { trim: false }` we'd have to replay
//! ratatui's word-wrap to find the same mapping, which we don't want
//! to maintain in lockstep.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{ScrollbackEntry, ScrollbackKind};

/// One styled [`Line`] per source line in `entry`. The leading
/// 2-cell column is the kind-prefix (`› `, `· `, `⚠ `, etc.) on the
/// first source line and two spaces on continuation lines, so a
/// multi-line entry visually hangs together.
///
/// `area_width` is the terminal column count — used to
/// right-justify the per-input timer marker on the first line of
/// an `Input` entry. Pass the same width the renderer will wrap
/// to so the timer ends up flush at the right margin.
/// Backwards-compatible full-entry render. Same output as
/// [`entry_lines_windowed`] called with `0..u32::MAX`. Used by tests
/// and by the clipboard-copy path (which needs every source line
/// regardless of what's on screen).
pub fn entry_lines(
    entry: &ScrollbackEntry,
    area_width: u16,
) -> Vec<Line<'static>> {
    entry_lines_windowed(entry, area_width, 0..u32::MAX).0
}

/// Produce styled [`Line`] objects for only the source lines whose
/// **wrapped** rows overlap `window` (0-indexed within this entry's
/// wrapped row range).
///
/// Returns `(lines, offset_into_first)` where `offset_into_first` is
/// how many wrapped rows of the first emitted line's own wrapped
/// output precede `window.start`. Callers pass this back through the
/// ratatui `Paragraph::scroll` residual so the on-screen scroll
/// alignment stays byte-for-byte identical to what the full-entry
/// render would have produced.
///
/// This is the primary path — a huge scrollback entry (200K lines of
/// a `list<bd_pin>` repr, say) that intersects the viewport was
/// previously O(entry.text.lines().count()) work per redraw because
/// `entry_lines` walked every source line, called `highlight_line` on
/// each, allocated a `Line` per line, and then handed the whole Vec
/// to `wrap_lines` which processed it end-to-end. Only the visible
/// window (typically ≤ area.height rows) ever mattered. This function
/// still walks the source-line iterator to *count* rows for skipped
/// lines (that's cheap — just `chars().count()` + arithmetic per
/// line), but does no allocation or highlighting until we hit the
/// window. Same output visually; O(entry) → O(visible + prefix_scan).
pub fn entry_lines_windowed(
    entry: &ScrollbackEntry,
    area_width: u16,
    window: std::ops::Range<u32>,
) -> (Vec<Line<'static>>, u32) {
    // Collapsed entries: exactly one placeholder row regardless of
    // window — the placeholder itself is the whole render.
    if entry.collapse_state == Some(true) {
        return (collapsible_lines(entry, true), 0);
    }
    // Expanded (Some(false)) and non-collapsible (None) both walk
    // the windowed source-line path with the entry-kind's normal
    // prefix/style. Expanded collapsible entries additionally get
    // a `▼` marker in the leftmost 2-cell column so the toggleable
    // affordance is visible — users can see this cell is collapsible
    // without having to try Shift+click on every entry. None entries
    // (single-line, no meaningful collapse) skip the marker column.
    let orange = Color::Rgb(255, 140, 0);
    let (kind_prefix_str, prefix_style) = kind_prefix(entry.kind, orange);
    let marker_style = Style::default().fg(Color::Gray);
    // Marker column shows the group / block collapse affordance:
    //
    //   * Input entries always get a marker (▶ collapsed / ▼
    //     expanded) — every command groups its output, so the
    //     affordance is always available.
    //   * Non-Input entries only get the marker when they're
    //     themselves an expanded collapsible block
    //     (collapse_state == Some(false)) — the intra-entry
    //     "this multi-line body can be collapsed" case.
    let has_marker = matches!(entry.kind, ScrollbackKind::Input)
        || entry.collapse_state == Some(false);
    // Which glyph goes in the marker column. For Input rows we
    // key off `group_collapsed`; for expanded non-Input blocks
    // it's always `▼` (`▶` collapsed non-Input blocks are
    // handled entirely by `collapsible_lines` above and never
    // reach this branch).
    let marker_glyph = if matches!(entry.kind, ScrollbackKind::Input) {
        if entry.group_collapsed {
            "▶ "
        } else {
            "▼ "
        }
    } else {
        "▼ "
    };
    // Continuation rows get an indent matching row 0's total prefix
    // width so text stays visually aligned across a wrapped entry.
    let cont_indent = if has_marker { "    " } else { "  " };
    let body_style = match entry.kind {
        ScrollbackKind::Input => Style::default().fg(Color::White),
        ScrollbackKind::Result => Style::default().fg(Color::Gray),
        ScrollbackKind::Stdout => Style::default().fg(Color::White),
        ScrollbackKind::Error => Style::default().fg(Color::Red),
        ScrollbackKind::Warning => Style::default().fg(orange),
        ScrollbackKind::Notice => Style::default().fg(Color::Gray),
        ScrollbackKind::Chatter => {
            Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
        }
    };
    // For Input entries with a timer, render `<elapsed>` flush
    // right on the first line. Color follows whether it's still
    // running (dim while live) vs. completed (subtle gray).
    let timer = timer_for(entry);
    // Highlighting strategy per kind:
    //   - Result / Stdout: repr highlighter (per-line shape recognition
    //     for compiler-emitted enum reprs).
    //   - Input: htcl-source highlighter (whole-entry parse, per-line
    //     span slicing) — colors keywords, calls, $vars, types,
    //     comments etc. the same as the input editor.
    //   - Error / Warning / Notice: single body color (not repr-formatted).
    let repr_highlight =
        matches!(entry.kind, ScrollbackKind::Result | ScrollbackKind::Stdout);
    let input_highlight = matches!(entry.kind, ScrollbackKind::Input);
    // Input entries: parse the whole entry text once and slice per-line.
    // The body_style on Input is the bright cyan we'd otherwise apply
    // flatly; the htcl highlighter overrides it for recognized tokens
    // and leaves it for the gaps.
    let input_per_line: Option<Vec<Vec<Span<'static>>>> = if input_highlight {
        Some(crate::highlight_htcl::highlight_per_line(
            &entry.text,
            body_style,
        ))
    } else {
        None
    };
    let mut out = Vec::new();
    // Per-source-line wrapped-row accounting — matches the formula
    // in `count_wrapped_rows` exactly. Any drift here would misalign
    // the caller's global row math with the actual rendered rows.
    let w = area_width.max(1) as usize;
    let prefix_width = 2;
    let mut cumulative: u32 = 0;
    let mut first_emitted_row_start: Option<u32> = None;
    let mut had_lines = false;
    for (i, line) in entry.text.lines().enumerate() {
        had_lines = true;
        let body_chars = line.chars().count();
        let total = body_chars.saturating_add(prefix_width).max(1);
        let line_rows = (total.div_ceil(w).max(1)) as u32;
        let line_end = cumulative.saturating_add(line_rows);
        // Skip source lines whose wrapped-row range ends before the
        // window even starts. Cheap — just chars().count() + arith,
        // no allocations, no highlight calls.
        if line_end <= window.start {
            cumulative = line_end;
            continue;
        }
        // Stop once we've moved past the window's end. `cumulative`
        // here is the FIRST row this line contributes; if that's
        // already past the window, everything remaining is offscreen.
        if cumulative >= window.end {
            break;
        }
        if first_emitted_row_start.is_none() {
            first_emitted_row_start = Some(cumulative);
        }
        // Row-0 gutter for a collapsible expanded entry: `▼ ` marker
        // in dim gray, then the entry-kind's normal prefix. Row 0+ of
        // the same entry uses `cont_indent` (4 spaces to reserve room
        // for both the marker column and the kind prefix) so wrapped
        // text hangs cleanly under row 0's body.
        let mut spans: Vec<Span<'static>> = Vec::new();
        if i == 0 {
            if has_marker {
                spans
                    .push(Span::styled(marker_glyph.to_string(), marker_style));
            }
            spans.push(Span::styled(kind_prefix_str.to_string(), prefix_style));
        } else {
            spans.push(Span::styled(cont_indent.to_string(), prefix_style));
        }
        if let Some(per_line) = input_per_line.as_ref() {
            if let Some(line_spans) = per_line.get(i) {
                spans.extend(line_spans.iter().cloned());
            } else {
                spans.push(Span::styled(line.to_string(), body_style));
            }
        } else if repr_highlight {
            if let Some(highlighted) = crate::highlight::highlight_line(line) {
                spans.extend(highlighted);
            } else {
                spans.push(Span::styled(line.to_string(), body_style));
            }
        } else {
            spans.push(Span::styled(line.to_string(), body_style));
        }
        if i == 0 {
            // Severity badges on Input rows: `✗` (red bold) when
            // the group's children include any Error / CW, and
            // `⚠` (orange bold, same glyph the Warning gutter
            // uses) when they include any plain Warning. Both
            // can render together; each shows a count when >1.
            // Placement is between the input text and the timer
            // so the far-right timer position stays stable.
            let mut badges: Vec<(String, Style)> = Vec::new();
            if matches!(entry.kind, ScrollbackKind::Input) {
                if entry.error_child_count > 0 {
                    let text = if entry.error_child_count > 1 {
                        format!("✗ {}", entry.error_child_count)
                    } else {
                        "✗".to_string()
                    };
                    badges.push((
                        text,
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if entry.warning_child_count > 0 {
                    let text = if entry.warning_child_count > 1 {
                        format!("⚠ {}", entry.warning_child_count)
                    } else {
                        "⚠".to_string()
                    };
                    badges.push((
                        text,
                        Style::default()
                            .fg(orange)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
            let timer_ref = timer.as_ref();
            if timer_ref.is_some() || !badges.is_empty() {
                let used: usize =
                    spans.iter().map(|s| display_cells(&s.content)).sum();
                // Each badge contributes its glyph width + a
                // trailing space to separate it from the next
                // element.
                let badges_w: usize =
                    badges.iter().map(|(t, _)| display_cells(t) + 1).sum();
                let timer_w =
                    timer_ref.map(|(l, _)| display_cells(l)).unwrap_or(0);
                let total_right = badges_w + timer_w;
                if (used + total_right + 1) as u16 <= area_width {
                    let pad = area_width as usize - used - total_right;
                    spans.push(Span::raw(" ".repeat(pad)));
                    for (btext, bstyle) in badges {
                        spans.push(Span::styled(btext, bstyle));
                        spans.push(Span::raw(" "));
                    }
                    if let Some((label, label_style)) = timer_ref {
                        spans.push(Span::styled(label.clone(), *label_style));
                    }
                }
            }
        }
        out.push(Line::from(spans));
        cumulative = line_end;
    }
    if !had_lines && out.is_empty() {
        // Empty text: match the pre-windowed behavior (one blank
        // prefix-only line). Only render it if window includes row 0.
        if window.start == 0 && window.end > 0 {
            let mut blank_spans: Vec<Span<'static>> = Vec::new();
            if has_marker {
                blank_spans
                    .push(Span::styled(marker_glyph.to_string(), marker_style));
            }
            blank_spans
                .push(Span::styled(kind_prefix_str.to_string(), prefix_style));
            out.push(Line::from(blank_spans));
            first_emitted_row_start = Some(0);
        }
    }
    let offset = first_emitted_row_start
        .map(|s| window.start.saturating_sub(s))
        .unwrap_or(0);
    (out, offset)
}

/// Prefix + style for an entry-kind's row-0 gutter. Extracted so
/// the collapsed-placeholder renderer can use the same colors as
/// the expanded path — a collapsed `Result` reads with the same
/// gray body as an expanded `Result`, a collapsed `Warning` reads
/// with the same orange bold, etc. Only the entry's Chatter kind
/// specifically stays dim (that's the "background noise" bucket).
fn kind_prefix(kind: ScrollbackKind, orange: Color) -> (&'static str, Style) {
    match kind {
        ScrollbackKind::Input => (
            "› ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        ScrollbackKind::Result => ("  ", Style::default().fg(Color::Gray)),
        ScrollbackKind::Stdout => ("  ", Style::default().fg(Color::White)),
        ScrollbackKind::Error => (
            "✗ ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        ScrollbackKind::Warning => (
            "⚠ ",
            Style::default().fg(orange).add_modifier(Modifier::BOLD),
        ),
        ScrollbackKind::Notice => ("· ", Style::default().fg(Color::Gray)),
        // Chatter: 2-space prefix (no diagnostic glyph) + DIM
        // Gray — background-noise bucket for classifier-produced
        // NONE blocks. Multi-line Chatter still auto-collapses per
        // COLLAPSE_AUTO_THRESHOLD; the dim style just signals "this
        // is elidable" even before the user thinks about collapsing.
        ScrollbackKind::Chatter => (
            "  ",
            Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
        ),
    }
}

/// Render the single-row placeholder for a **collapsed** entry:
/// dimmed preview of the first non-empty content line plus a
/// `(N lines hidden)` tail. Uses a uniform dim dark-gray body
/// style regardless of entry kind — the whole point of collapse
/// is "this is elided content; expand to read." Rendering it in
/// the kind's normal color (bright red for Error, orange for
/// Warning, etc.) makes the placeholder compete visually with
/// entries that aren't collapsed, defeating the "elided noise"
/// signal. When the user Shift-clicks to expand, the entry
/// reverts to its normal kind coloring — that's when you actually
/// want the Warning to look like a Warning.
fn collapsible_lines(
    entry: &ScrollbackEntry,
    collapsed: bool,
) -> Vec<Line<'static>> {
    debug_assert!(
        collapsed,
        "expanded entries go through entry_lines_windowed's \
         regular source-line path — collapsible_lines is the \
         collapsed-only placeholder helper"
    );
    let marker_style = Style::default().fg(Color::Gray);
    let dim = Style::default().fg(Color::Gray).add_modifier(Modifier::DIM);
    let source_lines: Vec<&str> = entry.text.lines().collect();
    // Preview: first non-empty line (else first line, else "").
    let preview = source_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .copied()
        .unwrap_or_default();
    let count = source_lines.len();
    let suffix = if count > 1 {
        format!("  ({count} lines hidden)")
    } else {
        String::new()
    };
    let mut spans = vec![Span::styled("▶ ".to_string(), marker_style)];
    spans.push(Span::styled(preview.to_string(), dim));
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, dim));
    }
    vec![Line::from(spans)]
}

/// `(label, style)` for an entry's elapsed-time marker, or `None`
/// when the entry isn't timed. Color hints whether the timer is
/// still live (running) or frozen (completed).
fn timer_for(entry: &ScrollbackEntry) -> Option<(String, Style)> {
    let start = entry.started_at?;
    let end = entry.completed_at.unwrap_or_else(std::time::Instant::now);
    let elapsed = end.saturating_duration_since(start);
    let label = format_duration(elapsed);
    let style = if entry.completed_at.is_some() {
        // Frozen at final value — quiet, post-fact.
        Style::default().fg(Color::Gray)
    } else {
        // Live — slightly more present so the user sees it's
        // still moving.
        Style::default().fg(Color::Yellow)
    };
    Some((label, style))
}

/// Format a duration as `Ns`, `M:SS`, or `H:MM:SS` depending on
/// magnitude. Always second-granularity; never fractional. Matches
/// what users expect for "how long did this take" markers.
pub fn format_duration(d: std::time::Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}:{:02}", total / 60, total % 60)
    } else {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

/// Crude width estimator — counts chars, treating each as one
/// terminal cell. Good enough for our prefix glyphs (`› ` etc.,
/// each rendered as one cell in monospace terminals) and ASCII
/// timer labels. A full unicode-width crate would be more
/// correct but isn't worth the dep for the small set of
/// characters this code emits.
fn display_cells(s: &str) -> usize {
    s.chars().count()
}

/// Split each input line into screen-row-sized chunks of `width`
/// columns, preserving span styles across the split. The output
/// renders 1:1 against screen rows when fed to a `Paragraph` with no
/// further wrapping, so screen-row N is `out[scroll + N]`.
///
/// Splitting is character-based (no word-boundary respect) — this is
/// REPL output, not prose; long Vivado property dicts and Tcl errors
/// don't have natural break points.
/// Cheap pre-computation of how many wrapped terminal rows an
/// entry will occupy at the given width — WITHOUT actually
/// allocating wrapped lines. O(text length) per entry, no heap
/// allocations beyond the iterator.
///
/// Used by the viewport-slicing render path to find which
/// entries intersect the visible window in linear time, so the
/// expensive [`entry_lines`] + [`wrap_lines`] only runs on the
/// handful of entries actually in view. Without this, a huge
/// entry (e.g. the formatted `util::props` output) gets
/// fully re-wrapped on every draw — turning every wheel event
/// into multi-MB of per-char allocation.
///
/// The count must match what [`entry_lines`] + [`wrap_lines`]
/// actually produce: each natural text line contributes
/// `ceil((prefix + body_chars) / width)` wrapped rows (min 1).
/// The Input-entry timer suffix is ignored — when it fits it
/// pads the first line to exactly `width` (still 1 row); when
/// it doesn't fit it isn't added (so the body wraps normally
/// without it). Either way the row count matches.
pub fn count_wrapped_rows(entry: &ScrollbackEntry, width: u16) -> u32 {
    if width == 0 {
        return 1;
    }
    let w = width as usize;
    // Row-0 gutter width matches what `entry_lines_windowed` emits:
    //   - collapsible + expanded (Some(false)): 4 cells ("▼ " +
    //     kind prefix) on row 0, 4-space `cont_indent` on
    //     continuation rows.
    //   - collapsed (Some(true)): handled by the `▶` placeholder
    //     branch below.
    //   - non-collapsible (None): 2 cells ("kind_prefix").
    // Mirror the has-marker rule in `entry_lines_windowed`: Input
    // entries always carry the group-collapse marker; other entries
    // carry a marker only when they're expanded collapsibles.
    let prefix_width: usize = if matches!(entry.kind, ScrollbackKind::Input)
        || entry.collapse_state == Some(false)
    {
        4
    } else {
        2
    };
    // Collapsed entry: exactly one placeholder row of content,
    // wrapped like any other line if the preview + suffix exceed
    // the terminal width. The formula has to MATCH what
    // `collapsible_lines` actually emits, character for character
    // — any drift shifts every downstream entry's row index by the
    // rounding error, and a click on the visible content maps to
    // an adjacent buffer row (visible: "A total of 4711…", copies:
    // "· INFO: [Common 17-83] Releasing license…").
    if let Some(true) = entry.collapse_state {
        let preview = entry
            .text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let count = entry.text.lines().count();
        // Suffix exactly matches the `format!("  ({count} lines
        // hidden)")` in `collapsible_lines`: 3 (`"  ("`) + digits +
        // 14 (`" lines hidden)"`).
        let suffix_width = if count > 1 {
            3 + count.to_string().chars().count() + 14
        } else {
            0
        };
        let body_chars = preview.chars().count();
        let total = body_chars
            .saturating_add(prefix_width)
            .saturating_add(suffix_width)
            .max(1);
        return total.div_ceil(w).max(1) as u32;
    }
    let mut rows: u32 = 0;
    let mut had_lines = false;
    for line in entry.text.lines() {
        had_lines = true;
        let body_chars = line.chars().count();
        let total = body_chars.saturating_add(prefix_width).max(1);
        let line_rows = total.div_ceil(w).max(1);
        rows = rows.saturating_add(line_rows as u32);
    }
    if !had_lines {
        // Empty text → entry_lines emits one blank line.
        rows = 1;
    }
    rows
}

pub fn wrap_lines(input: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return input;
    }
    let w = width as usize;
    let mut out = Vec::with_capacity(input.len());
    for line in input {
        // Flatten spans → (char, style) so chunking can ignore the
        // span boundaries and only care about per-cell style.
        let mut chars: Vec<(char, Style)> = Vec::new();
        for span in &line.spans {
            for c in span.content.chars() {
                chars.push((c, span.style));
            }
        }
        if chars.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        for chunk in chars.chunks(w) {
            out.push(merge_to_line(chunk));
        }
    }
    out
}

fn merge_to_line(chunk: &[(char, Style)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut cur_style = chunk[0].1;
    for (c, st) in chunk {
        if *st != cur_style {
            spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            cur_style = *st;
        }
        buf.push(*c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style));
    }
    Line::from(spans)
}

/// Plain-text content of a [`Line`] — span styles dropped, content
/// concatenated. Used to extract the selected substring for clipboard
/// copy.
pub fn line_plain_text(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        out.push_str(span.content.as_ref());
    }
    out
}

/// Re-style cells in `lines` that fall inside the selection range,
/// `[start, end)`. Both endpoints are `(row, col)` indices into
/// `lines` (the post-wrap, post-scroll Vec). The range may be
/// inverted (cursor before anchor); callers should normalize first.
pub fn apply_selection_highlight(
    lines: &mut [Line<'static>],
    start: (usize, usize),
    end: (usize, usize),
) {
    let (sr, sc) = start;
    let (er, ec) = end;
    for (row_idx, line) in lines.iter_mut().enumerate() {
        if row_idx < sr || row_idx > er {
            continue;
        }
        let row_start = if row_idx == sr { sc } else { 0 };
        let row_end = if row_idx == er { ec } else { usize::MAX };
        highlight_cols(line, row_start, row_end);
    }
}

fn highlight_cols(line: &mut Line<'static>, start: usize, end: usize) {
    // Rebuild spans, splitting any that straddle the selection
    // boundary so the REVERSED modifier applies to exactly the cells
    // in [start, end).
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.spans.drain(..) {
        let span_chars: Vec<char> = span.content.chars().collect();
        let len = span_chars.len();
        let span_start = col;
        let span_end = col + len;
        col = span_end;

        if span_end <= start || span_start >= end {
            // Wholly outside selection — push unchanged.
            new_spans.push(span);
            continue;
        }

        // Compute the three potential sub-pieces [..lo, lo..hi, hi..]
        // where lo, hi are local offsets within span_chars.
        let lo = start.saturating_sub(span_start).min(len);
        let hi = end.saturating_sub(span_start).min(len);

        if lo > 0 {
            let s: String = span_chars[..lo].iter().collect();
            new_spans.push(Span::styled(s, span.style));
        }
        if hi > lo {
            let s: String = span_chars[lo..hi].iter().collect();
            new_spans.push(Span::styled(
                s,
                span.style.add_modifier(Modifier::REVERSED),
            ));
        }
        if hi < len {
            let s: String = span_chars[hi..].iter().collect();
            new_spans.push(Span::styled(s, span.style));
        }
    }
    line.spans = new_spans;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duration_seconds_under_minute() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn duration_mss_minute_to_hour() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1:00");
        assert_eq!(format_duration(Duration::from_secs(75)), "1:15");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59:59");
    }

    #[test]
    fn duration_hmmss_hour_plus() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1:01:01");
        assert_eq!(format_duration(Duration::from_secs(36_000)), "10:00:00");
    }

    #[test]
    fn duration_truncates_subsecond() {
        // 5.9s should render as "5s" — second granularity only,
        // never fractional.
        assert_eq!(format_duration(Duration::from_millis(5_900)), "5s");
    }

    // ------------------------------------------------------------------
    // Windowed slicing regression + correctness
    // ------------------------------------------------------------------

    fn result_entry(text: &str) -> crate::app::ScrollbackEntry {
        crate::app::ScrollbackEntry {
            kind: crate::app::ScrollbackKind::Result,
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

    /// A huge Result entry is what triggered the REPL lock-up: 200K
    /// source lines, each cheap on its own but O(entry) work per
    /// redraw when we allocated `Line`s for every one. `entry_lines`
    /// (which delegates to the windowed impl with a full range)
    /// still produces every row on demand — for the clipboard-copy
    /// path — but `entry_lines_windowed` with a tight range should
    /// produce only what fits.
    #[test]
    fn windowed_slicer_emits_only_visible_source_lines() {
        // Build an entry where each source line fits on ONE wrapped
        // row (short body + 2-cell prefix < area_width). Then the
        // number of wrapped rows the entry contributes is exactly
        // its source-line count, making the assertions easy to
        // reason about.
        let source: String =
            (0..200_000).map(|i| format!("pin_{i}\n")).collect();
        let entry = result_entry(&source);
        let (lines, offset) =
            entry_lines_windowed(&entry, 80, 100_000..100_030);
        // 30 lines requested, 30 lines emitted.
        assert_eq!(lines.len(), 30);
        // Window starts exactly on a source-line boundary — no
        // sub-line offset.
        assert_eq!(offset, 0);
    }

    /// The FIRST emitted source line may land mid-window when the
    /// window's start lands inside a line's wrapped output. The
    /// slicer emits the whole line (row 0 of that line) and
    /// reports the offset so ratatui's Paragraph::scroll can trim
    /// the top.
    #[test]
    fn windowed_slicer_returns_sub_line_offset_when_window_starts_mid_line() {
        // Build lines that wrap TO exactly 2 wrapped rows each on
        // a width-10 area (2 prefix + 12 body chars → 14 total →
        // ceil(14/10) = 2 rows).
        let source: String =
            (0..10).map(|i| format!("abcdefghjkl_{i:02}\n")).collect();
        let entry = result_entry(&source);
        // Window that starts on the SECOND row of source line 3.
        // Line 3's wrapped rows are [6, 8) globally; window [7, 12)
        // should emit lines 3..6 with an offset of 1 wrapped row.
        let (lines, offset) = entry_lines_windowed(&entry, 10, 7..12);
        assert!(!lines.is_empty(), "expected some lines emitted");
        assert_eq!(
            offset, 1,
            "first line's row 0 is one row before window.start"
        );
    }

    /// Full-range windowing must be identical to `entry_lines` —
    /// otherwise the `entry_lines` shim would silently render
    /// something different from what tests / clipboard-copy expect.
    #[test]
    fn windowed_slicer_full_range_matches_entry_lines() {
        let source = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
        let entry = result_entry(source);
        let unwindowed = entry_lines(&entry, 80);
        let (windowed, offset) = entry_lines_windowed(&entry, 80, 0..u32::MAX);
        assert_eq!(unwindowed.len(), windowed.len());
        for (a, b) in unwindowed.iter().zip(windowed.iter()) {
            assert_eq!(a.spans.len(), b.spans.len());
        }
        assert_eq!(offset, 0);
    }
}
