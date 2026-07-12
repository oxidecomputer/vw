// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Variable scope resolution shared by goto and hover.
//!
//! Tcl variables are local to a proc (its parameters plus whatever it
//! `set`s / `variable`s); top-level code shares the global scope. That
//! lexical model is enough to point a `$name` reference at its
//! definition.
//!
//! Two entry styles:
//!
//! - [`resolve_var_def`] resolves a name given a known scope — used by
//!   the structured path when the reference is a real [`WordPart::VarRef`].
//! - [`scan_var_ref`] + [`innermost_scope`] recover a reference the
//!   structured parser left buried in opaque text (a command
//!   substitution body, or an `if`/`while` condition), by reading the
//!   raw source at the cursor and locating the enclosing proc by span.

use crate::ast::{
    Command, CommandKind, Document, Proc, ProcArg, Stmt, TypeDecl, TypeExpr,
};
use crate::span::Span;

/// What a `$name` reference resolves to.
#[derive(Clone, Copy, Debug)]
pub enum VarDef<'a> {
    /// A parameter of the enclosing proc.
    Param(&'a ProcArg),
    /// A local established by `set name ...` or `variable name ...`.
    /// Carries the span of the defined name.
    Local(Span),
}

impl VarDef<'_> {
    /// The span to navigate to / anchor hover on.
    pub fn def_span(&self) -> Span {
        match self {
            VarDef::Param(arg) => arg.name_span,
            VarDef::Local(span) => *span,
        }
    }
}

/// Resolve `name` within `scope_stmts` (the statements of the current
/// scope), falling back to a parameter of `enclosing`. `offset` biases
/// local resolution toward the last definition at or before the
/// reference (the value in effect there).
pub fn resolve_var_def<'a>(
    name: &str,
    scope_stmts: &'a [Stmt],
    enclosing: Option<&'a Proc>,
    offset: u32,
) -> Option<VarDef<'a>> {
    let mut best: Option<Span> = None;
    let mut first: Option<Span> = None;
    for stmt in scope_stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let Some(def) = local_def_target(cmd, name) else {
            continue;
        };
        first.get_or_insert(def);
        if def.start <= offset {
            best = Some(def);
        }
    }
    if let Some(span) = best.or(first) {
        return Some(VarDef::Local(span));
    }

    let sig = enclosing?.signature.as_ref()?;
    sig.args.iter().find(|a| a.name == name).map(VarDef::Param)
}

/// If `cmd` defines variable `name` — via `set NAME …` / `variable
/// NAME …` / `foreach NAME …` / `foreach {A B …} …` / `dict for {K
/// V} …` / `catch BODY NAME` — return the span to anchor
/// hover/goto on. Braced varname-list positions (`foreach {a b}`,
/// `dict for {k v}`) return the span of the whole braced word;
/// sub-token spans would need extra parser wiring.
fn local_def_target(cmd: &Command, name: &str) -> Option<Span> {
    match &cmd.kind {
        CommandKind::Set => {
            let target = cmd.words.get(1)?;
            (target.as_text()? == name).then_some(target.span)
        }
        CommandKind::Generic => {
            let head = cmd.words.first()?.as_text()?;
            match head {
                "variable" => {
                    let target = cmd.words.get(1)?;
                    (target.as_text()? == name).then_some(target.span)
                }
                "foreach" => {
                    // Iterator target(s) at word 1 (and every
                    // second word after, up to but not including
                    // the body). Matches `unused::collect_foreach_decls`.
                    let body_idx = cmd.words.len().saturating_sub(1);
                    let mut i = 1;
                    while i < body_idx {
                        if word_declares_name(&cmd.words[i], name) {
                            return Some(cmd.words[i].span);
                        }
                        i += 2;
                    }
                    None
                }
                "dict" => {
                    // `dict for {K V} DICT BODY` — the kv list is at
                    // word 2, only for the `for` sub-command.
                    if cmd.words.get(1)?.as_text()? != "for" {
                        return None;
                    }
                    let target = cmd.words.get(2)?;
                    if word_declares_name(target, name) {
                        Some(target.span)
                    } else {
                        None
                    }
                }
                "catch" => {
                    // `catch BODY [RESVAR [OPTVAR]]` — words 2/3.
                    for w in cmd.words.iter().skip(2).take(2) {
                        if word_declares_name(w, name) {
                            return Some(w.span);
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        CommandKind::Proc(_)
        | CommandKind::Src(_)
        | CommandKind::NamespaceEval(_)
        | CommandKind::TypeDecl(_)
        | CommandKind::EnumDecl(_) => None,
    }
}

/// True when `word` names `target` — either as a bare identifier
/// (`foreach x …`) or as a whitespace-separated token inside a
/// braced list (`foreach {a b} …`, `dict for {k v} …`).
fn word_declares_name(word: &crate::ast::Word, target: &str) -> bool {
    use crate::ast::{WordForm, WordPart};
    // Bare word: exact match.
    if word.form == WordForm::Bare {
        return word.as_text() == Some(target);
    }
    // Braced list: whitespace-split the interior Text.
    if word.form != WordForm::Braced {
        return false;
    }
    let Some(WordPart::Text { value, .. }) = word.parts.first() else {
        return false;
    };
    value.split_whitespace().any(|tok| tok == target)
}

/// The innermost proc whose body contains `offset`, together with that
/// body's statements. `(document.stmts, None)` when `offset` is at the
/// top level.
pub fn innermost_scope(
    document: &Document,
    offset: u32,
) -> (&[Stmt], Option<&Proc>) {
    fn helper(stmts: &[Stmt], offset: u32) -> Option<(&[Stmt], &Proc)> {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            let CommandKind::Proc(proc) = &cmd.kind else {
                continue;
            };
            if proc.body_span.contains(offset) {
                return Some(
                    helper(&proc.body, offset).unwrap_or((&proc.body, proc)),
                );
            }
        }
        None
    }
    match helper(&document.stmts, offset) {
        Some((stmts, proc)) => (stmts, Some(proc)),
        None => (&document.stmts, None),
    }
}

/// If the cursor at `offset` sits on a `$name` (or `${name}`)
/// reference — even one the structured parser left inside opaque text
/// (a command substitution, or an expr condition) — return its name
/// and the span of the whole reference.
pub fn scan_var_ref(source: &str, offset: u32) -> Option<(String, Span)> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let off = (offset as usize).min(len);
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b':';

    // If the cursor sits on the `$` itself, step into the name.
    let probe = if off < len && bytes[off] == b'$' {
        off + 1
    } else {
        off
    };
    let probe = probe.min(len);

    let mut start = probe;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = probe;
    while end < len && is_ident(bytes[end]) {
        end += 1;
    }
    if end <= start {
        return None;
    }

    // `$name`
    if start > 0 && bytes[start - 1] == b'$' {
        let name = source.get(start..end)?.to_string();
        return Some((name, Span::new((start - 1) as u32, end as u32)));
    }
    // `${name}`
    if start >= 2
        && bytes[start - 1] == b'{'
        && bytes[start - 2] == b'$'
        && end < len
        && bytes[end] == b'}'
    {
        let name = source.get(start..end)?.to_string();
        return Some((name, Span::new((start - 2) as u32, (end + 1) as u32)));
    }
    None
}

/// Walk `document` and return the innermost [`TypeExpr`] whose
/// span contains `offset`. Considers proc-signature arg
/// annotations, proc return-type annotations, `type … = TYPE`
/// underlying, and generic type arguments (recursively). Returns
/// `None` when the cursor isn't on a type-expression position.
pub fn type_expr_at(document: &Document, offset: u32) -> Option<&TypeExpr> {
    fn inner(ty: &TypeExpr, offset: u32) -> Option<&TypeExpr> {
        if !ty.span().contains(offset) {
            return None;
        }
        // Recurse into generic args first so the innermost match wins.
        if let TypeExpr::Generic { args, .. } = ty {
            for a in args {
                if let Some(hit) = inner(a, offset) {
                    return Some(hit);
                }
            }
        }
        Some(ty)
    }
    fn walk(stmts: &[Stmt], offset: u32) -> Option<&TypeExpr> {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            if !cmd.span.contains(offset) {
                continue;
            }
            match &cmd.kind {
                CommandKind::Proc(proc) => {
                    if let Some(sig) = &proc.signature {
                        for arg in &sig.args {
                            if let Some(ty) = arg.type_annotation.as_ref() {
                                if let Some(hit) = inner(ty, offset) {
                                    return Some(hit);
                                }
                            }
                        }
                        if let Some(ret) = sig.return_type.as_ref() {
                            if let Some(hit) = inner(ret, offset) {
                                return Some(hit);
                            }
                        }
                    }
                    if let Some(hit) = walk(&proc.body, offset) {
                        return Some(hit);
                    }
                }
                CommandKind::TypeDecl(td) => {
                    if let Some(ty) = td.underlying.as_ref() {
                        if let Some(hit) = inner(ty, offset) {
                            return Some(hit);
                        }
                    }
                }
                CommandKind::NamespaceEval(ns) => {
                    if let Some(hit) = walk(&ns.body, offset) {
                        return Some(hit);
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(&document.stmts, offset)
}

/// Find a top-level `type NAME = …` declaration whose name matches
/// `name`. Handles bare and qualified forms — a caller looking up
/// `dcmac::MacPortProps` and one looking up `Properties` both hit
/// the right decl since parser stores the raw textual name.
pub fn find_type_decl<'a>(
    document: &'a Document,
    name: &str,
) -> Option<&'a TypeDecl> {
    fn walk<'a>(stmts: &'a [Stmt], name: &str) -> Option<&'a TypeDecl> {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            match &cmd.kind {
                CommandKind::TypeDecl(td)
                    if td.name.as_deref() == Some(name) =>
                {
                    return Some(td);
                }
                CommandKind::NamespaceEval(ns) => {
                    if let Some(hit) = walk(&ns.body, name) {
                        return Some(hit);
                    }
                }
                CommandKind::Proc(proc) => {
                    // Types can also be declared inside a proc body
                    // in principle (though rare). Search anyway.
                    if let Some(hit) = walk(&proc.body, name) {
                        return Some(hit);
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(&document.stmts, name)
}

/// Extract the qualified/bare identifier a [`TypeExpr`] references,
/// suitable for [`find_type_decl`] lookup. Returns the joined
/// `"namespace::variant"` for [`TypeExpr::Qualified`], and the raw
/// `name` field for the other two shapes.
pub fn type_expr_lookup_name(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named { name, .. } | TypeExpr::Generic { name, .. } => {
            name.clone()
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => format!("{namespace}::{variant}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_bare_var_from_within() {
        let src = "puts $kind here";
        // cursor on the `i` of `$kind`
        let pos = (src.find("kind").unwrap() + 1) as u32;
        let (name, span) = scan_var_ref(src, pos).unwrap();
        assert_eq!(name, "kind");
        assert_eq!(span.slice(src), "$kind");
    }

    #[test]
    fn scan_finds_var_on_dollar() {
        let src = "x $y";
        let pos = src.find('$').unwrap() as u32;
        let (name, _) = scan_var_ref(src, pos).unwrap();
        assert_eq!(name, "y");
    }

    #[test]
    fn scan_finds_braced_var() {
        let src = "a ${foo} b";
        let pos = (src.find("foo").unwrap() + 1) as u32;
        let (name, span) = scan_var_ref(src, pos).unwrap();
        assert_eq!(name, "foo");
        assert_eq!(span.slice(src), "${foo}");
    }

    #[test]
    fn scan_returns_none_off_a_var() {
        let src = "plain text";
        assert!(scan_var_ref(src, 2).is_none());
    }
}
