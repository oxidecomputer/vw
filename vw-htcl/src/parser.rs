// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! htcl source parser.
//!
//! Builds a [`Document`] CST plus a list of [`ParseError`]s. The parser
//! is error-tolerant: when a command can't be parsed it is recorded as
//! a [`Stmt::Error`] and the parser resyncs at the next statement
//! boundary (newline or semicolon). This is what makes the same parser
//! usable from the LSP, where input is incomplete by definition.
//!
//! The outer statement loop is hand-rolled (it owns recovery and
//! collects doc comments); inner pieces — words, parts, escapes —
//! drive [`winnow::LocatingSlice`] for position tracking. As the grammar
//! grows past Phase 0 the inner pieces will lean on winnow combinators
//! more heavily.

use winnow::stream::{Location, Stream};
use winnow::LocatingSlice;

use crate::ast::*;
use crate::span::Span;

type Input<'i> = LocatingSlice<&'i str>;

#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ParseOutput {
    pub document: Document,
    pub errors: Vec<ParseError>,
}

pub fn parse(source: &str) -> ParseOutput {
    let mut input = LocatingSlice::new(source);
    let mut errors = Vec::new();
    let mut document =
        parse_document(&mut input, source, &mut errors, Mode::Toplevel);
    populate_procs(&mut document.stmts, source, &mut errors);
    ParseOutput { document, errors }
}

/// Statement-termination mode for the parser.
///
/// At the top level (and inside proc bodies, which are themselves
/// scripts) a newline ends a command — the historical Tcl rule. Inside
/// a `[ … ]` command substitution we relax that: newlines are
/// whitespace and only `;` (or the closing bracket, which is EOF for
/// the interior parser) terminates a command. That lets a single call
/// span lines without `\` continuations, e.g.
///
/// ```htcl
/// set x [
///   create_cpm5_cpm_pcie0
///     -cell cpm5
///     -max_link_speed 32.0_GT/s
/// ]
/// ```
///
/// Multi-command `[…]` (rare in practice — only the last command's
/// value flows out) still works via explicit `;`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Toplevel,
    BracketBody,
}

/// Post-pass over every `proc` — top-level *and* nested — that fills
/// in the structured args [`signature`](crate::ast::Proc::signature)
/// and parses the proc [`body`](crate::ast::Proc::body) into real
/// statements.
///
/// The body is parsed as a standalone fragment (its braces are
/// already stripped by [`inner_text_span`]); the resulting spans are
/// relative to the fragment, so they're shifted by the body's start
/// offset back into whole-source coordinates before being stored.
/// After shifting they're absolute, which lets the recursion process
/// nested procs against the original `source` uniformly.
fn populate_procs(
    stmts: &mut [crate::ast::Stmt],
    source: &str,
    errors: &mut Vec<ParseError>,
) {
    use crate::ast::{CommandKind, Stmt};
    use crate::proc_args::parse_proc_args;
    for stmt in stmts.iter_mut() {
        let Stmt::Command(cmd) = stmt else { continue };

        // Walk every word's parts and parse `[…]` command substitution
        // interiors into statements. Spans inside the parsed body get
        // shifted into whole-source coordinates so downstream analyses
        // can navigate them uniformly with top-level commands.
        for word in &mut cmd.words {
            populate_cmd_subst_parts(&mut word.parts, source, errors);
        }

        match &mut cmd.kind {
            CommandKind::Proc(proc) => {
                let (sig, errs) = parse_proc_args(source, proc.args_span);
                errors.extend(errs);
                proc.signature = Some(sig);

                let delta = proc.body_span.start;
                let body_text = proc.body_span.slice(source);
                // Proc bodies are scripts — newlines still terminate
                // statements there.
                let (mut body_stmts, body_errs) =
                    parse_fragment(body_text, Mode::Toplevel);
                for stmt in &mut body_stmts {
                    shift_stmt(stmt, delta);
                }
                for mut err in body_errs {
                    err.span = err.span.shifted(delta);
                    errors.push(err);
                }
                proc.body = body_stmts;

                // Spans are now absolute, so nested procs can be processed
                // against the same `source`.
                populate_procs(&mut proc.body, source, errors);
            }
            CommandKind::NamespaceEval(ns) => {
                // Same body-recursion as `proc` — the braced body is
                // a script fragment, parsed in toplevel mode so
                // newlines terminate statements normally.
                let delta = ns.body_span.start;
                let body_text = ns.body_span.slice(source);
                let (mut body_stmts, body_errs) =
                    parse_fragment(body_text, Mode::Toplevel);
                for stmt in &mut body_stmts {
                    shift_stmt(stmt, delta);
                }
                for mut err in body_errs {
                    err.span = err.span.shifted(delta);
                    errors.push(err);
                }
                ns.body = body_stmts;
                populate_procs(&mut ns.body, source, errors);
            }
            _ => {}
        }
    }
}

fn populate_cmd_subst_parts(
    parts: &mut [WordPart],
    source: &str,
    errors: &mut Vec<ParseError>,
) {
    for part in parts {
        let WordPart::CmdSubst {
            source: text,
            span,
            body,
        } = part
        else {
            continue;
        };
        // `span` covers the whole `[...]` including the brackets, and
        // `text` is the interior; the first interior byte sits at
        // `span.start + 1`.
        let delta = span.start + 1;
        // Bracket-body mode: newlines are whitespace, so multi-line
        // calls inside `[ … ]` parse as one command without `\`.
        let (mut body_stmts, body_errs) =
            parse_fragment(text, Mode::BracketBody);
        for s in &mut body_stmts {
            shift_stmt(s, delta);
        }
        for mut err in body_errs {
            err.span = err.span.shifted(delta);
            errors.push(err);
        }
        *body = body_stmts;
        populate_procs(body, source, errors);
    }
}

/// Parse a fragment of htcl (e.g. a proc body) into statements. Spans
/// are relative to `text`; the caller shifts them into whole-source
/// coordinates.
fn parse_fragment(
    text: &str,
    mode: Mode,
) -> (Vec<crate::ast::Stmt>, Vec<ParseError>) {
    let mut input = LocatingSlice::new(text);
    let mut errors = Vec::new();
    let document = parse_document(&mut input, text, &mut errors, mode);
    (document.stmts, errors)
}

fn shift_stmt(stmt: &mut crate::ast::Stmt, delta: u32) {
    use crate::ast::Stmt;
    match stmt {
        Stmt::Command(cmd) => shift_command(cmd, delta),
        Stmt::Comment(c) => c.span = c.span.shifted(delta),
        Stmt::Error(e) => e.span = e.span.shifted(delta),
    }
}

fn shift_command(cmd: &mut Command, delta: u32) {
    cmd.span = cmd.span.shifted(delta);
    for word in &mut cmd.words {
        shift_word(word, delta);
    }
    // At this stage nested procs carry only the spans produced by
    // `parse_document`; `signature` is still `None` and `body` empty,
    // both filled later by the caller's `populate_procs` recursion.
    match &mut cmd.kind {
        CommandKind::Proc(proc) => {
            proc.name_span = proc.name_span.shifted(delta);
            proc.args_span = proc.args_span.shifted(delta);
            proc.body_span = proc.body_span.shifted(delta);
        }
        CommandKind::NamespaceEval(ns) => {
            ns.name_span = ns.name_span.shifted(delta);
            ns.body_span = ns.body_span.shifted(delta);
        }
        _ => {}
    }
}

fn shift_word(word: &mut Word, delta: u32) {
    word.span = word.span.shifted(delta);
    for part in &mut word.parts {
        let span = match part {
            WordPart::Text { span, .. }
            | WordPart::VarRef { span, .. }
            | WordPart::CmdSubst { span, .. }
            | WordPart::Escape { span, .. } => span,
        };
        *span = span.shifted(delta);
    }
}

#[derive(Clone, Debug)]
struct InnerError {
    message: String,
    #[allow(dead_code)]
    span: Span,
}

fn parse_document(
    input: &mut Input<'_>,
    source: &str,
    errors: &mut Vec<ParseError>,
    mode: Mode,
) -> Document {
    let start = input.location();
    let mut stmts = Vec::new();
    let mut pending_docs: Vec<String> = Vec::new();

    loop {
        skip_inline_ws(input, source, mode);
        if at_eof(input, source) {
            break;
        }
        let c = current_char(input, source);
        // In `BracketBody`, `\n` is whitespace consumed by
        // `skip_inline_ws`, so it never reaches this match.
        let is_separator = match mode {
            Mode::Toplevel => c == '\n' || c == ';',
            Mode::BracketBody => c == ';',
        };
        match c {
            _ if is_separator => {
                advance_char(input);
                // A statement separator drops any orphan doc comments;
                // doc comments only attach to the immediately
                // following command.
                if matches!(c, ';') {
                    // semicolons don't break doc attachment within a line
                } else {
                    // Blank line breaks the doc-comment run only if
                    // the next non-whitespace is itself a blank line.
                    // For v0 we keep this simple: any `\n` between a
                    // doc comment and the next command keeps the
                    // attachment so long as nothing else intervenes.
                }
                continue;
            }
            '#' => {
                let comment = parse_comment(input, source);
                if comment.is_doc {
                    pending_docs.push(comment.text.clone());
                } else {
                    pending_docs.clear();
                }
                stmts.push(Stmt::Comment(comment));
            }
            _ => {
                let cmd_start = input.location();
                match parse_command(input, source, mode) {
                    Ok(mut cmd) => {
                        cmd.doc_comments = std::mem::take(&mut pending_docs);
                        stmts.push(Stmt::Command(cmd));
                    }
                    Err(err) => {
                        pending_docs.clear();
                        // Resync to the next statement boundary. In
                        // `BracketBody` only `;` breaks; the surrounding
                        // `]` is EOF for the interior parser.
                        while !at_eof(input, source) {
                            let c = current_char(input, source);
                            let stop = match mode {
                                Mode::Toplevel => c == '\n' || c == ';',
                                Mode::BracketBody => c == ';',
                            };
                            if stop {
                                break;
                            }
                            advance_char(input);
                        }
                        let span = Span::new(
                            cmd_start as u32,
                            input.location() as u32,
                        );
                        errors.push(ParseError {
                            message: err.message.clone(),
                            span,
                        });
                        stmts.push(Stmt::Error(ParseFailure {
                            message: err.message,
                            span,
                        }));
                    }
                }
            }
        }
    }

    Document {
        stmts,
        span: Span::new(start as u32, input.location() as u32),
    }
}

fn parse_comment(input: &mut Input<'_>, source: &str) -> Comment {
    let start = input.location();
    advance_char(input); // leading `#`
    let mut is_doc = false;
    if !at_eof(input, source) && current_char(input, source) == '#' {
        is_doc = true;
        advance_char(input);
    }
    // Leading single space after `#` / `##` is conventionally part of
    // the marker; trim it so callers see the raw comment text.
    if !at_eof(input, source) && current_char(input, source) == ' ' {
        advance_char(input);
    }
    let text_start = input.location();
    while !at_eof(input, source) {
        let c = current_char(input, source);
        if c == '\n' {
            break;
        }
        advance_char(input);
    }
    let text_end = input.location();
    Comment {
        text: source[text_start..text_end].to_string(),
        span: Span::new(start as u32, text_end as u32),
        is_doc,
    }
}

fn parse_command(
    input: &mut Input<'_>,
    source: &str,
    mode: Mode,
) -> Result<Command, InnerError> {
    let start = input.location();
    let mut words = Vec::new();
    loop {
        skip_inline_ws(input, source, mode);
        if at_eof(input, source) {
            break;
        }
        let c = current_char(input, source);
        let terminate = match mode {
            Mode::Toplevel => c == '\n' || c == ';',
            // In bracket-body, only `;` terminates a command — `\n`
            // is whitespace consumed by `skip_inline_ws`.
            Mode::BracketBody => c == ';',
        };
        if terminate {
            break;
        }
        words.push(parse_word(input, source)?);
    }
    if words.is_empty() {
        return Err(InnerError {
            message: "expected command".into(),
            span: Span::new(start as u32, input.location() as u32),
        });
    }
    let span = Span::new(start as u32, input.location() as u32);
    let kind = classify_command(&words);
    Ok(Command {
        words,
        span,
        kind,
        doc_comments: Vec::new(),
    })
}

fn classify_command(words: &[Word]) -> CommandKind {
    let Some(first) = words.first() else {
        return CommandKind::Generic;
    };
    match first.as_text() {
        Some("set") => CommandKind::Set,
        Some("src") if words.len() == 2 => {
            let path_word = &words[1];
            CommandKind::Src(SrcImport {
                path: path_word.as_text().map(String::from),
                path_span: path_word.span,
            })
        }
        Some("proc") if words.len() >= 4 => {
            let name_word = &words[1];
            let args_word = &words[2];
            let body_word = &words[3];
            let name = name_word.as_text().map(|s| s.to_string());
            CommandKind::Proc(Proc {
                name,
                name_span: name_word.span,
                args_span: inner_text_span(args_word),
                body_span: inner_text_span(body_word),
                signature: None,
                body: Vec::new(),
            })
        }
        Some("namespace")
            if words.len() >= 4
                && words.get(1).and_then(Word::as_text) == Some("eval") =>
        {
            let name_word = &words[2];
            let body_word = &words[3];
            CommandKind::NamespaceEval(crate::ast::NamespaceEval {
                name: name_word.as_text().map(String::from),
                name_span: name_word.span,
                body_span: inner_text_span(body_word),
                body: Vec::new(),
            })
        }
        _ => CommandKind::Generic,
    }
}

/// For a braced word, return the span of the brace contents (without
/// the braces themselves). For any other word, return its full span.
/// Used so Phase 2's structured-proc reparse and the LSP can point at
/// the parseable interior directly.
fn inner_text_span(word: &Word) -> Span {
    if word.form == WordForm::Braced {
        if let [WordPart::Text { span, .. }] = word.parts.as_slice() {
            return *span;
        }
    }
    word.span
}

fn parse_word(input: &mut Input<'_>, source: &str) -> Result<Word, InnerError> {
    let start = input.location();
    let c = current_char(input, source);
    let (form, parts) = match c {
        '{' => parse_braced_word(input, source)?,
        '"' => parse_quoted_word(input, source)?,
        _ => parse_bare_word(input, source)?,
    };
    let end = input.location();
    Ok(Word {
        form,
        parts,
        span: Span::new(start as u32, end as u32),
    })
}

fn parse_braced_word(
    input: &mut Input<'_>,
    source: &str,
) -> Result<(WordForm, Vec<WordPart>), InnerError> {
    let open_cp = input.checkpoint();
    let open = input.location();
    advance_char(input); // {
    let inner_start = input.location();
    let mut depth = 1usize;
    while !at_eof(input, source) {
        let c = current_char(input, source);
        match c {
            '\\' => {
                advance_char(input);
                if !at_eof(input, source) {
                    advance_char(input);
                }
            }
            '{' => {
                depth += 1;
                advance_char(input);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let inner_end = input.location();
                    advance_char(input);
                    let text = source[inner_start..inner_end].to_string();
                    return Ok((
                        WordForm::Braced,
                        vec![WordPart::Text {
                            value: text,
                            span: Span::new(
                                inner_start as u32,
                                inner_end as u32,
                            ),
                        }],
                    ));
                }
                advance_char(input);
            }
            _ => advance_char(input),
        }
    }
    // Unterminated: rewind to just past the open brace so the outer
    // loop's resync can find the next statement boundary instead of
    // being stuck at EOF.
    input.reset(&open_cp);
    advance_char(input);
    Err(InnerError {
        message: "unterminated brace group".into(),
        span: Span::new(open as u32, (open + 1) as u32),
    })
}

fn parse_quoted_word(
    input: &mut Input<'_>,
    source: &str,
) -> Result<(WordForm, Vec<WordPart>), InnerError> {
    let open = input.location();
    advance_char(input); // "
    let parts = collect_parts(input, source, Some('"'))?;
    if at_eof(input, source) || current_char(input, source) != '"' {
        return Err(InnerError {
            message: "unterminated string".into(),
            span: Span::new(open as u32, input.location() as u32),
        });
    }
    advance_char(input); // closing "
    Ok((WordForm::Quoted, parts))
}

fn parse_bare_word(
    input: &mut Input<'_>,
    source: &str,
) -> Result<(WordForm, Vec<WordPart>), InnerError> {
    let start = input.location();
    let parts = collect_parts(input, source, None)?;
    if parts.is_empty() {
        return Err(InnerError {
            message: "expected word".into(),
            span: Span::new(start as u32, input.location() as u32),
        });
    }
    Ok((WordForm::Bare, parts))
}

/// Accumulate [`WordPart`]s.
///
/// `terminator` controls the stop condition: `Some('"')` for
/// double-quoted words (stops at the closing quote, newlines are
/// content), `None` for bare words (stops at whitespace, `;`, `\n`,
/// EOF).
fn collect_parts(
    input: &mut Input<'_>,
    source: &str,
    terminator: Option<char>,
) -> Result<Vec<WordPart>, InnerError> {
    let mut parts = Vec::new();
    let mut text_buf = String::new();
    let mut text_start: Option<u32> = None;

    let flush = |parts: &mut Vec<WordPart>,
                 buf: &mut String,
                 start: &mut Option<u32>,
                 end: u32| {
        if let Some(s) = start.take() {
            if !buf.is_empty() {
                parts.push(WordPart::Text {
                    value: std::mem::take(buf),
                    span: Span::new(s, end),
                });
            }
            buf.clear();
        }
    };

    loop {
        if at_eof(input, source) {
            break;
        }
        let c = current_char(input, source);
        if Some(c) == terminator {
            break;
        }
        if terminator.is_none() {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' => break,
                _ => {}
            }
        }
        match c {
            '$' => {
                flush(
                    &mut parts,
                    &mut text_buf,
                    &mut text_start,
                    input.location() as u32,
                );
                parts.push(parse_var_ref(input, source)?);
            }
            '[' => {
                flush(
                    &mut parts,
                    &mut text_buf,
                    &mut text_start,
                    input.location() as u32,
                );
                parts.push(parse_cmd_subst(input, source)?);
            }
            '\\' => {
                flush(
                    &mut parts,
                    &mut text_buf,
                    &mut text_start,
                    input.location() as u32,
                );
                parts.push(parse_escape(input, source)?);
            }
            _ => {
                if text_start.is_none() {
                    text_start = Some(input.location() as u32);
                }
                text_buf.push(c);
                advance_char(input);
            }
        }
    }
    flush(
        &mut parts,
        &mut text_buf,
        &mut text_start,
        input.location() as u32,
    );
    Ok(parts)
}

fn parse_var_ref(
    input: &mut Input<'_>,
    source: &str,
) -> Result<WordPart, InnerError> {
    let start = input.location();
    advance_char(input); // $
    if at_eof(input, source) {
        return Err(InnerError {
            message: "expected variable name after `$`".into(),
            span: Span::new(start as u32, input.location() as u32),
        });
    }
    let mut name = String::new();
    if current_char(input, source) == '{' {
        advance_char(input);
        while !at_eof(input, source) {
            let c = current_char(input, source);
            if c == '}' {
                advance_char(input);
                return Ok(WordPart::VarRef {
                    name,
                    span: Span::new(start as u32, input.location() as u32),
                });
            }
            name.push(c);
            advance_char(input);
        }
        return Err(InnerError {
            message: "unterminated `${...}`".into(),
            span: Span::new(start as u32, input.location() as u32),
        });
    }
    while !at_eof(input, source) {
        let c = current_char(input, source);
        if c.is_alphanumeric() || c == '_' || c == ':' {
            name.push(c);
            advance_char(input);
        } else {
            break;
        }
    }
    Ok(WordPart::VarRef {
        name,
        span: Span::new(start as u32, input.location() as u32),
    })
}

fn parse_cmd_subst(
    input: &mut Input<'_>,
    source: &str,
) -> Result<WordPart, InnerError> {
    let start = input.location();
    advance_char(input); // [
    let inner_start = input.location();
    let mut depth = 1usize;
    while !at_eof(input, source) {
        let c = current_char(input, source);
        match c {
            '\\' => {
                advance_char(input);
                if !at_eof(input, source) {
                    advance_char(input);
                }
            }
            '[' => {
                depth += 1;
                advance_char(input);
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    let inner_end = input.location();
                    advance_char(input);
                    let span = Span::new(start as u32, input.location() as u32);
                    let text = source[inner_start..inner_end].to_string();
                    return Ok(WordPart::CmdSubst {
                        source: text,
                        span,
                        body: Vec::new(),
                    });
                }
                advance_char(input);
            }
            _ => advance_char(input),
        }
    }
    Err(InnerError {
        message: "unterminated `[...]` command substitution".into(),
        span: Span::new(start as u32, input.location() as u32),
    })
}

fn parse_escape(
    input: &mut Input<'_>,
    source: &str,
) -> Result<WordPart, InnerError> {
    let start = input.location();
    advance_char(input); // backslash
    if at_eof(input, source) {
        return Err(InnerError {
            message: "trailing `\\` at end of input".into(),
            span: Span::new(start as u32, input.location() as u32),
        });
    }
    let c = current_char(input, source);
    advance_char(input);
    let value = match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '\\' => '\\',
        '"' => '"',
        '[' => '[',
        ']' => ']',
        '{' => '{',
        '}' => '}',
        '$' => '$',
        other => other,
    };
    Ok(WordPart::Escape {
        value,
        span: Span::new(start as u32, input.location() as u32),
    })
}

fn skip_inline_ws(input: &mut Input<'_>, source: &str, mode: Mode) {
    while !at_eof(input, source) {
        let c = current_char(input, source);
        if c == ' ' || c == '\t' || c == '\r' {
            advance_char(input);
        } else if mode == Mode::BracketBody && c == '\n' {
            // Inside `[ … ]` the newline isn't a statement terminator;
            // it's just whitespace.
            advance_char(input);
        } else if c == '\\' {
            let pos = input.location();
            if pos + 1 < source.len() && source.as_bytes()[pos + 1] == b'\n' {
                advance_char(input);
                advance_char(input);
            } else {
                break;
            }
        } else {
            break;
        }
    }
}

fn at_eof(input: &Input<'_>, source: &str) -> bool {
    input.location() >= source.len()
}

fn current_char(input: &Input<'_>, source: &str) -> char {
    source[input.location()..].chars().next().unwrap_or('\0')
}

fn advance_char(input: &mut Input<'_>) {
    let _ = input.next_token();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        let out = parse("");
        assert!(out.document.stmts.is_empty());
        assert!(out.errors.is_empty());
    }

    #[test]
    fn parse_comment_and_doc() {
        let src = "# regular\n## doc text\nputs hi\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.document.stmts.len(), 3);
        let Stmt::Command(cmd) = &out.document.stmts[2] else {
            panic!("expected command, got {:?}", out.document.stmts[2]);
        };
        assert_eq!(cmd.doc_comments, vec!["doc text".to_string()]);
        assert_eq!(cmd.words[0].as_text(), Some("puts"));
        assert_eq!(cmd.words[1].as_text(), Some("hi"));
    }

    #[test]
    fn parse_set_command() {
        let out = parse("set x 42");
        assert!(out.errors.is_empty());
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        assert!(matches!(cmd.kind, CommandKind::Set));
        assert_eq!(cmd.words.len(), 3);
    }

    #[test]
    fn parse_proc_braced() {
        let src = "proc greet {name} { puts \"hi $name\" }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!("expected proc, got {:?}", cmd.kind);
        };
        assert_eq!(proc.name.as_deref(), Some("greet"));
        let args = proc.args_span.slice(src);
        let body = proc.body_span.slice(src);
        assert_eq!(args, "name");
        assert!(body.contains("puts"));
    }

    #[test]
    fn parse_variable_and_subst() {
        let src = "puts $x [foo bar]";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        assert_eq!(cmd.words.len(), 3);
        let WordPart::VarRef { name, .. } = &cmd.words[1].parts[0] else {
            panic!("expected var ref");
        };
        assert_eq!(name, "x");
        let WordPart::CmdSubst { source: src, .. } = &cmd.words[2].parts[0]
        else {
            panic!("expected cmd subst");
        };
        assert_eq!(src, "foo bar");
    }

    #[test]
    fn parse_quoted_with_subst() {
        let src = r#"puts "hello $name""#;
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        assert_eq!(cmd.words[1].form, WordForm::Quoted);
        assert_eq!(cmd.words[1].parts.len(), 2);
        let WordPart::Text { value, .. } = &cmd.words[1].parts[0] else {
            panic!();
        };
        assert_eq!(value, "hello ");
        let WordPart::VarRef { name, .. } = &cmd.words[1].parts[1] else {
            panic!();
        };
        assert_eq!(name, "name");
    }

    #[test]
    fn recovers_from_unterminated_brace() {
        let src = "puts {oops\nputs ok\n";
        let out = parse(src);
        assert!(!out.errors.is_empty());
        assert!(out.errors[0].message.contains("brace group"));
        // After the error we should still see the second `puts ok`.
        let ok_cmd = out.document.stmts.iter().find_map(|s| match s {
            Stmt::Command(c)
                if c.words.first().and_then(|w| w.as_text())
                    == Some("puts")
                    && c.words.get(1).and_then(|w| w.as_text())
                        == Some("ok") =>
            {
                Some(c)
            }
            _ => None,
        });
        assert!(
            ok_cmd.is_some(),
            "expected recovery: {:?}",
            out.document.stmts
        );
    }

    #[test]
    fn proc_body_parses_into_statements_with_absolute_spans() {
        let src = "proc outer {\n  a\n} {\n  inner_call foo\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!("expected proc");
        };
        assert_eq!(proc.body.len(), 1, "{:?}", proc.body);
        let Stmt::Command(body_cmd) = &proc.body[0] else {
            panic!("expected command in body");
        };
        // Span is absolute: it slices back to the original source.
        assert_eq!(body_cmd.words[0].span.slice(src), "inner_call");
        assert_eq!(
            body_cmd.span.start as usize,
            src.find("inner_call").unwrap()
        );
    }

    #[test]
    fn nested_proc_body_is_parsed_recursively() {
        let src =
            "proc outer {\n  a\n} {\n  proc inner {\n  b\n} {\n  deep\n}\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Proc(outer) = &cmd.kind else {
            panic!("expected outer proc");
        };
        let Stmt::Command(inner_cmd) = &outer.body[0] else {
            panic!("expected inner proc command");
        };
        let CommandKind::Proc(inner) = &inner_cmd.kind else {
            panic!("expected inner proc");
        };
        assert_eq!(inner.name.as_deref(), Some("inner"));
        // Inner proc got its signature and body populated too.
        assert!(inner.signature.is_some());
        let Stmt::Command(deep) = &inner.body[0] else {
            panic!("expected deep command");
        };
        assert_eq!(deep.words[0].span.slice(src), "deep");
    }

    #[test]
    fn parses_src_statement() {
        let src = "src common/log\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Src(import) = &cmd.kind else {
            panic!("expected Src, got {:?}", cmd.kind);
        };
        assert_eq!(import.path.as_deref(), Some("common/log"));
        assert_eq!(import.path_span.slice(src), "common/log");
    }

    #[test]
    fn parses_src_with_named_dep_prefix() {
        let out = parse("src @xilinx-ip/cpm5\n");
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Src(import) = &cmd.kind else {
            panic!("expected Src");
        };
        assert_eq!(import.path.as_deref(), Some("@xilinx-ip/cpm5"));
    }

    #[test]
    fn src_with_extra_words_is_generic() {
        // `src a b` isn't a valid import — it falls back to generic so
        // the validator can report it as an unknown command rather than
        // the parser silently accepting it.
        let out = parse("src a b\n");
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        assert!(matches!(cmd.kind, CommandKind::Generic), "{:?}", cmd.kind);
    }

    #[test]
    fn bracket_body_treats_newlines_as_whitespace() {
        // Multi-line call inside `[ … ]` parses as a *single* command,
        // no backslash continuations needed.
        let src = "\
set cell [
  create_cpm5_cpm_pcie0
    -cell cpm5
    -max_link_speed 32.0_GT/s
]
";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(set_cmd) = &out.document.stmts[0] else {
            panic!();
        };
        assert!(matches!(set_cmd.kind, CommandKind::Set));
        // The `set`'s value word is the cmd-subst; its body should be
        // a single command with five words.
        let WordPart::CmdSubst { body, .. } = &set_cmd.words[2].parts[0] else {
            panic!("expected CmdSubst");
        };
        assert_eq!(body.len(), 1, "{body:#?}");
        let Stmt::Command(inner) = &body[0] else {
            panic!();
        };
        let word_texts: Vec<&str> =
            inner.words.iter().filter_map(|w| w.as_text()).collect();
        assert_eq!(
            word_texts,
            vec![
                "create_cpm5_cpm_pcie0",
                "-cell",
                "cpm5",
                "-max_link_speed",
                "32.0_GT/s",
            ]
        );
    }

    #[test]
    fn bracket_body_still_separates_on_semicolon() {
        // Explicit `;` keeps the multi-command form available inside
        // brackets for users who want it.
        let src = "set x [a 1 ; b 2]\n";
        let out = parse(src);
        assert!(out.errors.is_empty());
        let Stmt::Command(set_cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let WordPart::CmdSubst { body, .. } = &set_cmd.words[2].parts[0] else {
            panic!();
        };
        assert_eq!(body.len(), 2, "{body:#?}");
    }

    #[test]
    fn toplevel_newlines_still_terminate() {
        // The bracket-body relaxation does not leak to the top level.
        let src = "puts a\nputs b\n";
        let out = parse(src);
        let cmds: Vec<&Command> = out
            .document
            .stmts
            .iter()
            .filter_map(|s| {
                if let Stmt::Command(c) = s {
                    Some(c)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn proc_body_newlines_still_terminate() {
        // Proc bodies are scripts; the relaxation is bracket-only.
        let src = "proc f {} {\n  puts a\n  puts b\n}\n";
        let out = parse(src);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!();
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!();
        };
        assert_eq!(proc.body.len(), 2);
    }

    #[test]
    fn semicolon_separates_commands() {
        let src = "set a 1; set b 2";
        let out = parse(src);
        assert!(out.errors.is_empty());
        let cmds: Vec<&Command> = out
            .document
            .stmts
            .iter()
            .filter_map(|s| {
                if let Stmt::Command(c) = s {
                    Some(c)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(cmds.len(), 2);
    }
}
