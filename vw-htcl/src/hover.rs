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
    WordForm, WordPart,
};
use crate::lower::{signature_table, SignatureTable};
use crate::scope::{
    find_type_decl, innermost_scope, resolve_var_def, scan_var_ref,
    type_expr_at, type_expr_lookup_name, VarDef,
};
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
    /// reference itself. `ty` carries the type inferred from the
    /// binding's RHS when the shape is knowable (a `[typed_proc ...]`
    /// call substitution, a `$other_typed_var` copy, or a
    /// `true`/`false` literal); `None` when the RHS is opaque.
    LocalVar {
        name: String,
        span: Span,
        ty: Option<crate::ast::TypeExpr>,
    },
    /// Cursor is on the name of an `enum` declaration. Shows the
    /// variants block as a hover popup.
    EnumDef {
        decl: &'a crate::ast::EnumDecl,
        span: Span,
    },
    /// Cursor is on a type name in a proc signature or type-decl
    /// underlying — e.g. `MyNewtype` in `proc f {} MyNewtype { … }`
    /// or `dcmac::MacPortProps` in `-port0: dcmac::MacPortProps`.
    /// Resolves to a declared `type` in the document.
    TypeDef {
        decl: &'a crate::ast::TypeDecl,
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
            | HoverTarget::EnumDef { span, .. }
            | HoverTarget::TypeDef { span, .. } => *span,
        }
    }
}

pub fn hover_at<'a>(
    document: &'a Document,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let table = signature_table(document);
    // Doc-comment `[NAME]` reference — cheap to detect and rules
    // out any structural resolution when the cursor is inside a
    // `##` block. Renders the target proc's signature as the hover
    // content, same as if the cursor were on a call site.
    if let Some(t) = hover_in_doc_comment(document, source, offset, &table) {
        return Some(t);
    }
    // Cursor on the name-word of a `set VAR X` binding → treat it
    // as the local's definition site, showing the same
    // name-and-inferred-type hover that a `$VAR` reference would.
    // Runs before the general stmt walker because that walker
    // resolves `set` as a generic call and returns nothing useful
    // for the name-word position.
    if let Some(t) = hover_in_set_binding(document, source, offset, &table) {
        return Some(t);
    }
    // Cursor on a binder-name inside a control-flow varname-list
    // (`foreach {a b}`, `dict for {k v}`, `catch BODY resvar`).
    // Same LocalVar hover shape as the `set NAME` binding site so
    // the two look identical to the reader.
    if let Some(t) = hover_in_control_flow_binding(document, source, offset) {
        return Some(t);
    }
    hover_in_stmts(&document.stmts, &table, source, offset)
        // Fallback: a `$var` reference — including one buried in opaque
        // text (a command substitution or `if`/`while` condition).
        .or_else(|| hover_scanned_var(document, source, offset, &table))
        // Fallback: cursor on a type-name annotation (arg type,
        // return type, `type … = TYPE` underlying, generic arg).
        .or_else(|| hover_of_type(document, offset))
}

/// Cursor on the name-word of a `set NAME VALUE` command (the
/// binding site, no `$` prefix). Reuses the same `infer_local_type`
/// walker as the `$var` fallback so the hover shows `$NAME: T` in
/// both places consistently.
fn hover_in_set_binding<'a>(
    document: &'a Document,
    _source: &'a str,
    offset: u32,
    sig_table: &crate::lower::SignatureTable<'a>,
) -> Option<HoverTarget<'a>> {
    use crate::ast::CommandKind;
    let (stmts, enclosing) = innermost_scope(document, offset);
    let cmd = find_set_command_at(stmts, offset)?;
    // Skip the containing proc when scanning nested control-flow —
    // for now `find_set_command_at` only looks at top-level stmts of
    // the innermost scope, which covers the common case.
    let CommandKind::Set = cmd.kind else {
        return None;
    };
    let name_word = cmd.words.get(1)?;
    if !name_word.span.contains(offset) {
        return None;
    }
    let name = name_word.as_text()?.to_string();
    let ty = infer_local_type(
        stmts,
        enclosing,
        sig_table,
        document,
        &name,
        name_word.span,
    );
    Some(HoverTarget::LocalVar {
        name,
        span: name_word.span,
        ty,
    })
}

/// Cursor sits on a bare identifier inside a control-flow varname
/// list — the `lib` / `srcs` inside `dict for {lib srcs} …`, an
/// `a` / `b` inside `foreach {a b} …`, or the result-var of
/// `catch { … } err`. Returns a `LocalVar` target pointing at the
/// whole braced word (or the bare word), so hover shows the same
/// `$name` shape it would show on a `$name` reference lower in
/// the body.
///
/// Sub-token spans inside the braced list aren't currently
/// tracked by the parser, so the returned span covers the whole
/// braced group — the click target still lands on `lib` (or
/// wherever the cursor is), just with a slightly wider highlight.
fn hover_in_control_flow_binding<'a>(
    document: &'a Document,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    use crate::ast::{CommandKind, WordForm, WordPart};
    let (stmts, _enclosing) = innermost_scope(document, offset);
    // Walk statements looking for a Generic-command whose head is
    // a body-host and whose varname arg contains `offset`.
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if !matches!(cmd.kind, CommandKind::Generic) {
            continue;
        }
        let Some(head) = cmd.words.first().and_then(|w| w.as_text()) else {
            continue;
        };
        // Collect (word-idx, is-varname-list) tuples per head.
        let varname_word_indices: Vec<usize> = match head {
            "foreach" => {
                // Word 1, 3, 5, … up to body_idx (last word).
                let body_idx = cmd.words.len().saturating_sub(1);
                (1..body_idx).step_by(2).collect()
            }
            "dict" => {
                if cmd.words.get(1).and_then(|w| w.as_text()) == Some("for") {
                    vec![2]
                } else {
                    continue;
                }
            }
            "catch" => vec![2, 3],
            _ => continue,
        };
        for idx in varname_word_indices {
            let Some(word) = cmd.words.get(idx) else {
                continue;
            };
            if !word.span.contains(offset) {
                continue;
            }
            // Figure out which sub-name the cursor is on.
            let target_name = match word.form {
                WordForm::Bare => word.as_text()?.to_string(),
                WordForm::Braced => {
                    let WordPart::Text {
                        value,
                        span: text_span,
                    } = word.parts.first()?
                    else {
                        continue;
                    };
                    // Find the whitespace-delimited token at the
                    // cursor offset within the braced interior.
                    let rel = offset.saturating_sub(text_span.start) as usize;
                    let bytes = value.as_bytes();
                    let mut start = rel.min(bytes.len());
                    while start > 0
                        && !bytes[start - 1].is_ascii_whitespace()
                        && bytes[start - 1] != b'{'
                    {
                        start -= 1;
                    }
                    let mut end = rel.min(bytes.len());
                    while end < bytes.len()
                        && !bytes[end].is_ascii_whitespace()
                        && bytes[end] != b'}'
                    {
                        end += 1;
                    }
                    if start >= end {
                        continue;
                    }
                    value[start..end].to_string()
                }
                _ => continue,
            };
            let _ = source; // reserved for future sub-token spans
            return Some(HoverTarget::LocalVar {
                name: target_name,
                span: word.span,
                ty: None,
            });
        }
    }
    None
}

fn find_set_command_at(stmts: &[Stmt], offset: u32) -> Option<&Command> {
    use crate::ast::{CommandKind, Stmt};
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if matches!(cmd.kind, CommandKind::Set) {
            return Some(cmd);
        }
    }
    None
}

/// Cursor on a type name → return a `TypeDef` hover target for the
/// matching declaration. Mirror of [`crate::goto::definition_of_type`].
fn hover_of_type(document: &Document, offset: u32) -> Option<HoverTarget<'_>> {
    let ty = type_expr_at(document, offset)?;
    let name = type_expr_lookup_name(ty);
    let decl = find_type_decl(document, &name)?;
    Some(HoverTarget::TypeDef {
        decl,
        span: ty.span(),
    })
}

/// Hover for a `[NAME]` reference inside a `##` doc-comment block.
/// The result mirrors `HoverTarget::CallSite` for the referenced
/// proc, so the LSP formatter renders the target's signature with
/// its own docs — exactly what a reader following the reference
/// wants to see.
fn hover_in_doc_comment<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
    table: &SignatureTable<'a>,
) -> Option<HoverTarget<'a>> {
    let block = enclosing_doc_block(&document.stmts, offset)?;
    let name = extract_ref_at(source, block, offset)?;
    // Anchor the hover span on the `[NAME]` reference itself so the
    // editor highlights just that token.
    let ref_span = ref_span_at(source, block, offset)?;
    let sig = *table.get(&name)?;
    Some(HoverTarget::CallSite {
        proc_name: name,
        signature: sig,
        span: ref_span,
    })
}

/// Return the doc-comment span containing `offset`. Mirror of
/// [`crate::goto::enclosing_doc_block`] — kept crate-local because
/// both consumers want the same rule.
fn enclosing_doc_block(stmts: &[Stmt], offset: u32) -> Option<Span> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if let Some(span) = cmd.doc_comments_span {
            if span.contains(offset) {
                return Some(span);
            }
        }
        if let CommandKind::Proc(proc) = &cmd.kind {
            if let Some(sig) = &proc.signature {
                for arg in &sig.args {
                    if let Some(span) = arg.doc_comments_span {
                        if span.contains(offset) {
                            return Some(span);
                        }
                    }
                }
            }
            if let Some(span) = enclosing_doc_block(&proc.body, offset) {
                return Some(span);
            }
        }
        if let CommandKind::NamespaceEval(ns) = &cmd.kind {
            if let Some(span) = enclosing_doc_block(&ns.body, offset) {
                return Some(span);
            }
        }
    }
    None
}

/// Extract the identifier inside a `[NAME]` reference at `offset`.
/// See goto.rs's `extract_ref_at` for the disambiguation rules.
fn extract_ref_at(source: &str, block: Span, offset: u32) -> Option<String> {
    let (_, name) = find_ref(source, block, offset)?;
    Some(name)
}

/// Return the inclusive span of the `[NAME]` token at `offset` in
/// the block, so the hover popup anchors on the reference (not the
/// entire doc block).
fn ref_span_at(source: &str, block: Span, offset: u32) -> Option<Span> {
    let (span, _) = find_ref(source, block, offset)?;
    Some(span)
}

fn find_ref(source: &str, block: Span, offset: u32) -> Option<(Span, String)> {
    let bytes = source.as_bytes();
    let start = block.start as usize;
    let end = (block.end as usize).min(bytes.len());
    let off = offset as usize;
    if off < start || off > end {
        return None;
    }
    let mut i = start;
    while i < end {
        if bytes[i] == b'[' {
            let content_start = i + 1;
            // Ident-start rule mirrors doc.rs / goto.rs.
            if content_start < end
                && (bytes[content_start].is_ascii_alphabetic()
                    || bytes[content_start] == b'_')
            {
                let mut j = content_start;
                while j < end && bytes[j] != b']' {
                    let b = bytes[j];
                    let ok =
                        b.is_ascii_alphanumeric() || b == b'_' || b == b':';
                    if !ok {
                        break;
                    }
                    j += 1;
                }
                if j < end && bytes[j] == b']' && j > content_start {
                    if off >= i && off <= j {
                        let name =
                            std::str::from_utf8(&bytes[content_start..j])
                                .ok()?
                                .to_string();
                        return Some((
                            Span::new(i as u32, (j + 1) as u32),
                            name,
                        ));
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    None
}

/// Hover for a `$var` reference found by scanning the source. Resolves
/// to a parameter (rendered like an arg) or a local (`set`/`variable`).
fn hover_scanned_var<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
    sig_table: &crate::lower::SignatureTable<'a>,
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
        VarDef::Local(def_span) => {
            let ty = infer_local_type(
                stmts, enclosing, sig_table, document, &name, def_span,
            );
            Some(HoverTarget::LocalVar { name, span, ty })
        }
    }
}

/// Infer the type of the local variable `name` whose defining `set`
/// command's name-word lives at `def_span`. Walks `scope_stmts` in
/// order to seed `VarTypeTable` with any typed `set`s that come
/// before the target — so a chain like `set a [typed]; set b $a`
/// still types `b`. Seeds parameter types too, in case `set b $arg`
/// forwards a parameter through a local. Returns `None` when the
/// RHS is opaque (the same policy the validator uses).
fn infer_local_type<'a>(
    scope_stmts: &'a [Stmt],
    enclosing: Option<&'a crate::ast::Proc>,
    sig_table: &crate::lower::SignatureTable<'a>,
    document: &'a Document,
    name: &str,
    def_span: Span,
) -> Option<crate::ast::TypeExpr> {
    use crate::ast::{CommandKind, Stmt};
    let mut var_table = crate::validate::VarTypeTable::new();
    // Parameter types are visible to `set` RHS inference — a forward
    // like `set out $arg` propagates the arg's type through.
    if let Some(proc) = enclosing {
        if let Some(sig) = &proc.signature {
            for arg in &sig.args {
                if let Some(ty) = &arg.type_annotation {
                    var_table.insert(arg.name.clone(), ty.clone());
                }
            }
        }
    }
    // Full document-wide proc table so `[user_proc]` return-type
    // inference kicks in for user procs without an annotated
    // return type — same walker the REPL / putr chain uses.
    let proc_table = crate::validate::build_proc_table(document);
    for stmt in scope_stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !matches!(cmd.kind, CommandKind::Set) {
            continue;
        }
        let Some(name_word) = cmd.words.get(1) else {
            continue;
        };
        let Some(value_word) = cmd.words.get(2) else {
            continue;
        };
        let Some(binding_name) = name_word.as_text() else {
            continue;
        };
        // Type the RHS in the pre-target var-table (chain support).
        let ty = crate::validate::value_type_with_procs(
            value_word,
            sig_table,
            &var_table,
            Some(&proc_table),
        );
        if let Some(ref t) = ty {
            var_table.insert(binding_name.to_string(), t.clone());
        }
        // Return the type of the specific binding the hover is on —
        // matched by name-word span so shadowed bindings before it
        // don't overwrite the answer.
        if name_word.span == def_span && binding_name == name {
            return ty;
        }
    }
    None
}

/// Find the hover target at `offset` within `stmts`, descending into
/// proc bodies. The signature table is the document-wide (top-level)
/// one, so a call inside a body still resolves to the proc it names.
fn hover_in_stmts<'a>(
    stmts: &'a [Stmt],
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if let Some(target) = hover_in_command(cmd, table, source, offset) {
            return Some(target);
        }
    }
    None
}

fn hover_in_command<'a>(
    cmd: &'a Command,
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let primary = match &cmd.kind {
        CommandKind::Proc(proc) => hover_in_proc_decl(proc, offset)
            // Cursor isn't on the proc's name or an arg — look inside
            // the body.
            .or_else(|| hover_in_stmts(&proc.body, table, source, offset)),
        CommandKind::NamespaceEval(ns) => {
            // `namespace eval <name> { … }` — descend into the
            // populated body. The parser's post-pass already
            // reparsed the block into `ns.body`, so we walk the
            // structured AST rather than re-triggering the on-demand
            // brace-body reparse.
            hover_in_stmts(&ns.body, table, source, offset)
        }
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
    primary
        .or_else(|| hover_in_cmd_substs(&cmd.words, table, source, offset))
        .or_else(|| hover_in_braced_bodies(cmd, table, source, offset))
}

/// Cursor inside a `{ … }` control-flow body (the second word of
/// `if`, the third of `while`, the body of `foreach`, etc.). The
/// parser leaves those as opaque `Braced` words — semantically
/// Tcl scripts, but no eager-parse pass like `[ … ]` gets. So we
/// reparse the body on demand when hover lands the cursor inside
/// one. Mirrors `goto::definition_in_braced_bodies` (same fix, same
/// motivating case — IP-wrapper `set_property` calls sit inside
/// `if {[llength $_vw_d] > 0} { … }` scaffolds).
fn hover_in_braced_bodies<'a>(
    cmd: &'a Command,
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let head = cmd.words.first().and_then(|w| w.as_text())?;
    if !is_body_host(head) {
        return None;
    }
    for word in cmd.words.iter().skip(1) {
        if !matches!(word.form, WordForm::Braced) {
            continue;
        }
        if !word.span.contains(offset) {
            continue;
        }
        let Some(WordPart::Text {
            value,
            span: text_span,
        }) = word.parts.first()
        else {
            continue;
        };
        // Reparse against the fragment text; shift spans up to
        // whole-source coordinates; THEN run `populate_procs` so
        // nested `[ … ]` CmdSubst bodies inside the brace-body
        // get their own recursive parse. Without the populate
        // pass, a call like
        // `set cell [vivado_cmd::create_bd_cell …]` inside an
        // `if {$bd} { … }` branch has an empty CmdSubst body and
        // the transient walker can't reach the call.
        let (mut stmts, mut errs) = crate::parser::parse_fragment(
            value.as_str(),
            crate::parser::Mode::BracketBody,
        );
        let delta = text_span.start;
        for s in &mut stmts {
            crate::parser::shift_stmt(s, delta);
        }
        crate::parser::populate_procs(&mut stmts, source, &mut errs);
        // NOTE: the reparsed stmts are owned by this call; but
        // `HoverTarget` variants only borrow from `Command` /
        // `Proc` / `ProcArg` / `ProcSignature` values that we've
        // been threading via `&'a` from the outer document.
        // Anything produced from the *reparsed* fragment would need
        // its own owned storage — and there's nowhere to put it in
        // the current HoverTarget shape. Rather than restructure
        // that lifetime, we recurse only to look up calls that
        // resolve through `table` (which lives at the top level
        // and is already `'a`).
        return hover_in_stmts_transient(&stmts, table, source, offset);
    }
    None
}

/// Sig-table-only pass over transient (locally-owned) stmts:
/// only the `hover_in_call` branch that resolves through the
/// document-level `SignatureTable<'a>` is reachable. See the
/// comment in [`hover_in_braced_bodies`] for the lifetime story.
fn hover_in_stmts_transient<'a>(
    stmts: &[Stmt],
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        // Cursor on a call name → hover its proc via `table`.
        if let Some(target) = hover_call_via_table(cmd, table, offset) {
            return Some(target);
        }
        // Nested `[ … ]` inside the reparsed body — recurse via
        // the same transient walker so an `if { if { … [call] } }`
        // chain works.
        for word in &cmd.words {
            if !word.span.contains(offset) {
                continue;
            }
            for part in &word.parts {
                if let WordPart::CmdSubst { span, body, .. } = part {
                    if span.contains(offset) {
                        return hover_in_stmts_transient(
                            body, table, source, offset,
                        );
                    }
                }
            }
        }
        // Nested braced body inside the reparsed body — same idea.
        let head = cmd.words.first().and_then(|w| w.as_text());
        if let Some(head) = head {
            if is_body_host(head) {
                for word in cmd.words.iter().skip(1) {
                    if !matches!(word.form, WordForm::Braced) {
                        continue;
                    }
                    if !word.span.contains(offset) {
                        continue;
                    }
                    let Some(WordPart::Text {
                        value,
                        span: text_span,
                    }) = word.parts.first()
                    else {
                        continue;
                    };
                    let (mut inner, mut inner_errs) =
                        crate::parser::parse_fragment(
                            value.as_str(),
                            crate::parser::Mode::BracketBody,
                        );
                    let delta = text_span.start;
                    for s in &mut inner {
                        crate::parser::shift_stmt(s, delta);
                    }
                    crate::parser::populate_procs(
                        &mut inner,
                        source,
                        &mut inner_errs,
                    );
                    return hover_in_stmts_transient(
                        &inner, table, source, offset,
                    );
                }
            }
        }
    }
    None
}

/// Sig-table-only variant of [`hover_in_call`] that avoids taking
/// any borrow from `cmd` — returned data references only `'a`
/// values that live in the outer document via the sig table.
fn hover_call_via_table<'a>(
    cmd: &Command,
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    let first = cmd.words.first()?;
    let name_text = first.as_text()?;
    let sig = *table.get(name_text)?;
    // Cursor on the call name → the callee's signature.
    if first.span.contains(offset) {
        return Some(HoverTarget::CallSite {
            proc_name: name_text.to_string(),
            signature: sig,
            span: first.span,
        });
    }
    // Cursor on a `-flag` word → the corresponding arg's docs.
    for word in cmd.words.iter().skip(1) {
        if !word.span.contains(offset) {
            continue;
        }
        let text = word.as_text()?;
        let flag = text.strip_prefix('-')?;
        let arg = sig.find(flag)?;
        return Some(HoverTarget::CallArg {
            proc_name: name_text.to_string(),
            arg,
            span: word.span,
        });
    }
    None
}

/// Command names whose brace-args hold Tcl scripts rather than
/// data. Same list as [`crate::goto`]'s counterpart. Exposed to
/// the crate so the unused-var pass (`crate::unused`) can reuse
/// it without a third copy.
pub(crate) fn is_body_host(head: &str) -> bool {
    matches!(
        head,
        "if" | "elseif"
            | "else"
            | "while"
            | "for"
            | "foreach"
            | "catch"
            | "try"
            | "finally"
            | "eval"
            | "uplevel"
            | "namespace"
            | "on"
            | "apply"
            // `dict for` — head word alone doesn't disambiguate
            // (`dict get`/`dict set`/… have no script body); we
            // include `dict` here and let the per-word-form loop
            // skip non-braced args. False positives cost a
            // reparse but never produce spurious hover results.
            | "dict"
    )
}

/// Descend into any `[ … ]` command substitutions on this command's
/// words so hover works on calls written inline, e.g.
/// `set cell [create_cpm5 -name x]`.
fn hover_in_cmd_substs<'a>(
    words: &'a [Word],
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<HoverTarget<'a>> {
    for word in words {
        if !word.span.contains(offset) {
            continue;
        }
        for part in &word.parts {
            if let crate::ast::WordPart::CmdSubst { span, body, .. } = part {
                if span.contains(offset) {
                    return hover_in_stmts(body, table, source, offset);
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

    #[test]
    fn hover_on_var_ref_inside_dict_for_body_resolves_to_kv_binder() {
        // `dict for {lib srcs} $deps { … $lib … }` — hovering the
        // `$lib` reference in the body should resolve to the
        // binder at the `{lib srcs}` list.
        let src = "\
set deps [some_proc]
dict for {lib srcs} $deps {
  puts $lib
}
";
        let parsed = parse(src);
        let pos = first(src, "$lib\n") + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, .. } => assert_eq!(name, "lib"),
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_bare_name_inside_dict_for_kv_list_resolves() {
        // Cursor on `lib` INSIDE `{lib srcs}` — the binding site
        // itself. Same LocalVar hover shape as if the cursor were
        // on `$lib` later in the body.
        let src = "dict for {lib srcs} $deps { puts $lib }\n";
        let parsed = parse(src);
        let pos = first(src, "lib srcs");
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, .. } => assert_eq!(name, "lib"),
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_foreach_kv_list_binder_resolves() {
        // Same story for `foreach {a b} $pairs { … }`.
        let src = "foreach {a b} $pairs { puts $a }\n";
        let parsed = parse(src);
        let pos = first(src, "a b}");
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, .. } => assert_eq!(name, "a"),
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_set_binding_name_infers_type() {
        // Cursor on the LHS of `set x [typed]` (no `$`) — should
        // resolve as a LocalVar with the RHS's type, same as
        // hovering `$x` later would.
        let src = "\
proc make_it {} string { return hi }
proc p {} {
  set x [make_it]
}
";
        let parsed = parse(src);
        // Cursor lands on the `x` of `set x` (not `$x`).
        let pos = first(src, "set x ") + "set ".len() as u32;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, ty, .. } => {
                assert_eq!(name, "x");
                let ty = ty.expect("expected inferred type");
                assert!(
                    matches!(
                        ty,
                        crate::ast::TypeExpr::Named { ref name, .. }
                            if name == "string"
                    ),
                    "got {ty:?}",
                );
            }
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_local_var_infers_type_from_typed_call() {
        // `set x [typed_proc ...]` where `typed_proc` has an
        // annotated return type → hovering `$x` should carry that
        // type.
        let src = "\
proc make_it {} string { return hi }
proc p {} {
  set x [make_it]
  use $x
}
";
        let parsed = parse(src);
        let pos = first(src, "$x") + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { name, ty, .. } => {
                assert_eq!(name, "x");
                let ty = ty.expect("expected inferred type");
                assert!(
                    matches!(
                        ty,
                        crate::ast::TypeExpr::Named { ref name, .. }
                            if name == "string"
                    ),
                    "got {ty:?}",
                );
            }
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_local_var_untyped_rhs_reports_no_type() {
        let src = "\
proc p {} {
  set x [some_untyped]
  use $x
}
";
        let parsed = parse(src);
        let pos = first(src, "$x") + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::LocalVar { ty, .. } => {
                assert!(ty.is_none(), "expected no type, got {ty:?}");
            }
            other => panic!("expected LocalVar, got {other:?}"),
        }
    }

    /// `[NAME]` inside a `##` block renders as a CallSite hover on
    /// the referenced proc — same as if the cursor were on a live
    /// call. Lets the reader hover the reference and see the
    /// target's signature.
    #[test]
    fn doc_ref_hovers_as_target_call_site() {
        let src = "\
## Documented target proc.
proc target {} { puts hi }
## See [target] for more.
proc caller {} { return 1 }
";
        let parsed = crate::parser::parse(src);
        let pos = src.find("[target]").unwrap() as u32 + 1;
        let target = hover_at(&parsed.document, src, pos).unwrap();
        match target {
            HoverTarget::CallSite { proc_name, .. } => {
                assert_eq!(proc_name, "target");
            }
            other => panic!("expected CallSite, got {other:?}"),
        }
    }
}
