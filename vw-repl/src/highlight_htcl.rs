// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Syntax highlighter for htcl source text.
//!
//! Single-pass character scanner with a small context-state machine.
//! Walks the source one byte at a time, recognizing comments, strings,
//! `$variable` refs, command substitutions, attributes, numeric
//! literals, and bare-word identifiers. The state machine tracks
//! "what was the previous token" so an identifier after `proc` is
//! styled as a declaration name, an identifier after `set` is styled
//! as a variable, etc.
//!
//! Why not the AST? The user-facing requirement is **stable**
//! highlighting — tokens keep the same color whether or not the
//! source as a whole parses. An AST-based highlighter has to
//! choose between (a) emitting nothing for unparseable regions
//! (flickers off mid-edit) or (b) merging with a separate lexical
//! pass that produces different output than the AST pass
//! (flickers between two styles when the parse status changes).
//! Neither is acceptable.
//!
//! A scanner-with-state produces the same output for `proc foo`
//! whether followed by `{x} unit { ... }` (complete) or by `{x`
//! (incomplete) — every recognizable byte run gets its consistent
//! token kind in one pass. Tree-sitter's grammar-with-error-recovery
//! gives the same property; a stateful scanner is the lighter-weight
//! analog for our small language.
//!
//! Token kinds:
//!
//! - **Keyword** — `proc`, `set`, `type`, `enum`, `src`, `namespace`,
//!   plus the control-flow / Tcl-builtin set.
//! - **Function (builtin)** — `puts`, `lappend`, `dict`, etc.
//! - **Declaration** — the identifier immediately following a
//!   declaration keyword (`proc foo`, `type T`, `enum E`).
//! - **Variable** — `$name`, `${name}`, and the identifier after
//!   `set`.
//! - **Type** — the identifier following `:` (proc arg type
//!   annotation), and the bare word after a proc decl's args
//!   brace (the return-type slot).
//! - **Parameter** — bare identifiers inside the first `{...}` of
//!   a proc decl (the args block).
//! - **Attribute** — `@name` on proc args.
//! - **String** — `"..."` quoted, and `{...}` braced when not a
//!   script body.
//! - **Comment / DocComment** — `#` / `##` to end of line, only
//!   when in command-start position.
//! - **Numeric** — integer literals.
//! - **Punctuation** — `[`, `]`, `(`, `)`, `:`. Brackets recurse:
//!   the scanner re-enters at `[` with fresh state for the inner
//!   command, then resumes the outer state after the matching `]`.

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span as RatatuiSpan;

/// Built-in command / control-flow words.
const BUILTIN_KEYWORDS: &[&str] = &[
    "if", "elseif", "else", "while", "for", "foreach", "switch", "catch",
    "return", "break", "continue", "expr", "eval", "uplevel", "upvar",
    "global", "variable", "try", "throw", "finally",
];

/// Built-in commands; styled distinct from user-proc calls.
/// `putr` is compile-time-rewritten to `puts [T::repr -v $x]` by
/// `vw_htcl::putr::rewrite` before any code reaches Tcl, but the
/// user writes it exactly like they write `puts` — same color for
/// visual consistency.
const BUILTIN_FUNCS: &[&str] = &[
    "puts", "putr", "lappend", "lindex", "llength", "lrange", "lsearch",
    "lset", "lsort", "dict", "list", "string", "incr", "format", "scan",
    "regexp", "regsub", "join", "split", "concat", "subst", "info",
];

/// Declaration keywords that name the next identifier as a decl.
const DECL_KEYWORDS: &[&str] = &["proc", "type", "enum", "namespace"];

/// One styled byte range. Used by both the scrollback renderer
/// (slice 2) and the input editor (slice 3) to apply per-token
/// styles on top of the entry's default body color.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenSpan {
    pub range: Range<usize>,
    pub style: Style,
}

// --- color palette --------------------------------------------------

pub fn keyword_style() -> Style {
    Style::default()
        .fg(Color::Rgb(180, 130, 220))
        .add_modifier(Modifier::BOLD)
}
pub fn builtin_func_style() -> Style {
    Style::default().fg(Color::Rgb(180, 130, 220))
}
pub fn function_style() -> Style {
    Style::default().fg(Color::Rgb(230, 200, 120))
}
pub fn declaration_style() -> Style {
    Style::default()
        .fg(Color::Rgb(230, 200, 120))
        .add_modifier(Modifier::BOLD)
}
pub fn parameter_style() -> Style {
    Style::default().fg(Color::Rgb(220, 220, 220))
}
pub fn variable_style() -> Style {
    Style::default().fg(Color::Rgb(130, 200, 230))
}
pub fn string_style() -> Style {
    Style::default().fg(Color::Rgb(140, 200, 130))
}
pub fn comment_style() -> Style {
    Style::default()
        .fg(Color::Rgb(110, 110, 110))
        .add_modifier(Modifier::DIM)
}
pub fn doc_comment_style() -> Style {
    Style::default()
        .fg(Color::Rgb(130, 160, 170))
        .add_modifier(Modifier::ITALIC)
}
pub fn type_style() -> Style {
    Style::default().fg(Color::Rgb(100, 200, 200))
}
pub fn attribute_style() -> Style {
    Style::default().fg(Color::Rgb(200, 130, 180))
}
pub fn numeric_style() -> Style {
    Style::default().fg(Color::Rgb(180, 200, 130))
}
pub fn punct_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

// --- public entry points --------------------------------------------

/// Scan `source` and return a sorted, non-overlapping list of
/// [`TokenSpan`]s. Output is stable across edits — the same byte
/// sequence produces the same tokens regardless of whether the
/// surrounding source parses successfully.
pub fn highlight_source(source: &str) -> Vec<TokenSpan> {
    let mut s = Scanner::new(source);
    s.scan_script(usize::MAX);
    s.out
}

/// Slice `text` per-line using the same token classifier as
/// [`highlight_source`], returning one styled-`Span` Vec per line.
/// Lines are split on `\n`. Bytes outside any token use `body_style`.
pub fn highlight_per_line(
    text: &str,
    body_style: Style,
) -> Vec<Vec<RatatuiSpan<'static>>> {
    let tokens = highlight_source(text);
    let mut out = Vec::new();
    let mut line_start: usize = 0;
    for line in text.split('\n') {
        let line_end = line_start + line.len();
        let mut spans: Vec<RatatuiSpan<'static>> = Vec::new();
        let mut cursor = line_start;
        for ts in tokens
            .iter()
            .filter(|t| t.range.start < line_end && t.range.end > line_start)
        {
            let start = ts.range.start.max(line_start);
            let end = ts.range.end.min(line_end);
            if cursor < start {
                spans.push(RatatuiSpan::styled(
                    text[cursor..start].to_string(),
                    body_style,
                ));
            }
            if start < end {
                spans.push(RatatuiSpan::styled(
                    text[start..end].to_string(),
                    ts.style,
                ));
                cursor = end;
            }
        }
        if cursor < line_end {
            spans.push(RatatuiSpan::styled(
                text[cursor..line_end].to_string(),
                body_style,
            ));
        }
        out.push(spans);
        line_start = line_end + 1;
    }
    out
}

// --- scanner --------------------------------------------------------

struct Scanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    out: Vec<TokenSpan>,
}

/// Per-command classification state. Reset at every command
/// boundary (`\n`, `;`, or the start of a `[...]` interior).
#[derive(Clone, Copy, Default)]
struct CmdState {
    /// What the previous bare-word token was. Used to classify the
    /// NEXT bare word in context (e.g. "after `proc`" → decl).
    prev: PrevToken,
    /// Word index within the current command (0 = command name,
    /// 1 = first arg, ...). Used to detect the `proc NAME ARGS RET`
    /// shape: at word 2 inside a proc decl, the args brace is
    /// expected; at word 3, the return-type ident.
    word_idx: usize,
    /// True when this command's word-0 was `proc`. Drives the
    /// "args-block braces hold parameters" + "word-3 is return
    /// type" behaviour.
    is_proc_decl: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum PrevToken {
    #[default]
    None,
    Keyword,    // proc / type / enum / namespace
    SetKeyword, // `set`
}

impl<'a> Scanner<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            out: Vec::new(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.pos + off).copied()
    }

    fn push(&mut self, range: Range<usize>, style: Style) {
        if range.start < range.end {
            self.out.push(TokenSpan { range, style });
        }
    }

    /// Scan a script: a sequence of commands until either end-of-
    /// source or a closing-bracket terminator (when we're inside a
    /// `[...]` substitution). `limit` is a byte ceiling on the scan
    /// (usize::MAX for top-level).
    fn scan_script(&mut self, limit: usize) {
        while self.pos < self.bytes.len() && self.pos < limit {
            self.skip_horizontal_ws();
            if self.pos >= limit {
                break;
            }
            match self.peek() {
                None => break,
                Some(b']') => return, // end of `[...]` interior
                Some(b'\n') | Some(b';') => {
                    self.pos += 1;
                    continue;
                }
                Some(b'#') => {
                    // Comment in command position.
                    self.scan_comment();
                    continue;
                }
                _ => {}
            }
            self.scan_command();
        }
    }

    /// Skip spaces and tabs. NOT newlines (which are command
    /// terminators).
    fn skip_horizontal_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' {
                self.pos += 1;
            } else if c == b'\\' && self.peek_at(1) == Some(b'\n') {
                // Line continuation.
                self.pos += 2;
            } else {
                break;
            }
        }
    }

    fn scan_command(&mut self) {
        let mut state = CmdState::default();
        loop {
            self.skip_horizontal_ws();
            match self.peek() {
                None => return,
                Some(b'\n') | Some(b';') | Some(b']') => return,
                _ => {}
            }
            self.scan_word(&mut state);
            state.word_idx += 1;
            // After scanning a word, advance state per its
            // classification — handled inside scan_word.
        }
    }

    fn scan_word(&mut self, state: &mut CmdState) {
        match self.peek() {
            Some(b'"') => {
                self.scan_quoted();
                state.prev = PrevToken::None;
            }
            Some(b'{') => {
                if state.is_proc_decl && state.word_idx == 2 {
                    // Proc args block: scan interior for params/types.
                    self.scan_proc_args();
                } else {
                    // All other braces — proc body (word 3 with no
                    // return type, or word 4 with one), control-flow
                    // condition / body, generic braced word — get
                    // scanned as a script. Tcl-convention braces are
                    // scripts; we always treat them that way so
                    // identifiers inside `if {…}`, `while {…}`, proc
                    // bodies, etc. get live highlighting. The narrow
                    // loss: braced return-type annotations
                    // (`proc foo {} {dict<string, string>} { … }`)
                    // get their interior scanned as a script rather
                    // than as a type — acceptable since the bare-word
                    // form is far more common.
                    self.scan_braced_as_script();
                }
                state.prev = PrevToken::None;
            }
            Some(b'[') => {
                self.scan_bracket_subst();
                state.prev = PrevToken::None;
            }
            Some(b'$') => {
                self.scan_var_ref();
                // After a $var, the next ident isn't a decl/var/type.
                state.prev = PrevToken::None;
            }
            Some(b'@') if state.word_idx > 0 => {
                self.scan_attribute();
                state.prev = PrevToken::None;
            }
            Some(c) if c.is_ascii_digit() => {
                self.scan_numeric();
                state.prev = PrevToken::None;
            }
            Some(b'-') => {
                // Could be a flag (`-name`) or a negative number.
                let next = self.peek_at(1);
                if matches!(next, Some(c) if c.is_ascii_digit()) {
                    self.scan_numeric();
                } else {
                    self.scan_bare_word(state);
                }
                state.prev = PrevToken::None;
            }
            Some(_) => {
                self.scan_bare_word(state);
            }
            None => (),
        }
    }

    /// Scan a `#` or `##` comment to end of line. Only called when
    /// `#` appears in command position.
    fn scan_comment(&mut self) {
        let start = self.pos;
        let is_doc = self.peek_at(1) == Some(b'#');
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
            self.pos += 1;
        }
        let style = if is_doc {
            doc_comment_style()
        } else {
            comment_style()
        };
        self.push(start..self.pos, style);
    }

    fn scan_quoted(&mut self) {
        let start = self.pos;
        self.pos += 1; // consume opening `"`
        while let Some(c) = self.peek() {
            if c == b'\\' && self.peek_at(1).is_some() {
                self.pos += 2;
                continue;
            }
            if c == b'"' {
                self.pos += 1;
                break;
            }
            self.pos += 1;
        }
        self.push(start..self.pos, string_style());
    }

    /// Scan a `{...}` proc-args block: emit braces as punct, walk
    /// the interior recognizing parameter names, `@attributes`,
    /// `:type` annotations, and `;` separators.
    fn scan_proc_args(&mut self) {
        let open = self.pos;
        self.pos += 1;
        self.push(open..open + 1, punct_style());
        let mut depth = 1usize;
        // Walk the interior emitting param-style for bare idents,
        // colon-then-type for annotations, attribute style for `@`.
        let mut expect_type = false;
        while let Some(c) = self.peek() {
            if c == b'{' {
                depth += 1;
                self.pos += 1;
                continue;
            }
            if c == b'}' {
                if depth == 1 {
                    let close = self.pos;
                    self.pos += 1;
                    self.push(close..close + 1, punct_style());
                    return;
                }
                depth -= 1;
                self.pos += 1;
                continue;
            }
            if c == b' ' || c == b'\t' || c == b'\n' || c == b';' {
                self.pos += 1;
                continue;
            }
            if c == b'#' {
                self.scan_comment();
                continue;
            }
            if c == b':' {
                let p = self.pos;
                self.pos += 1;
                self.push(p..p + 1, punct_style());
                expect_type = true;
                continue;
            }
            if c == b'@' {
                self.scan_attribute();
                continue;
            }
            if c.is_ascii_alphabetic() || c == b'_' {
                let start = self.pos;
                while let Some(d) = self.peek() {
                    if d.is_ascii_alphanumeric()
                        || d == b'_'
                        || d == b'<'
                        || d == b'>'
                        || d == b','
                    {
                        self.pos += 1;
                    } else if d == b':' && self.peek_at(1) == Some(b':') {
                        self.pos += 2;
                    } else {
                        break;
                    }
                }
                let style = if expect_type {
                    type_style()
                } else {
                    parameter_style()
                };
                self.push(start..self.pos, style);
                expect_type = false;
                continue;
            }
            // Unknown byte — skip.
            self.pos += 1;
        }
    }

    /// Scan `{...}` as a proc body: recurse with full script
    /// classification on the interior.
    fn scan_braced_as_script(&mut self) {
        let open = self.pos;
        self.pos += 1;
        self.push(open..open + 1, punct_style());
        let mut depth = 1usize;
        // Find the matching close brace position (best-effort).
        let mut p = self.pos;
        while p < self.bytes.len() {
            let c = self.bytes[p];
            if c == b'\\' && p + 1 < self.bytes.len() {
                p += 2;
                continue;
            }
            if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            p += 1;
        }
        // Recurse on the interior [self.pos .. p).
        let close_pos = p;
        self.scan_script(close_pos);
        // Skip past the matching close brace if we found one.
        if close_pos < self.bytes.len() && self.bytes[close_pos] == b'}' {
            self.pos = close_pos;
            self.push(close_pos..close_pos + 1, punct_style());
            self.pos += 1;
        } else {
            // EOF without close — leave the cursor at end.
            self.pos = close_pos.max(self.pos);
        }
    }

    /// `[command-substitution]` — recurse on the interior as a
    /// fresh script.
    fn scan_bracket_subst(&mut self) {
        let open = self.pos;
        self.push(open..open + 1, punct_style());
        self.pos += 1;
        self.scan_script(usize::MAX);
        if let Some(b']') = self.peek() {
            let close = self.pos;
            self.push(close..close + 1, punct_style());
            self.pos += 1;
        }
    }

    fn scan_var_ref(&mut self) {
        let start = self.pos;
        self.pos += 1; // $
        if self.peek() == Some(b'{') {
            self.pos += 1;
            while let Some(c) = self.peek() {
                self.pos += 1;
                if c == b'}' {
                    break;
                }
            }
        } else {
            while let Some(c) = self.peek() {
                if c.is_ascii_alphanumeric()
                    || c == b'_'
                    || c == b':' && self.peek_at(1) == Some(b':')
                {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if self.pos > start + 1 {
            self.push(start..self.pos, variable_style());
        }
    }

    fn scan_attribute(&mut self) {
        let start = self.pos;
        self.pos += 1; // `@`
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos > start + 1 {
            self.push(start..self.pos, attribute_style());
        }
        // Optional `(...)` payload — consume as a balanced group.
        if self.peek() == Some(b'(') {
            let p = self.pos;
            self.pos += 1;
            self.push(p..p + 1, punct_style());
            let mut depth = 1usize;
            while let Some(c) = self.peek() {
                if c == b'(' {
                    depth += 1;
                    self.pos += 1;
                } else if c == b')' {
                    depth -= 1;
                    self.pos += 1;
                    if depth == 0 {
                        self.push(self.pos - 1..self.pos, punct_style());
                        return;
                    }
                } else if c == b'"' {
                    self.scan_quoted();
                } else if c.is_ascii_digit() {
                    self.scan_numeric();
                } else if c.is_ascii_alphabetic() || c == b'_' {
                    let s = self.pos;
                    while let Some(d) = self.peek() {
                        if d.is_ascii_alphanumeric() || d == b'_' {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    self.push(s..self.pos, string_style());
                } else {
                    self.pos += 1;
                }
            }
        }
    }

    fn scan_numeric(&mut self) {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos > start {
            self.push(start..self.pos, numeric_style());
        }
    }

    fn scan_bare_word(&mut self, state: &mut CmdState) {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric()
                || c == b'_'
                || c == b'-'
                || c == b'.'
                || c == b'/'
            {
                self.pos += 1;
            } else if c == b':' && self.peek_at(1) == Some(b':') {
                self.pos += 2;
            } else {
                break;
            }
        }
        if self.pos == start {
            // No bare-word chars (probably punctuation we don't know).
            self.pos += 1;
            return;
        }
        let word = &self.source[start..self.pos];
        let style = match state.prev {
            PrevToken::Keyword => {
                // Declaration name follows a decl keyword.
                state.prev = PrevToken::None;
                Some(declaration_style())
            }
            PrevToken::SetKeyword => {
                state.prev = PrevToken::None;
                Some(variable_style())
            }
            PrevToken::None => {
                if state.word_idx == 0 {
                    // Command position: keyword / builtin / function.
                    if DECL_KEYWORDS.contains(&word) {
                        state.prev = PrevToken::Keyword;
                        if word == "proc" {
                            state.is_proc_decl = true;
                        }
                        Some(keyword_style())
                    } else if word == "set" {
                        state.prev = PrevToken::SetKeyword;
                        Some(keyword_style())
                    } else if BUILTIN_KEYWORDS.contains(&word) {
                        Some(keyword_style())
                    } else if BUILTIN_FUNCS.contains(&word) {
                        Some(builtin_func_style())
                    } else {
                        Some(function_style())
                    }
                } else if state.is_proc_decl && state.word_idx == 3 {
                    // Return-type slot (bare-ident form).
                    Some(type_style())
                } else if word.starts_with('-') && word.len() > 1 {
                    // Flag-style word.
                    Some(attribute_style())
                } else {
                    // Argument to a call — leave to body color, with
                    // the exception that if the next char is `:` we
                    // still treat this as a parameter name (rare in
                    // call args but harmless to leave unstyled).
                    None
                }
            }
        };
        if let Some(s) = style {
            self.push(start..self.pos, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlights(source: &str) -> Vec<(String, &'static str)> {
        highlight_source(source)
            .into_iter()
            .map(|s| {
                let text = source[s.range.clone()].to_string();
                let tag = style_tag(&s.style);
                (text, tag)
            })
            .collect()
    }

    fn style_tag(style: &Style) -> &'static str {
        if style == &keyword_style() {
            "keyword"
        } else if style == &builtin_func_style() {
            "builtin_func"
        } else if style == &function_style() {
            "function"
        } else if style == &declaration_style() {
            "decl"
        } else if style == &parameter_style() {
            "param"
        } else if style == &variable_style() {
            "var"
        } else if style == &string_style() {
            "string"
        } else if style == &comment_style() {
            "comment"
        } else if style == &doc_comment_style() {
            "doc"
        } else if style == &type_style() {
            "type"
        } else if style == &attribute_style() {
            "attr"
        } else if style == &numeric_style() {
            "num"
        } else if style == &punct_style() {
            "punct"
        } else {
            "?"
        }
    }

    fn has(h: &[(String, &str)], text: &str, tag: &str) -> bool {
        h.iter().any(|(t, g)| t == text && *g == tag)
    }

    #[test]
    fn empty_input() {
        assert!(highlight_source("").is_empty());
    }

    #[test]
    fn set_keyword_var_value() {
        let h = highlights("set foo 42");
        assert!(has(&h, "set", "keyword"), "{h:?}");
        assert!(has(&h, "foo", "var"), "{h:?}");
        assert!(has(&h, "42", "num"), "{h:?}");
    }

    #[test]
    fn generic_call_first_word_function() {
        let h = highlights("foo bar baz");
        assert!(has(&h, "foo", "function"), "{h:?}");
    }

    #[test]
    fn builtin_control_flow_keyword() {
        let h = highlights("if {$x > 0} { puts hi }");
        assert!(has(&h, "if", "keyword"), "{h:?}");
        // `puts` inside the body braces should still highlight as a
        // builtin func because the body is scanned as a script.
        assert!(has(&h, "puts", "builtin_func"), "{h:?}");
        // $x inside the condition braces gets var styling via the
        // braced-as-string scanner... but the condition braces are
        // treated as a string today. v1 accepts that — the cost of
        // braced-string consistency is occasional loss of $var
        // styling inside conditions. Acceptable trade.
        let _ = h;
    }

    #[test]
    fn proc_decl_keywords_and_names() {
        let h = highlights("proc foo {x: int} bool { return $x }");
        assert!(has(&h, "proc", "keyword"));
        assert!(has(&h, "foo", "decl"));
        assert!(has(&h, "x", "param"));
        assert!(has(&h, "int", "type"));
        assert!(has(&h, "bool", "type"));
        // Body scanned as script.
        assert!(has(&h, "return", "keyword"));
        assert!(has(&h, "$x", "var"));
    }

    #[test]
    fn proc_with_attribute() {
        let h = highlights("proc foo {@default(0) x: int} unit {}");
        assert!(has(&h, "@default", "attr"));
        assert!(has(&h, "0", "num"));
        assert!(has(&h, "x", "param"));
        assert!(has(&h, "int", "type"));
        assert!(has(&h, "unit", "type"));
    }

    #[test]
    fn type_decl() {
        let h = highlights("type Properties = {dict<string, Property>}");
        assert!(has(&h, "type", "keyword"));
        assert!(has(&h, "Properties", "decl"));
    }

    #[test]
    fn enum_decl() {
        let h = highlights("enum Direction = {North South East West}");
        assert!(has(&h, "enum", "keyword"));
        assert!(has(&h, "Direction", "decl"));
    }

    #[test]
    fn doc_comment_vs_regular() {
        let h = highlights("# regular\n## doc\nset x 1");
        assert!(h
            .iter()
            .any(|(t, tag)| t.contains("regular") && *tag == "comment"));
        assert!(h.iter().any(|(t, tag)| t.contains("doc") && *tag == "doc"));
    }

    #[test]
    fn cmd_substitution_recurses() {
        let h = highlights("set y [foo $x]");
        assert!(has(&h, "set", "keyword"));
        assert!(has(&h, "foo", "function"));
        assert!(has(&h, "$x", "var"));
        assert!(has(&h, "[", "punct"));
        assert!(has(&h, "]", "punct"));
    }

    #[test]
    fn quoted_string() {
        let h = highlights("puts \"hello world\"");
        assert!(has(&h, "puts", "builtin_func"));
        assert!(has(&h, "\"hello world\"", "string"));
    }

    // The crucial stability tests: incomplete input must produce
    // the SAME classifications for the tokens that ARE present.
    #[test]
    fn stable_on_unclosed_proc_brace() {
        let complete = highlights("proc foo {x: int} bool { return 0 }");
        let incomplete = highlights("proc foo {x: int} bool { return 0");
        // The tokens shared between the two must classify the same.
        // Specifically: proc/foo/x/int/bool/return all match.
        for (text, tag) in &[
            ("proc", "keyword"),
            ("foo", "decl"),
            ("x", "param"),
            ("int", "type"),
            ("bool", "type"),
            ("return", "keyword"),
        ] {
            assert!(
                has(&complete, text, tag),
                "complete missing ({text}, {tag}): {complete:?}"
            );
            assert!(
                has(&incomplete, text, tag),
                "incomplete missing ({text}, {tag}): {incomplete:?}"
            );
        }
    }

    #[test]
    fn stable_on_unclosed_outer_brace() {
        let complete = highlights("proc foo {}");
        let incomplete = highlights("proc foo {");
        for (text, tag) in &[("proc", "keyword"), ("foo", "decl")] {
            assert!(has(&complete, text, tag), "complete missing");
            assert!(has(&incomplete, text, tag), "incomplete missing");
        }
    }

    #[test]
    fn stable_on_unclosed_bracket() {
        let complete = highlights("set y [foo $x]");
        let incomplete = highlights("set y [foo $x");
        for (text, tag) in
            &[("set", "keyword"), ("foo", "function"), ("$x", "var")]
        {
            assert!(
                has(&complete, text, tag),
                "complete missing ({text}, {tag})"
            );
            assert!(
                has(&incomplete, text, tag),
                "incomplete missing ({text}, {tag}): {incomplete:?}"
            );
        }
    }

    #[test]
    fn no_panic_on_garbage() {
        let _ = highlight_source("[[[");
        let _ = highlight_source("{{{");
        let _ = highlight_source("$$$");
        let _ = highlight_source("");
        let _ = highlight_source("# unterminated\nproc \"weird name\"");
    }

    #[test]
    fn plain_prose_not_styled_as_decl() {
        // Top-level plain words shouldn't get function styling for
        // their non-first words (i.e. just the first word styles
        // as function-call; the rest are left to body color).
        let h = highlights("just some words");
        assert!(has(&h, "just", "function"));
        // "some" and "words" are call args — no special styling.
        assert!(!has(&h, "some", "decl"));
        assert!(!has(&h, "words", "decl"));
    }

    #[test]
    fn proc_body_contents_scan_as_script() {
        // Regression: the body of a proc decl (without a return-type
        // annotation) used to be styled as a single type span,
        // painting the whole interior teal. Body must scan as a
        // normal script so `set`, `[...]`, `$vars`, etc. all
        // classify normally.
        let h =
            highlights("proc foo { x: int } { set foo [reticulate -value x] }");
        // Inside the body:
        assert!(has(&h, "set", "keyword"), "{h:?}");
        // `foo` after `set` is the variable being assigned.
        assert!(
            h.iter().any(|(t, tag)| t == "foo" && *tag == "var"),
            "expected `foo` styled as var inside body: {h:?}"
        );
        assert!(has(&h, "reticulate", "function"), "{h:?}");
        assert!(has(&h, "[", "punct"));
        assert!(has(&h, "]", "punct"));
    }

    #[test]
    fn body_with_and_without_return_type_scan_same() {
        // Identical body content should classify the same whether
        // the proc has a return-type annotation or not.
        let with = highlights("proc f {} unit { set x 1 }");
        let without = highlights("proc f {} { set x 1 }");
        for (text, tag) in &[("set", "keyword"), ("x", "var")] {
            assert!(has(&with, text, tag), "with: missing ({text}, {tag})");
            assert!(
                has(&without, text, tag),
                "without: missing ({text}, {tag}): {without:?}"
            );
        }
    }

    #[test]
    fn per_line_preserves_content() {
        let src = "set a 1\nproc f {} unit {}\nputs hi";
        let lines = highlight_per_line(src, Style::default());
        let mut reconstructed = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                reconstructed.push('\n');
            }
            for span in line {
                reconstructed.push_str(span.content.as_ref());
            }
        }
        assert_eq!(reconstructed, src);
    }
}
