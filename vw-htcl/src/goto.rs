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
    ProcSignature, Stmt, WordPart,
};
use crate::lower::{signature_table, SignatureTable};
use crate::scope::{innermost_scope, resolve_var_def, scan_var_ref};
use crate::span::Span;

pub fn definition_at(
    document: &Document,
    source: &str,
    offset: u32,
) -> Option<Span> {
    let table = signature_table(document);
    definition_in_stmts(&document.stmts, None, document, &table, offset)
        // Fallback: a `$var` the structured tree keeps opaque — inside
        // a command substitution or an `if`/`while` condition. Found by
        // scanning the source and resolving against the enclosing
        // proc's scope.
        .or_else(|| definition_of_scanned_var(document, source, offset))
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
                offset,
            );
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
            definition_in_cmd_substs(cmd, document, table, offset)
        {
            return Some(span);
        }
    }
    None
}

fn definition_in_cmd_substs<'a>(
    cmd: &'a Command,
    document: &'a Document,
    table: &SignatureTable<'a>,
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
                        body, None, document, table, offset,
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
            if let WordPart::VarRef { name, span } = part {
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

fn find_proc_decl<'a>(document: &'a Document, name: &str) -> Option<&'a Proc> {
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(proc) = &cmd.kind else {
            continue;
        };
        if proc.name.as_deref() == Some(name) {
            return Some(proc);
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
}
