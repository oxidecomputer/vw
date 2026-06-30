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
pub fn entry_lines(
    entry: &ScrollbackEntry,
    area_width: u16,
) -> Vec<Line<'static>> {
    let orange = Color::Rgb(255, 140, 0);
    let (prefix, prefix_style) = match entry.kind {
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
        ScrollbackKind::Notice => ("· ", Style::default().fg(Color::DarkGray)),
    };
    let body_style = match entry.kind {
        ScrollbackKind::Input => Style::default().fg(Color::White),
        ScrollbackKind::Result => Style::default().fg(Color::Gray),
        ScrollbackKind::Stdout => Style::default().fg(Color::White),
        ScrollbackKind::Error => Style::default().fg(Color::Red),
        ScrollbackKind::Warning => Style::default().fg(orange),
        ScrollbackKind::Notice => Style::default().fg(Color::DarkGray),
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
    for (i, line) in entry.text.lines().enumerate() {
        let leading = if i == 0 { prefix } else { "  " };
        let mut spans: Vec<Span<'static>> =
            vec![Span::styled(leading.to_string(), prefix_style)];
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
            if let Some((label, label_style)) = timer.as_ref() {
                let used: usize =
                    spans.iter().map(|s| display_cells(&s.content)).sum();
                let label_w = display_cells(label);
                if (used + label_w + 1) as u16 <= area_width {
                    let pad = area_width as usize - used - label_w;
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(label.clone(), *label_style));
                }
            }
        }
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(vec![Span::styled(
            prefix.to_string(),
            prefix_style,
        )]));
    }
    out
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
        Style::default().fg(Color::DarkGray)
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
    // Every entry kind gets a 2-cell prefix ("› ", "  ", etc.).
    let prefix_width = 2;
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
}
