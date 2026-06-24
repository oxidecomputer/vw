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

use crate::ast::{Command, CommandKind, Document, Proc, ProcArg, Stmt};
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

/// If `cmd` defines variable `name` (`set name ...` or `variable
/// name ...`), return the span of the defined name.
fn local_def_target(cmd: &Command, name: &str) -> Option<Span> {
    match &cmd.kind {
        CommandKind::Set => {
            let target = cmd.words.get(1)?;
            (target.as_text()? == name).then_some(target.span)
        }
        CommandKind::Generic => {
            if cmd.words.first()?.as_text()? != "variable" {
                return None;
            }
            let target = cmd.words.get(1)?;
            (target.as_text()? == name).then_some(target.span)
        }
        CommandKind::Proc(_)
        | CommandKind::Src(_)
        | CommandKind::NamespaceEval(_) => None,
    }
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
