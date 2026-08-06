// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Syntax highlighter for compiler-emitted enum reprs.
//!
//! Reprs follow a uniform shape regardless of which enum produced
//! them (the compiler emits `Variant`, `Variant(payload)`, or
//! `Variant(\n  inner\n)` for any user-declared enum, and
//! dict/list reprs join entries with `\n`). This module recognizes
//! that shape line-by-line and emits styled
//! [`ratatui::text::Span`]s — keys in blue, variant names in teal,
//! punctuation in dim, scalar payloads in green.
//!
//! Shape-based, not name-based: the highlighter has no knowledge
//! of `Property` / `Properties` / any specific enum. It recognizes
//! the structural pattern (`IDENT '(' … ')'` for variant calls,
//! `KEY SP VARIANT …` for dict entries, bare `)` for multi-line
//! close), so adding a new enum to the htcl source automatically
//! gets the same highlighting on its repr output.
//!
//! Falls back to plain text when a line doesn't parse — non-repr
//! content (raw `puts` output, error messages, etc.) renders
//! normally.
//!
//! Color palette is exported so [`crate::render::entry_lines`]
//! can apply the same fallback `body_style` for unparsed runs.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use winnow::ascii::space0;
use winnow::combinator::repeat;
use winnow::error::ContextError;
use winnow::token::take_while;
use winnow::{ModalResult, Parser};

/// Semantic role of a highlighted piece of text. Backend-agnostic:
/// callers (the REPL's ratatui renderer, `vw run`'s ANSI stdout
/// renderer, or anything else consuming
/// [`highlight_line_pieces`]) map each variant to whatever styling
/// primitives their target supports. The RGB values in
/// [`key_style`] / [`variant_style`] / [`scalar_style`] and the
/// dim modifier in [`punct_style`] are the CANONICAL palette; every
/// backend should reproduce these colors so the two rendering
/// surfaces look identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    /// Uncolored spacing (indent, inter-token whitespace,
    /// trailing runs). Backends emit the text verbatim.
    Plain,
    /// Dict key (`CONFIG`, `CPM_PCIE0_MODES`, …).
    Key,
    /// Enum variant name (`Scalar`, `Nested`, …).
    Variant,
    /// Structural punctuation (`(` / `)`).
    Punct,
    /// Scalar payload (the string inside `Scalar(…)`).
    Scalar,
}

/// One styled fragment of a highlighted repr line.
#[derive(Debug, Clone)]
pub struct Piece {
    pub text: String,
    pub kind: StyleKind,
}

impl Piece {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StyleKind::Plain,
        }
    }
    fn styled(text: impl Into<String>, kind: StyleKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

/// Style for dict keys (`CONFIG`, `CPM_PCIE0_MODES`, …) — the
/// identifier immediately preceding a value.
pub fn key_style() -> Style {
    Style::default().fg(Color::Rgb(80, 150, 255))
}

/// Style for enum variant names (`Scalar`, `Nested`, …) — the
/// identifier immediately preceding `(`.
pub fn variant_style() -> Style {
    Style::default().fg(Color::Rgb(100, 200, 200))
}

/// Style for structural punctuation (`(` and `)`) — dimmed so the
/// nesting structure recedes visually next to keys and values.
pub fn punct_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Style for scalar payloads — the string inside `Scalar(…)`.
pub fn scalar_style() -> Style {
    Style::default().fg(Color::Rgb(120, 200, 120))
}

/// Convert a [`StyleKind`] to the ratatui [`Style`] the REPL's
/// scrollback uses. Kept as a single map so the REPL and any other
/// ratatui-based consumer stay in sync with the canonical palette.
pub fn ratatui_style(kind: StyleKind) -> Style {
    match kind {
        StyleKind::Plain => Style::default(),
        StyleKind::Key => key_style(),
        StyleKind::Variant => variant_style(),
        StyleKind::Punct => punct_style(),
        StyleKind::Scalar => scalar_style(),
    }
}

/// Try to recognize `line` as a compiler-emitted enum-repr line
/// and return a backend-agnostic piece sequence. Returns `None`
/// when the line doesn't match the repr grammar — callers fall
/// back to rendering the raw text with their default body style.
///
/// Callers targeting a ratatui surface (the REPL scrollback) can
/// use [`highlight_line`] for a Span-typed shortcut; callers
/// targeting a plain terminal (`vw run`'s stdout stream) consume
/// pieces directly and produce ANSI escapes.
pub fn highlight_line_pieces(line: &str) -> Option<Vec<Piece>> {
    let mut input = line;
    parse_line.parse_next(&mut input).ok().filter(|pieces| {
        // Reject parses that didn't consume the whole line — a
        // partial match means we'd silently style some tokens
        // and drop the rest. Better to fall through to plain.
        input.is_empty() && !pieces.is_empty()
    })
}

/// Ratatui-flavored wrapper around [`highlight_line_pieces`]. The
/// REPL's scrollback renderer consumes ratatui `Span`s directly.
pub fn highlight_line(line: &str) -> Option<Vec<Span<'static>>> {
    highlight_line_pieces(line).map(|pieces| {
        pieces
            .into_iter()
            .map(|p| match p.kind {
                StyleKind::Plain => Span::raw(p.text),
                _ => Span::styled(p.text, ratatui_style(p.kind)),
            })
            .collect()
    })
}

// Top-level line shapes:
//   `INDENT? ')' [SP] EOL`             — multi-line close
//   `INDENT? KEY SP VALUE [SP]? EOL`   — dict entry
fn parse_line(input: &mut &str) -> ModalResult<Vec<Piece>> {
    let mut pieces: Vec<Piece> = Vec::new();
    let indent = space0::<_, ContextError>.parse_next(input)?;
    if !indent.is_empty() {
        pieces.push(Piece::plain(indent));
    }
    // Multi-line-close line: bare `)`, optionally followed by trailing whitespace.
    if input.starts_with(')') {
        let close = ")";
        *input = &input[1..];
        pieces.push(Piece::styled(close, StyleKind::Punct));
        let trailing = space0::<_, ContextError>.parse_next(input)?;
        if !trailing.is_empty() {
            pieces.push(Piece::plain(trailing));
        }
        return Ok(pieces);
    }
    // Dict-entry line: KEY SP VALUE
    let key = parse_dict_key(input)?;
    pieces.push(Piece::styled(key, StyleKind::Key));
    let sp = take_while(1.., |c: char| c == ' ').parse_next(input)?;
    pieces.push(Piece::plain(sp));
    // Top-level: require the value to use parens to avoid false-
    // positives on plain prose `puts "Word Other"` Stdout lines.
    let value_pieces = parse_value(input, true)?;
    pieces.extend(value_pieces);
    Ok(pieces)
}

// VALUE is one of:
//   IDENT '(' INNER ')'   — single-line variant call with payload
//   IDENT '('             — multi-line open (line ends after `(`)
//   IDENT                 — empty-payload variant
//
// `require_parens = true` rejects the bare-IDENT form. We use that
// at the TOP LEVEL of a Stdout/Result line, where "KEY VARIANT"
// without parens is far more often plain prose (e.g.
// `puts "Configuring CIPS"` → `Configuring CIPS`) than an actual
// repr — styling that prose as if it were a typed value produces
// distracting false-positives. Inside an inline payload (sub-entries
// like `Nested(K1 Empty K2 Other(x))`) we accept bare IDENT because
// we've already seen the surrounding `Nested(`, so the context is
// unambiguous.
fn parse_value(
    input: &mut &str,
    require_parens: bool,
) -> ModalResult<Vec<Piece>> {
    let variant = parse_ident(input)?;
    let mut out = vec![Piece::styled(variant, StyleKind::Variant)];
    if !input.starts_with('(') {
        if require_parens {
            return Err(winnow::error::ErrMode::Backtrack(ContextError::new()));
        }
        return Ok(out);
    }
    *input = &input[1..];
    out.push(Piece::styled("(", StyleKind::Punct));
    if input.is_empty() {
        // `Variant(` at end of line — multi-line open. The
        // closing `)` will appear on a later line and be matched
        // by the close-only branch in `parse_line`.
        return Ok(out);
    }
    // Inline payload. The payload is everything up to the
    // matching close paren, with `(`/`)` balanced. Could be:
    //   - a scalar string (no inner parens): color green
    //   - a sub-entry KEY VARIANT(...) [SP KEY VARIANT(...)]*: recurse
    let payload_pieces = parse_inline_payload(input)?;
    out.extend(payload_pieces);
    if input.starts_with(')') {
        *input = &input[1..];
        out.push(Piece::styled(")", StyleKind::Punct));
    }
    Ok(out)
}

// Inline payload between `(` and its matching `)`. Recognizes
// either a single scalar (text with no parens) or a sequence of
// inline dict-entry-shaped sub-values (`KEY VARIANT(...) ...`).
// Stops at the closing `)` of the surrounding call.
fn parse_inline_payload(input: &mut &str) -> ModalResult<Vec<Piece>> {
    // Look ahead: does the payload look like `IDENT SP IDENT (`?
    // If so it's a sub-entry — recurse. Otherwise treat it as a
    // scalar value.
    if looks_like_sub_entry(input) {
        let mut out = Vec::new();
        // First sub-entry.
        let entry = parse_sub_entry(input)?;
        out.extend(entry);
        // Optional further sub-entries separated by space (for
        // dicts with multiple inline children).
        let more: Vec<Vec<Piece>> = repeat(
            0..,
            (
                take_while(1.., |c: char| c == ' ')
                    .map(|s: &str| Piece::plain(s)),
                parse_sub_entry,
            )
                .map(|(sp, mut e)| {
                    e.insert(0, sp);
                    e
                }),
        )
        .parse_next(input)?;
        for chunk in more {
            out.extend(chunk);
        }
        Ok(out)
    } else {
        // Scalar payload: take everything up to the matching close
        // paren of the surrounding call. Track paren depth so
        // scalar values containing their own `(…)` (e.g. Vivado's
        // `RS(544) CL119` FEC-config string) don't get cut short at
        // the FIRST `)` — that used to abort the parse, drop the
        // trailing text onto the caller's leftover input, and
        // fall the whole line back to gray.
        let start = *input;
        let mut depth: usize = 0;
        let mut end = 0;
        for (i, c) in start.char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    end = i;
                    break;
                }
                ')' => depth -= 1,
                _ => {}
            }
            end = i + c.len_utf8();
        }
        let scalar = &start[..end];
        *input = &start[end..];
        Ok(vec![Piece::styled(scalar, StyleKind::Scalar)])
    }
}

// A sub-entry inside an inline payload: KEY SP VARIANT [( ... )].
fn parse_sub_entry(input: &mut &str) -> ModalResult<Vec<Piece>> {
    let mut out = Vec::new();
    let key = parse_ident(input)?;
    out.push(Piece::styled(key, StyleKind::Key));
    let sp = take_while(1.., |c: char| c == ' ').parse_next(input)?;
    out.push(Piece::plain(sp));
    // Sub-entry: bare-ident variant allowed (we're already inside
    // an established `Variant(...)` so the context is unambiguous).
    let value_pieces = parse_value(input, false)?;
    out.extend(value_pieces);
    Ok(out)
}

// Best-effort lookahead: does the input start with `IDENT SP IDENT`
// (which would indicate a KEY SP VARIANT sub-entry rather than a
// bare scalar payload)? Doesn't consume input.
fn looks_like_sub_entry(input: &&str) -> bool {
    let s = input;
    let mut it = s.chars();
    // First ident
    let first_ok = matches!(
        it.next(),
        Some(c) if c.is_ascii_alphabetic() || c == '_',
    );
    if !first_ok {
        return false;
    }
    let mut saw_first = 1;
    for c in it.by_ref() {
        if c.is_ascii_alphanumeric() || c == '_' {
            saw_first += 1;
        } else if c == ' ' {
            break;
        } else {
            return false;
        }
    }
    if saw_first == 0 {
        return false;
    }
    // Next must be IDENT (after the space we just consumed).
    let mut second_count = 0;
    for c in it {
        if c.is_ascii_alphanumeric() || c == '_' {
            second_count += 1;
        } else {
            // A sub-entry's second ident is the variant name — must
            // be followed by `(` to count.
            return second_count > 0 && c == '(';
        }
    }
    false
}

// `[A-Za-z_][A-Za-z0-9_]*` — take identifier-shaped chars then
// verify the leading character is letter/underscore (we can't
// match the leading-letter constraint and the run cleanly with a
// single `take_while`, but it's fine to take everything plausible
// then reject if the leading char would have made it digit-led).
fn parse_ident<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    let ident =
        take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_')
            .parse_next(input)?;
    match ident.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => Ok(ident),
        _ => Err(winnow::error::ErrMode::Backtrack(ContextError::new())),
    }
}

/// Same as [`parse_ident`] but also accepts `.` in the middle —
/// used for dict KEYS, not variant names. Vivado's property keys
/// are `CONFIG.<PARAM>` style (a dot-composed namespace), so a
/// `<ip>::Config`-style repr line like `CONFIG.CPM_PCIE0_MODES
/// Scalar(None)` needs to accept the `.` as part of the key or
/// the whole line fails to parse and falls back to plain rendering
/// — which is what "the highlighter isn't working" looked like in
/// practice. Variant names (`Scalar`, `Nested`, `Empty`, …) still
/// use [`parse_ident`] so this stays confined to KEYS only.
fn parse_dict_key<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    let ident = take_while(1.., |c: char| {
        c.is_ascii_alphanumeric() || c == '_' || c == '.'
    })
    .parse_next(input)?;
    match ident.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            // Reject leading dot / trailing dot / consecutive dots
            // shape — those are structurally malformed keys, and
            // accepting them would silently paint bogus prose.
            if ident.starts_with('.')
                || ident.ends_with('.')
                || ident.contains("..")
            {
                Err(winnow::error::ErrMode::Backtrack(ContextError::new()))
            } else {
                Ok(ident)
            }
        }
        _ => Err(winnow::error::ErrMode::Backtrack(ContextError::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_only_line() {
        let spans = highlight_line(")").expect("parses");
        assert!(!spans.is_empty());
        // Last span content is ")"
        let last = &spans[spans.len() - 1];
        assert_eq!(last.content.as_ref(), ")");
    }

    #[test]
    fn indented_close_line() {
        let spans = highlight_line("  )").expect("parses");
        // First span = "  " (indent), last span = ")"
        assert_eq!(spans[0].content.as_ref(), "  ");
        assert_eq!(spans.last().unwrap().content.as_ref(), ")");
    }

    #[test]
    fn simple_scalar_entry() {
        let spans = highlight_line("CONFIG Scalar(foo)").expect("parses");
        // Should have spans for CONFIG, " ", Scalar, "(", foo, ")"
        let contents: Vec<&str> =
            spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            contents.contains(&"CONFIG"),
            "missing CONFIG span: {contents:?}"
        );
        assert!(
            contents.contains(&"Scalar"),
            "missing Scalar span: {contents:?}"
        );
        assert!(contents.contains(&"foo"), "missing foo span: {contents:?}");
    }

    #[test]
    fn variant_open_multiline() {
        let spans = highlight_line("CONFIG Nested(").expect("parses");
        let contents: Vec<&str> =
            spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(contents.contains(&"CONFIG"));
        assert!(contents.contains(&"Nested"));
        assert_eq!(spans.last().unwrap().content.as_ref(), "(");
    }

    /// The `<ip>::Config::repr` shape emits `CONFIG.<PARAM>
    /// Scalar(value)` — a dot-composed Vivado property key. The
    /// key parser has to accept dots or the whole line falls
    /// through to unstyled plain text.
    #[test]
    fn dotted_config_key_parses() {
        let spans = highlight_line("CONFIG.CPM_PCIE0_MODES Scalar(None)")
            .expect("parses");
        let contents: Vec<&str> =
            spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            contents.contains(&"CONFIG.CPM_PCIE0_MODES"),
            "dotted key not captured whole: {contents:?}"
        );
        assert!(contents.contains(&"Scalar"), "{contents:?}");
        assert!(contents.contains(&"None"), "{contents:?}");
    }

    /// Consecutive-dot / leading-dot / trailing-dot keys are
    /// rejected so genuine prose (`. Alignment ...`) doesn't
    /// silently get repainted as a repr.
    #[test]
    fn malformed_dotted_key_rejected() {
        assert!(highlight_line(".leading Scalar(x)").is_none());
        assert!(highlight_line("trailing. Scalar(x)").is_none());
        assert!(highlight_line("dou..ble Scalar(x)").is_none());
    }

    #[test]
    fn nested_inline_entry() {
        let spans =
            highlight_line("CONFIG Nested(CPM_PCIE0_MODES Scalar(None))")
                .expect("parses");
        let contents: Vec<&str> =
            spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(contents.contains(&"CONFIG"));
        assert!(contents.contains(&"Nested"));
        assert!(contents.contains(&"CPM_PCIE0_MODES"));
        assert!(contents.contains(&"Scalar"));
        assert!(contents.contains(&"None"));
    }

    #[test]
    fn non_repr_line_returns_none() {
        assert!(highlight_line("INFO: vivado started").is_none());
        assert!(highlight_line("just some random text").is_none());
        assert!(highlight_line("").is_none());
    }

    /// Regression: `Scalar(RS(544) CL119)` — Vivado's FEC-config
    /// property value contains its own `(…)` pair. The scalar
    /// payload parser must balance parens and stop only at the
    /// matching outer `)`, otherwise the whole line falls back to
    /// the gray no-repr styling.
    #[test]
    fn scalar_payload_with_inner_parens_still_highlights() {
        let spans = highlight_line("FEC_SLICE0_CFG_C0 Scalar(RS(544) CL119)")
            .expect("parses");
        let contents: Vec<&str> =
            spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            contents.contains(&"FEC_SLICE0_CFG_C0"),
            "missing key: {contents:?}"
        );
        assert!(
            contents.contains(&"Scalar"),
            "missing variant: {contents:?}"
        );
        assert!(
            contents.contains(&"RS(544) CL119"),
            "missing scalar payload: {contents:?}"
        );
    }

    #[test]
    fn plain_two_word_prose_not_styled_as_repr() {
        // Regression: stdout text like `puts "Configuring CIPS"` used
        // to match the `KEY EmptyVariant` shape and get colored. The
        // top-level parser must require parens to avoid this.
        assert!(highlight_line("Configuring CIPS").is_none());
        assert!(highlight_line("CPM 5 USER PROPS").is_none());
        assert!(highlight_line("Hello World").is_none());
    }
}
