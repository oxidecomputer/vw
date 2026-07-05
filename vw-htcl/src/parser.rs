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
pub(crate) enum Mode {
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
pub(crate) fn populate_procs(
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

                // Parse the return-type annotation if the outer
                // parse recorded one. We have `source` and the
                // error sink here, so this is the right place to
                // do it — `classify_command` only recorded the
                // (inner, brace-stripped) span.
                if let Some(rt_span) = proc.return_type_span {
                    let text = rt_span.slice(source);
                    match crate::type_parse::parse(text, rt_span.start) {
                        Ok(ty) => proc.return_type = Some(ty),
                        Err(e) => errors.push(ParseError {
                            message: e.message,
                            span: e.span,
                        }),
                    }
                }
                // Mirror the return type onto the signature so the
                // signature-table-based lookup paths (REPL repr
                // formatter, hover) see it without re-walking
                // back to the Proc node.
                if let Some(sig) = proc.signature.as_mut() {
                    sig.return_type = proc.return_type.clone();
                }

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
            CommandKind::TypeDecl(td) => {
                let text = td.underlying_span.slice(source);
                match crate::type_parse::parse(text, td.underlying_span.start) {
                    Ok(ty) => td.underlying = Some(ty),
                    Err(e) => errors.push(ParseError {
                        message: e.message,
                        span: e.span,
                    }),
                }
            }
            CommandKind::EnumDecl(ed) => {
                let text = ed.body_span.slice(source);
                match crate::enum_parse::parse(text, ed.body_span.start) {
                    Ok(vs) => ed.variants = vs,
                    Err(e) => errors.push(ParseError {
                        message: e.message,
                        span: e.span,
                    }),
                }
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
pub(crate) fn parse_fragment(
    text: &str,
    mode: Mode,
) -> (Vec<crate::ast::Stmt>, Vec<ParseError>) {
    let mut input = LocatingSlice::new(text);
    let mut errors = Vec::new();
    let document = parse_document(&mut input, text, &mut errors, mode);
    (document.stmts, errors)
}

pub(crate) fn shift_stmt(stmt: &mut crate::ast::Stmt, delta: u32) {
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
            if let Some(ref mut s) = proc.return_type_span {
                *s = s.shifted(delta);
            }
            // `return_type` is None at this point (parsed later in
            // `populate_procs` using the now-absolute span), so
            // there's nothing to shift inside it.
        }
        CommandKind::NamespaceEval(ns) => {
            ns.name_span = ns.name_span.shifted(delta);
            ns.body_span = ns.body_span.shifted(delta);
        }
        CommandKind::TypeDecl(td) => {
            td.name_span = td.name_span.shifted(delta);
            td.underlying_span = td.underlying_span.shifted(delta);
        }
        CommandKind::EnumDecl(ed) => {
            ed.name_span = ed.name_span.shifted(delta);
            ed.body_span = ed.body_span.shifted(delta);
            // Variants are filled later by `populate_procs` using
            // the now-absolute body_span, so there's nothing to
            // shift inside them yet.
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
    // Span covering all currently-pending `##` lines from the first
    // `#` byte to the last line's end. Grows with each new `##`
    // encountered; cleared alongside `pending_docs`. Used to seed
    // the attached command's `doc_comments_span` so the analyzer
    // can answer "is the cursor inside this doc block?"
    let mut pending_docs_span: Option<Span> = None;

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
                    // Extend the block-span to cover this line. The
                    // comment's own span starts at its `#` and ends
                    // at the line's end.
                    pending_docs_span = Some(match pending_docs_span {
                        Some(prev) => Span::new(prev.start, comment.span.end),
                        None => comment.span,
                    });
                } else {
                    pending_docs.clear();
                    pending_docs_span = None;
                }
                stmts.push(Stmt::Comment(comment));
            }
            _ => {
                let cmd_start = input.location();
                match parse_command(input, source, mode) {
                    Ok(mut cmd) => {
                        cmd.doc_comments = std::mem::take(&mut pending_docs);
                        cmd.doc_comments_span = pending_docs_span.take();
                        stmts.push(Stmt::Command(cmd));
                    }
                    Err(err) => {
                        pending_docs.clear();
                        pending_docs_span = None;
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
        // Line-continuation on a leading-dash next line: the
        // configurator shape `cmd\n  -flag val\n  -flag val\n`
        // parses as one command without needing `\` at every EOL.
        // Only triggers mid-command (`!words.is_empty()`) — a `-`
        // at the start of a fresh statement stays a new statement,
        // even if it doesn't lex as a command name.
        if mode == Mode::Toplevel
            && c == '\n'
            && !words.is_empty()
            && next_line_is_flag_continuation(input, source)
        {
            advance_char(input);
            continue;
        }
        let terminate = match mode {
            Mode::Toplevel => c == '\n' || c == ';',
            // In bracket-body, only `;` terminates a command — `\n`
            // is whitespace consumed by `skip_inline_ws`.
            Mode::BracketBody => c == ';',
        };
        if terminate {
            break;
        }
        // Inline comment at word-start position (mid-command). The
        // configurator idiom for commenting out an arg line —
        //
        //   set cfg [
        //     versal_cips::configure
        //       -enable_reg_interface 1
        //       #-intf_parent_pin_list 0
        //   ]
        //
        // — needs the parser to eat the `#-intf_parent_pin_list 0`
        // as a comment, otherwise it lands as a word and the
        // analyzer flags `expected keyword argument`. Only fires
        // MID-command (`!words.is_empty()`) so `#` at
        // command-start on the top-level (a real Tcl comment) still
        // reaches the outer `parse_document` handler; inside
        // brackets `words.is_empty()` at line-start is normal
        // because bracket-body has no prior context, but the
        // enclosing `[…]` was already parsed as a CmdSubst so any
        // `#` INSIDE the subst body's first command *does* have
        // words already (the command name).
        if c == '#' && !words.is_empty() {
            skip_to_end_of_line(input, source);
            continue;
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
        doc_comments_span: None,
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
            // 5 words = return-type slot present:
            //   proc NAME { args } TYPE { body }
            // 4 words = no return type:
            //   proc NAME { args } { body }
            // (>5 words is treated as 5+ junk; the body is taken
            //  from words[4] and the rest is silently ignored.
            //  The return-type slot is parsed in `populate_procs`
            //  where we have `source` and an error sink — we only
            //  record the span here.)
            let (return_type_span, body_word) = if words.len() >= 5 {
                (Some(inner_text_span(&words[3])), &words[4])
            } else {
                (None, &words[3])
            };
            let name = name_word.as_text().map(|s| s.to_string());
            CommandKind::Proc(Proc {
                name,
                name_span: name_word.span,
                args_span: inner_text_span(args_word),
                body_span: inner_text_span(body_word),
                signature: None,
                return_type: None,
                return_type_span,
                body: Vec::new(),
            })
        }
        // `type NAME = UNDERLYING` newtype declaration. The `=` may
        // be its own word (`type T = U`) or fused (`type T=U`) —
        // Tcl word splitting is whitespace-driven, so we accept
        // either by checking the third word. The underlying type
        // is parsed in `populate_procs`'s second pass (same
        // rationale as `proc`'s return type).
        Some("type") if words.len() >= 3 => {
            let name_word = &words[1];
            let underlying_word =
                if words.len() >= 4 && words[2].as_text() == Some("=") {
                    &words[3]
                } else {
                    &words[2]
                };
            CommandKind::TypeDecl(crate::ast::TypeDecl {
                name: name_word.as_text().map(String::from),
                name_span: name_word.span,
                underlying: None,
                underlying_span: inner_text_span(underlying_word),
            })
        }
        // `enum NAME = { …variants… }` sum-type declaration.
        // Same `=`-may-or-may-not-be-its-own-word convention as
        // `type`. The body word is brace-wrapped; its contents
        // (the variant list) are parsed in `populate_procs`'s
        // second pass, when we have the source + error sink.
        Some("enum") if words.len() >= 3 => {
            let name_word = &words[1];
            let body_word =
                if words.len() >= 4 && words[2].as_text() == Some("=") {
                    &words[3]
                } else {
                    &words[2]
                };
            CommandKind::EnumDecl(crate::ast::EnumDecl {
                name: name_word.as_text().map(String::from),
                name_span: name_word.span,
                variants: Vec::new(),
                body_span: inner_text_span(body_word),
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

/// Peek past a `\n` and any inline whitespace on the immediately-
/// following line: does that line's first non-whitespace byte
/// begin a flag-shaped token (`-` followed by a letter, digit,
/// or another `-`)? Called by [`parse_command`] to decide whether
/// a newline should be treated as command continuation.
///
/// Deliberately does NOT peek across a second `\n` — a blank line
/// terminates the continuation. Callers rely on this to model
/// "paragraph breaks" naturally, matching a reader's intuition.
///
/// Doesn't consume input; only inspects `source` bytes.
fn next_line_is_flag_continuation(input: &Input<'_>, source: &str) -> bool {
    let bytes = source.as_bytes();
    // Cursor sits on `\n`; look ahead starting at the byte after.
    let mut i = input.location() + 1;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            _ => break,
        }
    }
    if i >= bytes.len() || bytes[i] != b'-' {
        return false;
    }
    // Look at what follows the `-`. Flag-shaped: letter, digit,
    // underscore, or a second `-` (for `--end-of-options` idiom).
    // Anything else (whitespace, EOF, punctuation) declines to
    // continue. Underscore is included because real Vivado flag
    // names like `-_64bit` (create_bd_cell's 64-bit BAR flag) are
    // valid — without `_` in the set, the continuation rule
    // breaks on them and Tcl tries to execute `-_64bit` as a
    // command.
    let next = bytes.get(i + 1).copied().unwrap_or(b'\0');
    next.is_ascii_alphanumeric() || next == b'-' || next == b'_'
}

/// Consume input up to the next `\n` (not consuming the `\n`
/// itself). Used to treat `#`-prefixed lines mid-command as
/// inline comments — see the callsite in `parse_command`.
fn skip_to_end_of_line(input: &mut Input<'_>, source: &str) {
    while !at_eof(input, source) {
        if current_char(input, source) == '\n' {
            break;
        }
        advance_char(input);
    }
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
        // No return type slot.
        assert!(proc.return_type.is_none());
        assert!(proc.return_type_span.is_none());
    }

    #[test]
    fn parse_proc_with_return_type_named() {
        let src = "proc f {} string { return foo }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!()
        };
        assert_eq!(proc.name.as_deref(), Some("f"));
        let body = proc.body_span.slice(src);
        assert!(body.contains("return foo"));
        let ty = proc.return_type.as_ref().expect("return type set");
        assert_eq!(ty.name(), "string");
        match ty {
            crate::ast::TypeExpr::Named { .. } => {}
            _ => panic!("expected Named"),
        }
    }

    #[test]
    fn parse_proc_with_return_type_generic_no_whitespace() {
        let src = "proc f {} list<bd_cell> { return {} }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!()
        };
        let ty = proc.return_type.as_ref().unwrap();
        let crate::ast::TypeExpr::Generic { name, args, .. } = ty else {
            panic!("expected Generic")
        };
        assert_eq!(name, "list");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].name(), "bd_cell");
    }

    #[test]
    fn parse_proc_with_return_type_nested_generic() {
        let src = "proc f {} list<dict<string,bd_cell>> { return {} }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!()
        };
        let ty = proc.return_type.as_ref().unwrap();
        let crate::ast::TypeExpr::Generic { args, .. } = ty else {
            panic!()
        };
        let crate::ast::TypeExpr::Generic {
            name: inner_name,
            args: inner_args,
            ..
        } = &args[0]
        else {
            panic!("expected nested Generic")
        };
        assert_eq!(inner_name, "dict");
        assert_eq!(inner_args.len(), 2);
    }

    #[test]
    fn parse_proc_with_return_type_bracketed_whitespace() {
        let src = "proc f {} {dict<string, int>} { return {} }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!()
        };
        let ty = proc.return_type.as_ref().unwrap();
        let crate::ast::TypeExpr::Generic { name, args, .. } = ty else {
            panic!()
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name(), "string");
        assert_eq!(args[1].name(), "int");
    }

    #[test]
    fn parse_proc_with_invalid_return_type_emits_diagnostic() {
        let src = "proc f {} list< { return {} }\n";
        let out = parse(src);
        assert!(
            !out.errors.is_empty(),
            "expected a parse-error diagnostic for bad type"
        );
        assert!(out.errors.iter().any(|e| e.message.contains("expected")
            || e.message.contains("unterminated")));
    }

    #[test]
    fn parse_type_decl_named() {
        let src = "type bd_cell = string\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::TypeDecl(td) = &cmd.kind else {
            panic!("expected TypeDecl, got {:?}", cmd.kind)
        };
        assert_eq!(td.name.as_deref(), Some("bd_cell"));
        let underlying = td.underlying.as_ref().unwrap();
        assert_eq!(underlying.name(), "string");
    }

    #[test]
    fn parse_type_decl_generic_underlying() {
        let src = "type fancy_dict = {dict<string, int>}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::TypeDecl(td) = &cmd.kind else {
            panic!()
        };
        let crate::ast::TypeExpr::Generic { name, args, .. } =
            td.underlying.as_ref().unwrap()
        else {
            panic!()
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn parse_type_decl_without_equals_works() {
        // `type T U` (no `=`) is also accepted — the `=` is sugar.
        let src = "type widget string\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::TypeDecl(td) = &cmd.kind else {
            panic!()
        };
        assert_eq!(td.name.as_deref(), Some("widget"));
        assert_eq!(td.underlying.as_ref().unwrap().name(), "string");
    }

    #[test]
    fn parse_type_decl_with_bad_underlying_emits_diagnostic() {
        let src = "type foo = <bad>\n";
        let out = parse(src);
        assert!(
            !out.errors.is_empty(),
            "expected diagnostic for malformed underlying type"
        );
    }

    // --- enum declarations -----------------------------------------

    #[test]
    fn parse_enum_decl_simple() {
        let src = "enum Direction = {\n  North\n  South\n  East\n  West\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::EnumDecl(ed) = &cmd.kind else {
            panic!("expected EnumDecl, got {:?}", cmd.kind);
        };
        assert_eq!(ed.name.as_deref(), Some("Direction"));
        assert_eq!(ed.variants.len(), 4);
        for v in &ed.variants {
            assert!(v.payload.is_none(), "expected empty-payload variant");
        }
        let names: Vec<&str> =
            ed.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["North", "South", "East", "West"]);
    }

    #[test]
    fn parse_enum_decl_with_payloads() {
        let src = "enum Property = {\n  Scalar: string\n  Nested: dict<string,Property>\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::EnumDecl(ed) = &cmd.kind else {
            panic!()
        };
        assert_eq!(ed.name.as_deref(), Some("Property"));
        assert_eq!(ed.variants.len(), 2);
        assert_eq!(ed.variants[0].name, "Scalar");
        assert_eq!(ed.variants[0].payload.as_ref().unwrap().name(), "string");
        assert_eq!(ed.variants[1].name, "Nested");
        let crate::ast::TypeExpr::Generic { name, args, .. } =
            ed.variants[1].payload.as_ref().unwrap()
        else {
            panic!();
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name(), "string");
        assert_eq!(args[1].name(), "Property");
    }

    #[test]
    fn parse_enum_decl_mixed_payload_and_empty() {
        let src =
            "enum Mix = {\n  Empty\n  WithInt: int\n  Other\n  WithList: list<bd_cell>\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::EnumDecl(ed) = &cmd.kind else {
            panic!()
        };
        assert_eq!(ed.variants.len(), 4);
        assert!(ed.variants[0].payload.is_none());
        assert_eq!(ed.variants[1].payload.as_ref().unwrap().name(), "int");
        assert!(ed.variants[2].payload.is_none());
        assert_eq!(ed.variants[3].payload.as_ref().unwrap().name(), "list");
    }

    #[test]
    fn parse_enum_decl_without_equals() {
        let src = "enum Color {\n  Red\n  Green\n  Blue\n}\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::EnumDecl(ed) = &cmd.kind else {
            panic!()
        };
        assert_eq!(ed.name.as_deref(), Some("Color"));
        assert_eq!(ed.variants.len(), 3);
    }

    #[test]
    fn parse_enum_decl_with_bad_variant_emits_diagnostic() {
        // 123Foo is not a valid identifier — should diagnose.
        let src = "enum Bad = {\n  123Foo: int\n}\n";
        let out = parse(src);
        assert!(
            !out.errors.is_empty(),
            "expected diagnostic for malformed variant"
        );
    }

    #[test]
    fn parse_proc_with_qualified_arg_type() {
        // The `E::V` qualified syntax for overloaded handler args.
        let src =
            "proc handle_prop {v: Property::Scalar} string { return $v }\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!()
        };
        let CommandKind::Proc(proc) = &cmd.kind else {
            panic!()
        };
        let sig = proc.signature.as_ref().unwrap();
        assert_eq!(sig.args.len(), 1);
        let arg = &sig.args[0];
        assert_eq!(arg.name, "v");
        let crate::ast::TypeExpr::Qualified {
            namespace, variant, ..
        } = arg.type_annotation.as_ref().unwrap()
        else {
            panic!(
                "expected Qualified type annotation, got {:?}",
                arg.type_annotation
            );
        };
        assert_eq!(namespace, "Property");
        assert_eq!(variant, "Scalar");
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

    /// Line continuation via a leading `-` on the next line: the
    /// common flag-per-line configurator shape (`create_foo`,
    /// newline, indented `-bar val`, newline, `-baz val`, …)
    /// parses as one command without needing a trailing `\`.
    #[test]
    fn dash_leading_next_line_continues_command() {
        let src = "create_foo\n  -bar 1\n  -baz 2\n";
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
        assert_eq!(cmds.len(), 1, "{cmds:#?}");
        let words: Vec<&str> =
            cmds[0].words.iter().filter_map(Word::as_text).collect();
        assert_eq!(words, ["create_foo", "-bar", "1", "-baz", "2"]);
    }

    /// A `--` (end-of-options) continuation also chains — same
    /// leading-dash shape.
    #[test]
    fn double_dash_next_line_continues_command() {
        let src = "cmd -a 1\n  -- rest\n";
        let out = parse(src);
        let Stmt::Command(cmd) = &out.document.stmts[0] else {
            panic!("{:#?}", out.document.stmts);
        };
        assert_eq!(cmd.words.len(), 5);
    }

    /// Non-dash next line still terminates. A regression here
    /// would break every existing top-level script.
    #[test]
    fn non_dash_next_line_terminates_as_before() {
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

    /// A blank line between the header and a dash-led line breaks
    /// the continuation — the header stands alone and the dash-
    /// led line becomes a new (probably weird) command. This
    /// matches how a reader intuits paragraph breaks: an empty
    /// line is a stronger separator than a newline.
    /// Vivado flags like `-_64bit` start with `-_` — the underscore
    /// must be recognized as flag-shaped so the continuation rule
    /// keeps the line inside the enclosing command instead of
    /// starting a new (invalid) command.
    #[test]
    fn dash_underscore_line_continues_command() {
        let src = "create_bar\n  -cell foo\n  -_64bit 1\n";
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
        assert_eq!(cmds.len(), 1, "{cmds:#?}");
        let words: Vec<&str> =
            cmds[0].words.iter().filter_map(Word::as_text).collect();
        assert_eq!(words, ["create_bar", "-cell", "foo", "-_64bit", "1"]);
    }

    /// Trailing whitespace on the previous line must not defeat the
    /// dash-continuation rule — real-world files often carry a stray
    /// space at end of line, and we want the multi-line command to
    /// still parse as one command.
    #[test]
    fn dash_continuation_survives_trailing_ws_on_previous_line() {
        let src = "cmd\n  -foo 1 \n  -bar 2\n";
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
        assert_eq!(cmds.len(), 1, "{cmds:#?}");
        assert_eq!(cmds[0].words.len(), 5);
    }

    #[test]
    fn blank_line_before_dash_breaks_continuation() {
        let src = "cmd\n\n  -a 1\n";
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
        assert_eq!(cmds.len(), 2, "{cmds:#?}");
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

    #[test]
    fn inline_comment_arg_line_is_stripped() {
        // The configurator idiom for commenting out an arg line.
        // Parser should eat `#-intf_parent_pin_list 0` and leave a
        // clean word list `[configure, -enable_reg_interface, 1]`.
        let src = "\
set cfg [
  configure
    -enable_reg_interface 1
    #-intf_parent_pin_list 0
]\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "parse errors: {:?}", out.errors);
        // The set command has three words: `set`, `cfg`, `[…]`.
        let set = out
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                crate::ast::Stmt::Command(c) if c.words.first().and_then(|w| w.as_text()) == Some("set") => Some(c),
                _ => None,
            })
            .expect("set command");
        // The CmdSubst body should contain one command with 3 words
        // (the `#-intf_parent_pin_list 0` line is eaten).
        let bracket = &set.words[2];
        let crate::ast::WordPart::CmdSubst { body, .. } = &bracket.parts[0]
        else {
            panic!("expected CmdSubst");
        };
        let inner_cmd = body
            .iter()
            .find_map(|s| match s {
                crate::ast::Stmt::Command(c) => Some(c),
                _ => None,
            })
            .expect("configure command");
        let word_texts: Vec<&str> = inner_cmd
            .words
            .iter()
            .filter_map(|w| w.as_text())
            .collect();
        assert_eq!(
            word_texts,
            vec!["configure", "-enable_reg_interface", "1"],
            "expected the commented arg line to be gone",
        );
    }

    #[test]
    fn hash_mid_command_line_is_still_comment() {
        // `#` mid-command at word-start, even without a newline
        // in-between, is treated as a comment. Matches the
        // configurator ergonomics — inside `[cmd -a x #-b y]`
        // the `#-b y` gets eaten.
        let src = "[configure -a x #-b y]\n";
        let out = parse(src);
        assert!(out.errors.is_empty(), "parse errors: {:?}", out.errors);
    }
}
