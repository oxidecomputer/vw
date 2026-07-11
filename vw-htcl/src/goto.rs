// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Find the source-location a reference points to.
//!
//! Used by `vw analyzer` for `textDocument/definition`. Phase 2 scope:
//!
//! - Cursor on a call-site name → declaring `proc`'s name span.
//! - Cursor on an attribute ident value (e.g. the `has_tuser` inside
//!   `@requires(has_tuser)`) → the referenced arg's name span.
//! - Cursor on a variable reference (`$mode`) → its definition in the
//!   enclosing scope: a preceding `set`/`variable`, or, failing that,
//!   a parameter of the enclosing proc.
//!
//! Scope is approximated by Tcl's lexical structure: a proc body is
//! its own local scope (params + `set`s), top-level code is global.
//! `src` imports will be added when Phase 1's module system lands.

use crate::ast::{
    AttributeValue, Command, CommandKind, Document, Proc, ProcArg,
    ProcSignature, Stmt, WordForm, WordPart,
};
use crate::lower::{signature_table, SignatureTable};
use crate::scope::{
    find_type_decl, innermost_scope, resolve_var_def, scan_var_ref,
    type_expr_at, type_expr_lookup_name,
};
use crate::span::Span;

pub fn definition_at(
    document: &Document,
    source: &str,
    offset: u32,
) -> Option<Span> {
    let table = signature_table(document);
    // Try a doc-comment `[NAME]` reference first — it's cheap and
    // rules out any structural resolution when the cursor is inside
    // a `##` block. Otherwise fall through to the structural paths.
    if let Some(span) =
        definition_in_doc_comment(document, source, offset, &table)
    {
        return Some(span);
    }
    definition_in_stmts(&document.stmts, None, document, &table, source, offset)
        // Fallback: a `$var` the structured tree keeps opaque — inside
        // a command substitution or an `if`/`while` condition. Found by
        // scanning the source and resolving against the enclosing
        // proc's scope.
        .or_else(|| definition_of_scanned_var(document, source, offset))
        // Fallback: cursor on a type-name annotation (arg type,
        // return type, `type … = TYPE` underlying, generic arg).
        .or_else(|| definition_of_type(document, offset))
}

/// Cursor on a type name in a proc signature or a `type` decl's
/// underlying → return the matching type declaration's name span.
/// Handles qualified names (`dcmac::MacPortProps`) via the parser's
/// `Qualified` variant plus the type-table lookup helper.
fn definition_of_type(document: &Document, offset: u32) -> Option<Span> {
    let ty = type_expr_at(document, offset)?;
    let name = type_expr_lookup_name(ty);
    let decl = find_type_decl(document, &name)?;
    Some(decl.name_span)
}

/// Resolve a `[NAME]` reference embedded in a `##` doc-comment
/// block. Returns `None` when the cursor isn't inside any command's
/// or arg's doc-comment span, or when the `[…]` at the cursor
/// doesn't name a proc declared in this document.
///
/// Cross-file references (e.g. `[Properties::from]` where
/// `Properties::from` lives in `types.htcl`) resolve when the
/// document has already sourced that file; unresolved names simply
/// return `None`, which the LSP client renders as "no definition
/// available" without disrupting the fallback paths.
fn definition_in_doc_comment(
    document: &Document,
    source: &str,
    offset: u32,
    _table: &SignatureTable<'_>,
) -> Option<Span> {
    let block = enclosing_doc_block(&document.stmts, offset)?;
    let name = extract_ref_at(source, block, offset)?;
    let proc = find_proc_decl(document, &name)?;
    Some(proc.name_span)
}

/// Return the doc-comment span that contains `offset`, if any. Walks
/// commands, proc signatures, nested proc bodies, and namespace-eval
/// bodies.
fn enclosing_doc_block(stmts: &[Stmt], offset: u32) -> Option<Span> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if let Some(span) = cmd.doc_comments_span {
            if span.contains(offset) {
                return Some(span);
            }
        }
        // ProcArg doc-comment blocks sit inside the proc's args span.
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

/// Given a doc-comment block-span and a cursor offset inside it,
/// find a `[NAME]` reference whose interior span contains `offset`
/// and return `NAME`. Interior must be a valid ident (letters,
/// digits, `_`, `:` for namespace qualification). `[` and `]` are
/// the disambiguators from surrounding prose.
fn extract_ref_at(source: &str, block: Span, offset: u32) -> Option<String> {
    let bytes = source.as_bytes();
    let start = block.start as usize;
    let end = (block.end as usize).min(bytes.len());
    let off = offset as usize;
    if off < start || off > end {
        return None;
    }
    // Scan for `[…]` pairs. `[` and `]` are cheap to find; the
    // interior is validated once we have both delimiters.
    let mut i = start;
    while i < end {
        if bytes[i] == b'[' {
            let content_start = i + 1;
            // Ident-start rule: first char must be a letter or `_`.
            // Same as `render_refs` in doc.rs — keeps the analyzer
            // and the renderer in agreement on what counts as a ref.
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
                    // Reference spans `[NAME]` inclusive. The cursor
                    // hits a ref if it's on any of `[`, `NAME`, or `]`.
                    if off >= i && off <= j {
                        let name =
                            std::str::from_utf8(&bytes[content_start..j])
                                .ok()?
                                .to_string();
                        return Some(name);
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

fn definition_of_scanned_var(
    document: &Document,
    source: &str,
    offset: u32,
) -> Option<Span> {
    let (name, _) = scan_var_ref(source, offset)?;
    let (stmts, enclosing) = innermost_scope(document, offset);
    resolve_var_def(&name, stmts, enclosing, offset).map(|d| d.def_span())
}

/// Resolve the definition at `offset` within `stmts`, descending into
/// proc bodies. `enclosing` is the proc whose body `stmts` belongs to
/// (`None` at the top level), used to resolve variables to parameters.
/// `document` is the whole document so call sites — at any nesting
/// depth — can find their declaring proc, which always lives at the
/// top level.
fn definition_in_stmts<'a>(
    stmts: &'a [Stmt],
    enclosing: Option<&'a Proc>,
    document: &'a Document,
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<Span> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }

        // Inside a proc declaration, attribute ident values can
        // reference sibling args by name. Resolve those to the arg's
        // declaration site.
        if let CommandKind::Proc(proc) = &cmd.kind {
            if let Some(span) = definition_in_proc_decl(proc, offset) {
                return Some(span);
            }
            // Cursor on the proc's own name — "goto def" of the def
            // itself is the same span. Not super useful but
            // consistent.
            if proc.name_span.contains(offset) {
                return Some(proc.name_span);
            }
            // Otherwise the cursor is somewhere in the body: recurse,
            // making this proc the enclosing scope.
            return definition_in_stmts(
                &proc.body,
                Some(proc),
                document,
                table,
                source,
                offset,
            );
        }

        // `namespace eval <name> { … }` — descend into the populated
        // body directly. Without this arm we'd fall through to the
        // brace-body reparse path, which works but discards the
        // pre-populated proc bodies and re-triggers a full parse.
        if let CommandKind::NamespaceEval(ns) = &cmd.kind {
            if let Some(span) = definition_in_stmts(
                &ns.body, enclosing, document, table, source, offset,
            ) {
                return Some(span);
            }
        }

        // Cursor on a `$var` reference → its definition in scope.
        if let Some(span) = definition_of_var(cmd, stmts, enclosing, offset) {
            return Some(span);
        }

        // Generic call site. Two flavors:
        //   1. Cursor on the call name → proc declaration.
        //   2. Cursor on a `-flag` arg → that arg's decl in the proc.
        if let Some(span) = definition_in_call(cmd, document, table, offset) {
            return Some(span);
        }

        // Cursor inside a `[ … ]` command substitution → recurse into
        // its parsed body so goto works on calls written inline.
        if let Some(span) =
            definition_in_cmd_substs(cmd, document, table, source, offset)
        {
            return Some(span);
        }

        // Cursor inside a `{ … }` control-flow body (the second word
        // of `if`, the third of `while`, the body of `foreach`,
        // etc.). The parser leaves those as opaque `Braced` words —
        // they're semantically Tcl scripts, but there's no
        // eager-parse pass like there is for `[ … ]`. So we reparse
        // on demand when goto lands the cursor inside one.
        if let Some(span) =
            definition_in_braced_bodies(cmd, document, table, source, offset)
        {
            return Some(span);
        }
    }
    None
}

/// When the cursor sits inside a `{ … }` word of a control-flow
/// command, reparse that word's interior as a htcl fragment, shift
/// all spans back into whole-source coordinates, and recurse into
/// [`definition_in_stmts`]. Without this, `if { … <call> … }`,
/// `while { … <call> … }`, `foreach x $xs { … <call> … }`, and
/// friends never resolve their body calls — goto-def returns
/// "no definition found" from inside every generated
/// `if {[llength …]} { <wrapped_call> }` scaffold.
///
/// Only `Braced` words are considered; the enclosing command's
/// head has to be one of a small list of body-hosts so we don't
/// waste a parse on `list {a b c}`-style data braces (their content
/// is a Tcl list, not a script).
fn definition_in_braced_bodies<'a>(
    cmd: &'a Command,
    document: &'a Document,
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<Span> {
    let head = cmd.words.first().and_then(|w| w.as_text())?;
    if !is_body_host(head) {
        return None;
    }
    // The interior text lives in the source we're parsing against
    // — we don't have that here directly, but we do have span
    // access to the whole-source Command tree. Each `Braced` word's
    // interior is `span.start+1 .. span.end-1` in the outer
    // source. We recover it via the WordPart::Text that the parser
    // populates for braced words.
    for word in cmd.words.iter().skip(1) {
        if !matches!(word.form, WordForm::Braced) {
            continue;
        }
        if !word.span.contains(offset) {
            continue;
        }
        // First-part Text carries the interior string with a span
        // starting at word.span.start + 1 (the byte after `{`).
        let Some(WordPart::Text {
            value,
            span: text_span,
        }) = word.parts.first()
        else {
            continue;
        };
        // Reparse the body as a bracket-body-mode fragment so
        // newlines are whitespace and multi-line control flow
        // parses cleanly.
        // Reparse the brace-body against `source` — the WHOLE
        // outer document. That lets us pass `source` back into
        // `populate_procs` below so nested `[ … ]` CmdSubst
        // bodies get filled in against the same coordinate space
        // the outer AST uses. Without the populate pass, a call
        // like `set cell [vivado_cmd::create_bd_cell …]` inside
        // `if {$bd} { … }` still has an empty CmdSubst.body and
        // the recursion bottoms out at `set`.
        //
        // Order matters: shift first (spans become absolute), then
        // populate — the shifted CmdSubst.span carries the
        // absolute offset `populate_cmd_subst_parts` needs to shift
        // its reparsed interior into place.
        let (mut stmts, mut errs) = crate::parser::parse_fragment(
            value.as_str(),
            crate::parser::Mode::BracketBody,
        );
        let delta = text_span.start;
        for s in &mut stmts {
            crate::parser::shift_stmt(s, delta);
        }
        crate::parser::populate_procs(&mut stmts, source, &mut errs);
        // Recurse into the reparsed stmts. Passing `None` as the
        // enclosing proc mirrors what `definition_in_cmd_substs`
        // does — variable resolution across a control-flow body
        // is out of scope for this fix; the call-site → proc-decl
        // path is what matters.
        return definition_in_stmts(
            &stmts, None, document, table, source, offset,
        );
    }
    None
}

/// Command names whose brace-args hold Tcl scripts rather than
/// data. Restricting the lookup to these keeps us from waking the
/// parser on data braces (`list {a b c}`, `dict {k v}`, etc.).
fn is_body_host(head: &str) -> bool {
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
    )
}

fn definition_in_cmd_substs<'a>(
    cmd: &'a Command,
    document: &'a Document,
    table: &SignatureTable<'a>,
    source: &'a str,
    offset: u32,
) -> Option<Span> {
    for word in &cmd.words {
        if !word.span.contains(offset) {
            continue;
        }
        for part in &word.parts {
            if let crate::ast::WordPart::CmdSubst { span, body, .. } = part {
                if span.contains(offset) {
                    return definition_in_stmts(
                        body, None, document, table, source, offset,
                    );
                }
            }
        }
    }
    None
}

/// If the cursor is on a `$var` reference (a real [`WordPart::VarRef`])
/// in `cmd`, resolve it to its definition within `scope_stmts` or a
/// parameter of `enclosing`.
fn definition_of_var<'a>(
    cmd: &'a Command,
    scope_stmts: &'a [Stmt],
    enclosing: Option<&'a Proc>,
    offset: u32,
) -> Option<Span> {
    let name = var_ref_at(cmd, offset)?;
    resolve_var_def(name, scope_stmts, enclosing, offset).map(|d| d.def_span())
}

/// The name of the `$var` reference under the cursor, if any. Walks
/// word parts so it also fires inside quoted words (`"hi $name"`) and
/// array syntax (`$arr($idx)`).
fn var_ref_at(cmd: &Command, offset: u32) -> Option<&str> {
    for word in &cmd.words {
        if !word.span.contains(offset) {
            continue;
        }
        for part in &word.parts {
            if let WordPart::VarRef { name, span, .. } = part {
                if span.contains(offset) {
                    return Some(name.as_str());
                }
            }
        }
    }
    None
}

fn definition_in_call<'a>(
    cmd: &'a Command,
    document: &'a Document,
    table: &SignatureTable<'a>,
    offset: u32,
) -> Option<Span> {
    let first = cmd.words.first()?;
    let name = first.as_text()?;

    // Cursor on the call name.
    if first.span.contains(offset) {
        let proc = find_proc_decl(document, name)?;
        return Some(proc.name_span);
    }

    // Cursor on one of the `-flag` words. Look the flag up in the
    // called proc's signature and return that arg's name_span.
    let sig = *table.get(name)?;
    for word in cmd.words.iter().skip(1) {
        if !word.span.contains(offset) {
            continue;
        }
        let text = word.as_text()?;
        let flag = text.strip_prefix('-')?;
        let arg = sig.find(flag)?;
        return Some(arg.name_span);
    }

    None
}

/// Find the `proc` declaration that registers under `name` in the
/// document's signature table. Walks `namespace eval` bodies
/// recursively so a call to `project::set_target_language` resolves
/// to the inner `proc set_target_language` inside
/// `namespace eval project { … }`.
fn find_proc_decl<'a>(document: &'a Document, name: &str) -> Option<&'a Proc> {
    find_proc_decl_in(&document.stmts, "", name)
}

fn find_proc_decl_in<'a>(
    stmts: &'a [Stmt],
    prefix: &str,
    name: &str,
) -> Option<&'a Proc> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(decl_name) = proc.name.as_deref() else {
                    continue;
                };
                let qualified = if prefix.is_empty() {
                    decl_name.to_string()
                } else {
                    format!("{prefix}::{decl_name}")
                };
                if qualified == name {
                    return Some(proc);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(ns_name) = ns.name.as_deref() else {
                    continue;
                };
                let nested = if prefix.is_empty() {
                    ns_name.to_string()
                } else {
                    format!("{prefix}::{ns_name}")
                };
                if let Some(found) = find_proc_decl_in(&ns.body, &nested, name)
                {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn definition_in_proc_decl(proc: &Proc, offset: u32) -> Option<Span> {
    let sig = proc.signature.as_ref()?;
    for arg in &sig.args {
        for attr in &arg.attributes {
            for value in &attr.values {
                let AttributeValue::Ident { value: name, span } = value else {
                    continue;
                };
                if !span.contains(offset) {
                    continue;
                }
                if let Some(target) = find_sibling_arg(sig, name) {
                    return Some(target.name_span);
                }
                // Ident value naming an unknown arg — no definition.
                return None;
            }
        }
    }
    None
}

fn find_sibling_arg<'a>(
    sig: &'a ProcSignature,
    name: &str,
) -> Option<&'a ProcArg> {
    sig.args.iter().find(|a| a.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn first(src: &str, needle: &str) -> u32 {
        src.find(needle).expect("needle not found") as u32
    }

    fn nth(src: &str, needle: &str, n: usize) -> u32 {
        let mut start = 0;
        for i in 0..=n {
            let pos = src[start..]
                .find(needle)
                .map(|p| start + p)
                .expect("needle not found enough times");
            if i == n {
                return pos as u32;
            }
            start = pos + needle.len();
        }
        unreachable!()
    }

    #[test]
    fn call_to_proc_decl() {
        let src = "\
proc greet {\n  name\n} { puts hi }\n\
greet -name there\n";
        let parsed = parse(src);
        // Cursor on the `g` of the call-site `greet`.
        let pos = first(src, "greet -");
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // Should point at the `greet` in the proc declaration (first
        // occurrence after `proc `).
        let decl_span = parsed
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Command(c) => match &c.kind {
                    CommandKind::Proc(p)
                        if p.name.as_deref() == Some("greet") =>
                    {
                        Some(p.name_span)
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(target, decl_span);
    }

    #[test]
    fn attribute_ident_to_sibling_arg() {
        let src = "\
proc f {\n  has_a\n  @requires(has_a) has_b\n} { }\n";
        let parsed = parse(src);
        // Cursor on `has_a` inside `@requires(has_a)`.
        // First occurrence is the declaration; second is the
        // attribute argument.
        let pos = nth(src, "has_a", 1);
        let target = definition_at(&parsed.document, src, pos).unwrap();
        let decl_pos = first(src, "has_a");
        assert_eq!(target.start, decl_pos);
    }

    #[test]
    fn call_to_unknown_proc_returns_none() {
        let src = "puts hello\n";
        let parsed = parse(src);
        assert!(
            definition_at(&parsed.document, src, first(src, "puts")).is_none()
        );
    }

    #[test]
    fn attribute_ident_to_unknown_arg_returns_none() {
        let src = "proc f {\n  @requires(typo) only\n} { }\n";
        let parsed = parse(src);
        let pos = first(src, "typo");
        assert!(definition_at(&parsed.document, src, pos).is_none());
    }

    #[test]
    fn call_flag_to_arg_decl() {
        let src = "\
proc show {\n  flag_a\n  width\n} { }\n\
show -width 16\n";
        let parsed = parse(src);
        // Cursor on `-width` at the call site.
        let pos = first(src, "-width");
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // Decl `width` arg name is the second `width` in the source.
        let decl_pos = nth(src, "width", 0);
        assert_eq!(target.start, decl_pos);
    }

    #[test]
    fn call_to_namespaced_proc_resolves_to_inner_decl() {
        // `project::set_target_language` at the call site should
        // resolve to `proc set_target_language` declared inside the
        // matching `namespace eval project { ... }` block.
        let src = "\
namespace eval project {
  proc set_target_language {
    proj
    language
  } { }
}
project::set_target_language -proj p -language VHDL
";
        let parsed = parse(src);
        let pos = first(src, "project::set_target_language");
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // The decl's name span covers just `set_target_language`
        // (without the namespace prefix), which appears as the
        // first occurrence of that bare token in the source.
        let decl_pos = first(src, "set_target_language");
        assert_eq!(target.start, decl_pos);
    }

    #[test]
    fn call_inside_proc_body_to_proc_decl() {
        // Mirrors interface.htcl: a call to a top-level proc from
        // inside another proc's body.
        let src = "\
proc if_tport {\n  type\n  name\n} { }\n\
proc axis {\n  width\n} {\n  if_tport\n}\n";
        let parsed = parse(src);
        // Cursor on the `if_tport` call inside `axis`'s body — the
        // second occurrence of `if_tport`.
        let pos = nth(src, "if_tport", 1);
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // Resolves to the `if_tport` name in the declaration (first
        // occurrence).
        assert_eq!(target.start, first(src, "if_tport"));
    }

    #[test]
    fn var_ref_to_set_in_same_body() {
        // Mirrors interface.htcl: `$mode` resolves to `set mode ...`.
        let src = "\
proc axis_if {\n  kind\n} {\n\
  set mode hello\n\
  use_it $mode\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$mode") + 1; // on the `m` of `$mode`
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // Should point at the `mode` in `set mode`.
        assert_eq!(target.start, first(src, "mode"));
    }

    #[test]
    fn var_ref_to_proc_parameter() {
        // `$name` has no `set`, so it resolves to the proc parameter.
        let src = "\
proc axis_if {\n  kind\n  name\n} {\n\
  use_it $name\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$name") + 1;
        let target = definition_at(&parsed.document, src, pos).unwrap();
        // Parameter `name` decl is the first occurrence of `name`.
        assert_eq!(target.start, first(src, "name"));
    }

    #[test]
    fn var_ref_to_variable_declaration() {
        let src = "\
proc p {} {\n\
  variable vlnv\n\
  use_it $vlnv\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$vlnv") + 1;
        let target = definition_at(&parsed.document, src, pos).unwrap();
        assert_eq!(target.start, first(src, "vlnv"));
    }

    #[test]
    fn unknown_var_ref_returns_none() {
        let src = "proc p {} {\n  use_it $nope\n}\n";
        let parsed = parse(src);
        let pos = first(src, "$nope") + 1;
        assert!(definition_at(&parsed.document, src, pos).is_none());
    }

    #[test]
    fn var_ref_inside_opaque_condition_resolves_to_param() {
        // `$kind` lives inside an `if` condition that sits inside a
        // command substitution — both opaque to the structured tree.
        // The source-scan fallback still resolves it to the parameter.
        let src = "\
proc axis_if {\n  kind\n} {\n\
  set mode [\n\
    if {$kind == controller} { Master }\n\
  ]\n}\n";
        let parsed = parse(src);
        let pos = nth(src, "$kind", 0) + 1;
        let target = definition_at(&parsed.document, src, pos).unwrap();
        assert_eq!(target.start, first(src, "kind"));
    }

    #[test]
    fn call_inside_command_substitution_resolves_to_decl() {
        let src = "\
proc create_cpm5 {\n  name\n} { puts hi }\n\
set cell [create_cpm5 -name x]\n";
        let parsed = parse(src);
        // The second occurrence of `create_cpm5` (the call inside `[…]`).
        let pos = nth(src, "create_cpm5", 1);
        let target = definition_at(&parsed.document, src, pos).unwrap();
        assert_eq!(target.start, first(src, "create_cpm5"));
    }

    #[test]
    fn call_flag_to_unknown_arg_returns_none() {
        let src = "\
proc show {\n  width\n} { }\n\
show -widthz 16\n";
        let parsed = parse(src);
        let pos = first(src, "-widthz");
        assert!(definition_at(&parsed.document, src, pos).is_none());
    }

    /// `[NAME]` in a proc's leading `##` doc block resolves to that
    /// proc's declaration when NAME is defined in the same document.
    #[test]
    fn doc_ref_in_proc_block_resolves_to_proc() {
        let src = "\
proc target {} { puts hi }
## See [target] for related config.
proc other {} { return 1 }
";
        let parsed = parse(src);
        // Cursor on the `t` inside `[target]`.
        let pos = first(src, "[target]") + 1;
        let target = definition_at(&parsed.document, src, pos).unwrap();
        let expected = parsed
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Command(c) => match &c.kind {
                    CommandKind::Proc(p)
                        if p.name.as_deref() == Some("target") =>
                    {
                        Some(p.name_span)
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(target, expected);
    }

    /// `[NAME]` in a proc arg's `##` block resolves too — this is
    /// exactly the shape the generator emits for `-port0`-style args.
    #[test]
    fn doc_ref_in_proc_arg_block_resolves() {
        let src = "\
proc mac_port {} { puts ok }
proc create {
  ## Configuration for MAC port 0. Construct with [mac_port].
  port0
} { return 1 }
";
        let parsed = parse(src);
        let pos = first(src, "[mac_port]") + 1;
        let target = definition_at(&parsed.document, src, pos).unwrap();
        let expected = parsed
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Command(c) => match &c.kind {
                    CommandKind::Proc(p)
                        if p.name.as_deref() == Some("mac_port") =>
                    {
                        Some(p.name_span)
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(target, expected);
    }

    /// Unresolved `[NAME]` — target doesn't exist — falls through to
    /// None without touching the structural paths.
    #[test]
    fn doc_ref_to_unknown_returns_none() {
        let src = "\
## See [nonexistent] for details.
proc foo {} { puts hi }
";
        let parsed = parse(src);
        let pos = first(src, "[nonexistent]") + 1;
        assert!(definition_at(&parsed.document, src, pos).is_none());
    }

    /// Cursor on prose inside a doc comment (not on a `[…]` token)
    /// falls through to structural resolution. Sanity check that the
    /// doc-ref path doesn't intercept every cursor position in a `##`.
    #[test]
    fn doc_prose_falls_through() {
        let src = "\
## Just prose, no refs here at all.
proc target {} { puts hi }
";
        let parsed = parse(src);
        // Cursor on the `p` of "prose" — inside the block but not
        // inside a `[…]`.
        let pos = first(src, "prose");
        assert!(definition_at(&parsed.document, src, pos).is_none());
    }

    /// Goto on a return-type annotation resolves to the `type` decl.
    #[test]
    fn goto_on_return_type_finds_type_decl() {
        let src = "\
type MyProps = string
proc use {name} MyProps { return $name }
";
        let parsed = parse(src);
        // Cursor on `MyProps` in the return-type slot (2nd occurrence).
        let pos = nth(src, "MyProps", 1);
        let target = definition_at(&parsed.document, src, pos).unwrap();
        let expected = parsed
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Command(c) => match &c.kind {
                    CommandKind::TypeDecl(td)
                        if td.name.as_deref() == Some("MyProps") =>
                    {
                        Some(td.name_span)
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(target, expected);
    }

    /// Goto on a qualified type name (`dcmac::MacPortProps`) resolves
    /// to the declaration when the type_table key matches.
    #[test]
    fn goto_on_qualified_type_finds_decl() {
        let src = "\
namespace eval dcmac {}
namespace eval dcmac::T {}
type dcmac::T = string
proc dcmac::T::from {v: string} dcmac::T { return $v }
proc dcmac::T::to {v: dcmac::T} string { return $v }
proc dcmac::T::repr {v: dcmac::T} string { return $v }
proc use {port0: dcmac::T} string { return $port0 }
";
        let parsed = parse(src);
        // Cursor on `T` inside `port0: dcmac::T` (the last occurrence).
        let pos = src.rfind("dcmac::T").unwrap() as u32 + 7; // land on `T`
        let target = definition_at(&parsed.document, src, pos).unwrap();
        let expected = parsed
            .document
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Command(c) => match &c.kind {
                    CommandKind::TypeDecl(td)
                        if td.name.as_deref() == Some("dcmac::T") =>
                    {
                        Some(td.name_span)
                    }
                    _ => None,
                },
                _ => None,
            })
            .unwrap();
        assert_eq!(target, expected);
    }
}
