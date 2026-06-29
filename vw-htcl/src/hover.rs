// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Find the htcl construct at a given byte offset.
//!
//! Used by `vw analyzer` for `textDocument/hover` and (later) by the
//! REPL for inline hover-style popups. Pure analysis — returns a
//! structured [`HoverTarget`] referencing into the document; the
//! caller formats it (markdown for LSP, a ratatui widget for the
//! REPL, etc).

use crate::ast::{
    Command, CommandKind, Document, Proc, ProcArg, ProcSignature, Stmt, Word,
};
use crate::lower::{signature_table, SignatureTable};
use crate::scope::{innermost_scope, resolve_var_def, scan_var_ref, VarDef};
use crate::span::Span;

/// A construct the cursor is on, plus the data needed to render
/// hover content. Lifetime-tied to the [`Document`] passed into
/// [`hover_at`].
#[derive(Clone, Debug)]
pub enum HoverTarget<'a> {
    /// Cursor is on the name of a `proc` declaration. The proc's own
    /// signature contains the docs.
    ProcDef { proc: &'a Proc, span: Span },
    /// Cursor is on the name of an argument inside a `proc`
    /// declaration's args braces.
    ProcArgDef {
        proc_name: String,
        arg: &'a ProcArg,
        span: Span,
    },
    /// Cursor is on the first word of a command that resolves to a
    /// known structured proc — i.e. a call to a documented proc.
    CallSite {
        proc_name: String,
        signature: &'a ProcSignature,
        span: Span,
    },
    /// Cursor is on a `-flag` word in a call to a known proc.
    CallArg {
        proc_name: String,
        arg: &'a ProcArg,
        span: Span,
    },
    /// Cursor is on a `$var` reference that resolves to a local
    /// (`set`/`variable`) rather than a parameter. The span is the
    /// reference itself.
    LocalVar { name: String, span: Span },
    /// Cursor is on the name of an `enum` declaration. Shows the
    /// variants block as a hover popup.
    EnumDef {
        decl: &'a crate::ast::EnumDecl,
        span: Span,
    },
}

impl HoverTarget<'_> {
    pub fn span(&self) -> Span {
        match self {
            HoverTarget::ProcDef { span, .. }
            | HoverTarget::ProcArgDef { span, .. }
            | HoverTarget::CallSite { span, .. }
            | HoverTarget::CallArg { span, .. }
            | HoverTarget::LocalVar { span, .. }
            | HoverTarget::EnumDef { span, .. } => *span,
        }
    }
}

pub fn hover_at<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let table = signature_table(document);
    hover_in_stmts(&document.stmts, &table, offset)
        // Fallback: a `$var` reference — including one buried in opaque
        // text (a command substitution or `if`/`while` condition).
        .or_else(|| hover_scanned_var(document, source, offset))
}

/// Hover for a `$var` reference found by scanning the source. Resolves
/// to a parameter (rendered like an arg) or a local (`set`/`variable`).
fn hover_scanned_var<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let (name, span) = scan_var_ref(source, offset)?;
    let (stmts, enclosing) = innermost_scope(document, offset);
    match resolve_var_def(&name, stmts, enclosing, offset)? {
        VarDef::Param(arg) => Some(HoverTarget::ProcArgDef {
            proc_name: enclosing
                .and_then(|p| p.name.clone())
                .unwrap_or_default(),
            arg,
            // Anchor the hover on the reference, not the declaration.
            span,
        }),
        VarDef::Local(_) => Some(HoverTarget::LocalVar { name, span }),
    }
}

/// Find the hover target at `offset` within `stmts`, descending into
/// proc bodies. The signature table is the document-wide (top-level)
/// one, so a call inside a body still resolves to the proc it names.
fn hover_in_stmts<'a>(
    stmts: &'a [Stmt],
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if let Some(target) = hover_in_command(cmd, table, offset) {
            return Some(target);
        }
    }
    None
}

fn hover_in_command<'a>(
    cmd: &'a Command,
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let primary = match &cmd.kind {
        CommandKind::Proc(proc) => hover_in_proc_decl(proc, offset)
            // Cursor isn't on the proc's name or an arg — look inside
            // the body.
            .or_else(|| hover_in_stmts(&proc.body, table, offset)),
        CommandKind::EnumDecl(decl) => {
            // Cursor on the enum's name → show the variants.
            if decl.name_span.contains(offset) {
                Some(HoverTarget::EnumDef {
                    decl,
                    span: decl.name_span,
                })
            } else {
                None
            }
        }
        _ => hover_in_call(cmd, table, offset),
    };
    primary.or_else(|| hover_in_cmd_substs(&cmd.words, table, offset))
}

/// Descend into any `[ … ]` command substitutions on this command's
/// words so hover works on calls written inline, e.g.
/// `set cell [create_cpm5 -name x]`.
fn hover_in_cmd_substs<'a>(
    words: &'a [Word],
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    for word in words {
        if !word.span.contains(offset) {
            continue;
        }
        for part in &word.parts {
            if let crate::ast::WordPart::CmdSubst { span, body, .. } = part {
                if span.contains(offset) {
                    return hover_in_stmts(body, table, offset);
                }
            }
        }
    }
    None
}

fn hover_in_proc_decl<'a>(
    proc: &'a Proc,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    if proc.name_span.contains(offset) {
        return Some(HoverTarget::ProcDef {
            proc,
            span: proc.name_span,
        });
    }
    if let Some(sig) = proc.signature.as_ref() {
        for arg in &sig.args {
            if arg.name_span.contains(offset) {
                let proc_name = proc.name.clone().unwrap_or_default();
                return Some(HoverTarget::ProcArgDef {
                    proc_name,
                    arg,
                    span: arg.name_span,
                });
            }
        }
    }
    None
}

fn hover_in_call<'a>(
    cmd: &'a Command,
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let first = cmd.words.first()?;
    let name = first.as_text()?;
    let sig = *table.get(name)?;

    if first.span.contains(offset) {
        return Some(HoverTarget::CallSite {
            proc_name: name.to_string(),
            signature: sig,
            span: first.span,
        });
    }

    // Walk remaining words looking for the `-flag` under the cursor.
    // Value words (the token after a flag) don't trigger hover —
    // they could be anything from a literal to a [cmd subst], and
    // there's no general definition to point at.
    for word in cmd.words.iter().skip(1) {
        if !word.span.contains(offset) {
            continue;
        }
        let text = word.as_text()?;
        let flag = text.strip_prefix('-')?;
        let arg = sig.find(flag)?;
        return Some(HoverTarget::CallArg {
            proc_name: name.to_string(),
            arg,
            span: word.span,
        });
    }
    None
}

// Helpers retained for symmetric use from formatters that want to
// pretty-print attributes etc. without re-walking from raw AST.
#[allow(dead_code)]
fn _word_text(word: &Word) -> Option<&str> {
    word.as_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn at(src: &str, needle: &str, occurrence: usize) -> u32 {
        let mut start = 0;
        for i in 0..=occurrence {
            let pos = src[start..]
                .find(needle)
                .map(|p| start + p)
                .expect("needle not found");
            if i == occurrence {
                return pos as u32;
            }
            start = pos + needle.len();
        }
        unreachable!()
    }

    fn first(src: &str, needle: &str) -> u32 {
        at(src, needle, 0)
    }

    #[test]
    fn hover_on_call_name() {
        let src = "\
proc greet {\n  @default(\"world\") name\n} { puts $name }\n\
greet -name there\n";
        let parsed = parse(src);
        let target =
            hover_at(&parsed.document, src, first(src, "greet -")).unwrap();
        match target {
            HoverTarget::CallSite { proc_name, .. } => {
                assert_eq!(proc_name, "greet");
            }
            other => panic!("expected CallSite, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_call_arg_flag() {
        let src = "\
proc greet {\n  @default(\"world\") name\n} { puts $name }\n\
greet -name there\n";
        let parsed = parse(src);
        let pos = first(src, "-name there");
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::CallArg { arg, proc_name, .. } => {
                assert_eq!(proc_name, "greet");
                assert_eq!(arg.name, "name");
            }
            other => panic!("expected CallArg, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_value_word_returns_none() {
        let src = "\
proc greet {\n  @default(\"world\") name\n} { puts $name }\n\
greet -name there\n";
        let parsed = parse(src);
        let pos = first(src, "there");
        assert!(hover_at(&parsed.document, src, pos).is_none());
    }

    #[test]
    fn hover_on_proc_decl_name() {
        let src = "proc greet {\n  name\n} { puts $name }\n";
        let parsed = parse(src);
        let pos = first(src, "greet");
        let target = hover_at(&parsed.document, src, pos).unwrap();
        assert!(matches!(target, HoverTarget::ProcDef { .. }));
    }

    #[test]
    fn hover_on_proc_arg_decl() {
        let src = "proc greet {\n  @default(\"x\") name\n} { puts hi }\n";
        let parsed = parse(src);
        let pos = first(src, "name"); // first "name" is in args
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::ProcArgDef { arg, .. } => {
                assert_eq!(arg.name, "name");
                assert_eq!(arg.attributes[0].name, "default");
            }
            other => panic!("expected ProcArgDef, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_call_inside_proc_body() {
        // A call to a documented proc from within another proc's body
        // should hover, just like a top-level call.
        let src = "\
proc if_tport {\n  type\n  name\n} { }\n\
proc axis {\n  width\n} {\n  if_tport\n}\n";
        let parsed = parse(src);
        let pos = at(src, "if_tport", 1);
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::CallSite { proc_name, .. } => {
                assert_eq!(proc_name, "if_tport");
            }
            other => panic!("expected CallSite, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_call_inside_command_substitution() {
        // The interior of `[ … ]` is now parsed; hover on the inner
        // call's name should report the proc the same way it does at
        // the top level.
        let src = "\
proc create_cpm5 {\n  @default(0) name\n} { puts hi }\n\
set cell [create_cpm5 -name x]\n";
        let parsed = parse(src);
        let pos = at(src, "create_cpm5", 1); // the call inside [ ]
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::CallSite { proc_name, .. } => {
                assert_eq!(proc_name, "create_cpm5");
            }
            other => panic!("expected CallSite, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_unknown_call_returns_none() {
        let src = "puts hello\n";
        let parsed = parse(src);
        let pos = first(src, "puts");
        assert!(hover_at(&parsed.document, src, pos).is_none());
    }

    #[test]
    fn hover_on_var_in_condition_shows_param() {
        // `$kind` inside an opaque `if` condition resolves, via the
        // source scan, to the proc parameter — rendered like an arg.
        let src = "\
proc axis_if {\n  @enum(target, controller) kind\n} {\n\
  set m [ if {$kind == controller} { a } ]\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$kind") + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::ProcArgDef { arg, .. } => {
                assert_eq!(arg.name, "kind");
                assert_eq!(arg.attributes[0].name, "enum");
            }
            other => panic!("expected ProcArgDef, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_local_var_reports_local() {
        let src = "\
proc p {} {\n  set count 0\n  use $count\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$count") + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, .. } => assert_eq!(name, "count"),
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }
}
