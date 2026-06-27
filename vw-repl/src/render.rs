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
pub fn entry_lines(entry: &ScrollbackEntry) -> Vec<Line<'static>> {
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
    let mut out = Vec::new();
    for (i, line) in entry.text.lines().enumerate() {
        let leading = if i == 0 { prefix } else { "  " };
        out.push(Line::from(vec![
            Span::styled(leading.to_string(), prefix_style),
            Span::styled(line.to_string(), body_style),
        ]));
    }
    if out.is_empty() {
        out.push(Line::from(vec![Span::styled(
            prefix.to_string(),
            prefix_style,
        )]));
    }
    out
}

/// Split each input line into screen-row-sized chunks of `width`
/// columns, preserving span styles across the split. The output
/// renders 1:1 against screen rows when fed to a `Paragraph` with no
/// further wrapping, so screen-row N is `out[scroll + N]`.
///
/// Splitting is character-based (no word-boundary respect) — this is
/// REPL output, not prose; long Vivado property dicts and Tcl errors
/// don't have natural break points.
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
