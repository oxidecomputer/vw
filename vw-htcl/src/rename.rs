// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Local-scope rename.
//!
//! `textDocument/rename` for identifiers whose scope is confined to
//! the current file. Explicitly in scope:
//!
//! - proc parameters (rename the decl in the signature, every `$name`
//!   in the body, and any attribute-ident value that names the arg —
//!   e.g. `@requires(name)`)
//! - `set NAME value` locals
//! - `variable NAME` locals
//! - `foreach ITER $list { … }` iterators
//! - `upvar [LEVEL] remote LOCAL` locals
//!
//! Explicitly OUT of scope (returns `None`):
//!
//! - proc names, type names, enum names — renaming these would break
//!   call sites we can't see from a single file, so refuse rather
//!   than emit an incomplete edit set.
//! - proc-arg **flag references** at call sites (`caller -oldname …`) —
//!   same reason: cross-file. The user renaming a proc arg only gets
//!   the body-local rename; call sites keep the old flag name until
//!   they're touched manually.
//!
//! The cursor is allowed on any of: the decl itself, a `$name`
//! reference to it, or an attribute-ident value referring to a
//! sibling arg. Each maps to the same rename operation.

use crate::ast::{
    AttributeValue, Command, CommandKind, Document, Proc, ProcSignature, Stmt,
    Word, WordForm, WordPart,
};
use crate::hover::is_body_host;
use crate::scope::{resolve_var_def, scan_var_ref, VarDef};
use crate::span::Span;

/// A single text-substitution edit to apply. Spans are absolute in
/// the source we were given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameEdit {
    pub span: Span,
    pub new_text: String,
}

/// Compute the edits needed to rename the identifier at `offset` to
/// `new_name`. Returns `None` when:
///
/// - `new_name` isn't a valid Tcl identifier
/// - The cursor isn't on something we know how to rename locally
/// - The cursor is on a construct whose rename would leak beyond the
///   current file (proc names, types, etc.)
///
/// The returned edits are sorted by span start and deduplicated so
/// clients can apply them as-is.
pub fn rename_at(
    document: &Document,
    source: &str,
    offset: u32,
    new_name: &str,
) -> Option<Vec<RenameEdit>> {
    if !is_valid_tcl_ident(new_name) {
        return None;
    }
    // Try proc-arg rename first. Its identification is narrower
    // (only fires when the cursor lands on a signature arg / an
    // attribute-ident naming an arg / a `$var` resolving to an
    // arg), so it can't misclassify a local as a proc arg.
    let edits = rename_proc_arg(document, source, offset, new_name)
        .or_else(|| rename_local(document, source, offset, new_name))?;
    Some(finalize_edits(edits))
}

/// Tcl identifiers accept letters, digits, underscore, and `::`
/// (namespace separator). For rename we only allow the first three:
/// renaming across a namespace boundary changes visibility rules,
/// which is outside "local rename" semantics.
fn is_valid_tcl_ident(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut bytes = s.bytes();
    let first = bytes.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn finalize_edits(mut edits: Vec<RenameEdit>) -> Vec<RenameEdit> {
    edits.sort_by_key(|e| (e.span.start, e.span.end));
    edits.dedup_by(|a, b| a.span == b.span && a.new_text == b.new_text);
    edits
}

// ─── proc-arg rename ────────────────────────────────────────────────

/// Attempt to rename a proc parameter. Fires when the cursor is on:
///
/// - the arg's `name_span` in the signature
/// - an attribute-ident value inside the signature that names the arg
/// - a `$name` reference in the body that resolves to the arg
///
/// Emits: the signature-decl span, every attribute-ident span naming
/// the arg, and every use site inside the body.
fn rename_proc_arg(
    document: &Document,
    source: &str,
    offset: u32,
    new_name: &str,
) -> Option<Vec<RenameEdit>> {
    let (proc, arg_name) = find_proc_arg_at(document, source, offset)?;
    let sig = proc.signature.as_ref()?;
    let arg = sig.args.iter().find(|a| a.name == arg_name)?;

    let mut edits = Vec::new();
    edits.push(RenameEdit {
        span: arg.name_span,
        new_text: new_name.to_string(),
    });
    for attr_edit in attribute_ident_edits(sig, &arg_name, new_name) {
        edits.push(attr_edit);
    }
    collect_var_ref_edits(&proc.body, source, &arg_name, new_name, &mut edits);
    Some(edits)
}

/// If `offset` lands anywhere that identifies a proc arg, return the
/// enclosing proc plus the arg's name.
fn find_proc_arg_at<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
) -> Option<(&'a Proc, String)> {
    find_proc_arg_in(&document.stmts, source, offset)
}

fn find_proc_arg_in<'a>(
    stmts: &'a [Stmt],
    source: &str,
    offset: u32,
) -> Option<(&'a Proc, String)> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if let CommandKind::Proc(proc) = &cmd.kind {
            // Cursor on a signature arg name?
            if let Some(sig) = proc.signature.as_ref() {
                for arg in &sig.args {
                    if arg.name_span.contains(offset) {
                        return Some((proc, arg.name.clone()));
                    }
                    // Cursor on an attribute-ident naming a sibling arg?
                    for attr in &arg.attributes {
                        for value in &attr.values {
                            if let AttributeValue::Ident { value: name, span } =
                                value
                            {
                                if !span.contains(offset) {
                                    continue;
                                }
                                if sig.args.iter().any(|a| &a.name == name) {
                                    return Some((proc, name.clone()));
                                }
                            }
                        }
                    }
                }
            }
            // Cursor inside the body — resolve a var ref.
            if proc.body_span.contains(offset) {
                if let Some(name) =
                    var_ref_name_in_scope(&proc.body, source, offset)
                {
                    // Ensure the resolution lands on a proc arg (not a
                    // body-local `set`).
                    if let Some(VarDef::Param(_)) =
                        resolve_var_def(&name, &proc.body, Some(proc), offset)
                    {
                        return Some((proc, name));
                    }
                }
                // Recurse into nested procs.
                if let Some(hit) = find_proc_arg_in(&proc.body, source, offset)
                {
                    return Some(hit);
                }
            }
            return None;
        }
        if let CommandKind::NamespaceEval(ns) = &cmd.kind {
            if let Some(hit) = find_proc_arg_in(&ns.body, source, offset) {
                return Some(hit);
            }
        }
    }
    None
}

/// Every attribute-ident value across `sig` whose text is `old_name`,
/// as a rename edit to `new_name`.
fn attribute_ident_edits(
    sig: &ProcSignature,
    old_name: &str,
    new_name: &str,
) -> Vec<RenameEdit> {
    let mut out = Vec::new();
    for arg in &sig.args {
        for attr in &arg.attributes {
            for value in &attr.values {
                if let AttributeValue::Ident { value: name, span } = value {
                    if name == old_name {
                        out.push(RenameEdit {
                            span: *span,
                            new_text: new_name.to_string(),
                        });
                    }
                }
            }
        }
    }
    out
}

// ─── local rename ───────────────────────────────────────────────────

/// Attempt to rename a `set` / `variable` / `foreach` / `upvar`
/// local. Fires when the cursor is on:
///
/// - the target name of the decl command
/// - a `$name` reference resolving to a local (not a proc arg)
fn rename_local(
    document: &Document,
    source: &str,
    offset: u32,
    new_name: &str,
) -> Option<Vec<RenameEdit>> {
    let (scope_stmts, enclosing) = innermost_scope_full(document, offset);
    // 1. Cursor on a decl target?
    if let Some(name) =
        local_decl_name_at(scope_stmts, offset).map(|s| s.to_string())
    {
        let mut edits = Vec::new();
        collect_local_decl_edits(scope_stmts, &name, new_name, &mut edits);
        collect_var_ref_edits(scope_stmts, source, &name, new_name, &mut edits);
        // Foreach's iter can shadow an outer name, so filter out any
        // ref edits that fell inside an inner proc body — those are
        // separate scopes and we shouldn't touch them.
        if edits.is_empty() {
            return None;
        }
        return Some(edits);
    }
    // 2. Cursor on a `$var` that resolves to a local?
    let name = var_ref_name_in_scope(scope_stmts, source, offset)?;
    match resolve_var_def(&name, scope_stmts, enclosing, offset)? {
        VarDef::Local(_) => {}
        // Proc args are handled by `rename_proc_arg`.
        VarDef::Param(_) => return None,
    }
    let mut edits = Vec::new();
    collect_local_decl_edits(scope_stmts, &name, new_name, &mut edits);
    collect_var_ref_edits(scope_stmts, source, &name, new_name, &mut edits);
    if edits.is_empty() {
        return None;
    }
    Some(edits)
}

/// If `offset` lands on the target-name word of a `set`, `variable`,
/// `foreach` iter (bare form only), or `upvar` local, return that
/// name. Multi-var `foreach {a b c}` and the interior of upvar with
/// dynamic pairs are not supported for the "cursor is on a decl"
/// path — the cursor can only be on a bare-word decl. Users who need
/// to rename inside a brace-list `foreach` still get it via `$name`
/// from within the body.
fn local_decl_name_at(stmts: &[Stmt], offset: u32) -> Option<&str> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if !cmd.span.contains(offset) {
            continue;
        }
        if let Some(name) = decl_name_in_command(cmd, offset) {
            return Some(name);
        }
    }
    None
}

fn decl_name_in_command(cmd: &Command, offset: u32) -> Option<&str> {
    match &cmd.kind {
        CommandKind::Set => {
            // 2-word `set foo` is a read, not a decl.
            if cmd.words.len() < 3 {
                return None;
            }
            let target = cmd.words.get(1)?;
            if target.span.contains(offset) {
                return target.as_text();
            }
            None
        }
        CommandKind::Generic => {
            let head = cmd.words.first()?.as_text()?;
            match head {
                "variable" => {
                    let target = cmd.words.get(1)?;
                    target.span.contains(offset).then(|| target.as_text())?
                }
                "foreach" => {
                    // `foreach ITER LIST BODY` (4 words) — cursor
                    // on ITER position.
                    if cmd.words.len() < 4 {
                        return None;
                    }
                    // Every even-indexed word (skipping the leading
                    // `foreach`) up to body_idx-1 is an iter.
                    let body_idx = cmd.words.len() - 1;
                    let mut i = 1;
                    while i < body_idx {
                        let target = &cmd.words[i];
                        if target.span.contains(offset) {
                            return target.as_text();
                        }
                        i += 2;
                    }
                    None
                }
                "upvar" => {
                    // `upvar [LEVEL] remote local [remote local]…`
                    let mut idx = 1;
                    if let Some(w) = cmd.words.get(idx) {
                        if let Some(t) = w.as_text() {
                            if t.starts_with('#')
                                || t.chars()
                                    .next()
                                    .is_some_and(|c| c.is_ascii_digit())
                            {
                                idx += 1;
                            }
                        }
                    }
                    while idx + 1 < cmd.words.len() {
                        let local_word = &cmd.words[idx + 1];
                        if local_word.span.contains(offset) {
                            return local_word.as_text();
                        }
                        idx += 2;
                    }
                    None
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Push edits for every `set NAME …` / `variable NAME` / `foreach
/// NAME …` / `upvar … NAME` decl in `stmts` whose target text is
/// `old_name`.
fn collect_local_decl_edits(
    stmts: &[Stmt],
    old_name: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        push_decl_edit_if_matches(cmd, old_name, new_name, edits);
    }
}

fn push_decl_edit_if_matches(
    cmd: &Command,
    old_name: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    match &cmd.kind {
        CommandKind::Set => {
            if cmd.words.len() < 3 {
                return;
            }
            let target = &cmd.words[1];
            if target.as_text() == Some(old_name) {
                edits.push(RenameEdit {
                    span: target.span,
                    new_text: new_name.to_string(),
                });
            }
        }
        CommandKind::Generic => {
            let Some(head) = cmd.words.first().and_then(Word::as_text) else {
                return;
            };
            match head {
                "variable" => {
                    if let Some(target) = cmd.words.get(1) {
                        if target.as_text() == Some(old_name) {
                            edits.push(RenameEdit {
                                span: target.span,
                                new_text: new_name.to_string(),
                            });
                        }
                    }
                }
                "foreach" => {
                    if cmd.words.len() < 4 {
                        return;
                    }
                    let body_idx = cmd.words.len() - 1;
                    let mut i = 1;
                    while i < body_idx {
                        let target = &cmd.words[i];
                        if target.as_text() == Some(old_name) {
                            edits.push(RenameEdit {
                                span: target.span,
                                new_text: new_name.to_string(),
                            });
                        }
                        i += 2;
                    }
                }
                "upvar" => {
                    let mut idx = 1;
                    if let Some(w) = cmd.words.get(idx) {
                        if let Some(t) = w.as_text() {
                            if t.starts_with('#')
                                || t.chars()
                                    .next()
                                    .is_some_and(|c| c.is_ascii_digit())
                            {
                                idx += 1;
                            }
                        }
                    }
                    while idx + 1 < cmd.words.len() {
                        let local_word = &cmd.words[idx + 1];
                        if local_word.as_text() == Some(old_name) {
                            edits.push(RenameEdit {
                                span: local_word.span,
                                new_text: new_name.to_string(),
                            });
                        }
                        idx += 2;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Return the innermost proc's body plus that proc (or the document
/// plus `None` at the top level). Same shape as
/// [`crate::scope::innermost_scope`] but returned owned so callers
/// can decide the scope kind without another lookup.
fn innermost_scope_full(
    document: &Document,
    offset: u32,
) -> (&[Stmt], Option<&Proc>) {
    crate::scope::innermost_scope(document, offset)
}

// ─── shared use-site collector ─────────────────────────────────────

/// Walk `stmts` and every same-frame nested scope (body-host brace
/// bodies, `[ … ]` substitutions), pushing a rename edit for every
/// `$name` reference whose name matches `old_name`. **Does not**
/// descend into nested `proc` bodies or `namespace eval` blocks —
/// those introduce a fresh scope where the same name is unrelated.
fn collect_var_ref_edits(
    stmts: &[Stmt],
    source: &str,
    old_name: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // Skip nested-scope commands entirely — collect_var_ref_edits
        // is meant to walk one frame's worth of code.
        if matches!(
            &cmd.kind,
            CommandKind::Proc(_) | CommandKind::NamespaceEval(_)
        ) {
            continue;
        }
        collect_var_ref_edits_in_command(
            cmd, source, old_name, new_name, edits,
        );
    }
}

fn collect_var_ref_edits_in_command(
    cmd: &Command,
    source: &str,
    old_name: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    for word in &cmd.words {
        collect_var_ref_edits_in_word(word, source, old_name, new_name, edits);
    }
    // Body-host commands (if/while/foreach/…) carry brace-word
    // scripts that run in the same frame. Reparse each and walk it
    // like part of the current scope.
    if let Some(head) = cmd.words.first().and_then(Word::as_text) {
        if is_body_host(head) {
            for word in cmd.words.iter().skip(1) {
                if let Some(stmts) = reparse_braced_body(word, source) {
                    collect_var_ref_edits(
                        &stmts, source, old_name, new_name, edits,
                    );
                }
            }
        }
    }
}

fn collect_var_ref_edits_in_word(
    word: &Word,
    source: &str,
    old_name: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    for part in &word.parts {
        match part {
            WordPart::VarRef { name, span } => {
                // Only rename plain-name refs — `${arr(key)}` or
                // `${ns::var}` is out of scope for local rename
                // (namespaces cross scope boundaries; array cells
                // are the array's, not a separate identifier).
                if name == old_name {
                    push_var_ref_edit(*span, source, new_name, edits);
                }
            }
            WordPart::CmdSubst { body, .. } => {
                // Nested command substitution runs in the current
                // frame — its VarRefs count.
                collect_var_ref_edits(body, source, old_name, new_name, edits);
            }
            WordPart::Text { .. } | WordPart::Escape { .. } => {}
        }
    }
}

/// Emit a rename edit that replaces just the NAME portion of a
/// `$name` / `${name}` reference. The VarRef span covers the whole
/// reference including `$` (and, for the braced form, `${` … `}`);
/// we clip to the identifier byte range so we don't accidentally
/// drop the sigils.
fn push_var_ref_edit(
    ref_span: Span,
    source: &str,
    new_name: &str,
    edits: &mut Vec<RenameEdit>,
) {
    let bytes = source.as_bytes();
    let start = ref_span.start as usize;
    let end = ref_span.end as usize;
    if end <= start || end > bytes.len() {
        return;
    }
    // Determine `${…}` vs `$…` by peeking the second byte.
    let (name_start, name_end) = if start + 1 < end && bytes[start + 1] == b'{'
    {
        // `${…}` — identifier lives between `${` and the closing `}`.
        let s = start + 2;
        let e = end.saturating_sub(1);
        if e <= s {
            return;
        }
        (s, e)
    } else {
        // `$…` — identifier lives between `$` and end of span.
        let s = start + 1;
        if end <= s {
            return;
        }
        (s, end)
    };
    edits.push(RenameEdit {
        span: Span::new(name_start as u32, name_end as u32),
        new_text: new_name.to_string(),
    });
}

/// If `word` is a braced body-host arg, reparse its interior into
/// statements. Mirror of `unused::reparse_braced_body` — kept as its
/// own helper here so the modules don't tangle their pub-crate
/// surface.
fn reparse_braced_body(word: &Word, source: &str) -> Option<Vec<Stmt>> {
    if word.form != WordForm::Braced {
        return None;
    }
    let WordPart::Text { value, span } = word.parts.first()? else {
        return None;
    };
    let (mut stmts, mut errs) = crate::parser::parse_fragment(
        value.as_str(),
        crate::parser::Mode::BracketBody,
    );
    let delta = span.start;
    for s in &mut stmts {
        crate::parser::shift_stmt(s, delta);
    }
    crate::parser::populate_procs(&mut stmts, source, &mut errs);
    Some(stmts)
}

/// Recover the name of a `$var` at `offset`, whether it's a
/// structured [`WordPart::VarRef`] we can see or one buried inside
/// opaque text. Returns the bare identifier only.
fn var_ref_name_in_scope(
    _stmts: &[Stmt],
    source: &str,
    offset: u32,
) -> Option<String> {
    scan_var_ref(source, offset).map(|(name, _)| name)
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
                .expect("needle not found enough times");
            if i == occurrence {
                return pos as u32;
            }
            start = pos + needle.len();
        }
        unreachable!()
    }

    /// Apply edits and return the resulting source.
    fn apply(src: &str, edits: &[RenameEdit]) -> String {
        let mut out = src.to_string();
        for edit in edits.iter().rev() {
            let s = edit.span.start as usize;
            let e = edit.span.end as usize;
            out.replace_range(s..e, &edit.new_text);
        }
        out
    }

    fn edits_of(src: &str, pos: u32, new_name: &str) -> Vec<RenameEdit> {
        let parsed = parse(src);
        rename_at(&parsed.document, src, pos, new_name).unwrap_or_default()
    }

    #[test]
    fn rename_set_local_from_decl_position() {
        let src = "\
proc f {} {
  set mode fast
  puts $mode
  return $mode
}
";
        let pos = at(src, "mode", 0); // the `mode` in `set mode`
        let edits = edits_of(src, pos, "kind");
        assert!(!edits.is_empty(), "no edits produced");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("set kind fast"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
        assert!(renamed.contains("return $kind"), "{renamed}");
        assert!(!renamed.contains("mode"), "{renamed}");
    }

    #[test]
    fn rename_set_local_from_var_ref_position() {
        let src = "\
proc f {} {
  set mode fast
  puts $mode
}
";
        // Cursor on the `m` inside `$mode`.
        let pos = at(src, "$mode", 0) + 1;
        let edits = edits_of(src, pos, "kind");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("set kind fast"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
    }

    #[test]
    fn rename_proc_arg_from_decl() {
        let src = "\
proc f {
  mode
} {
  puts $mode
  return $mode
}
";
        let pos = at(src, "mode", 0); // arg decl
        let edits = edits_of(src, pos, "kind");
        let renamed = apply(src, &edits);
        // Arg decl updated + body refs updated.
        assert!(renamed.contains("  kind"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
        assert!(renamed.contains("return $kind"), "{renamed}");
    }

    #[test]
    fn rename_proc_arg_from_body_var_ref() {
        let src = "\
proc f {
  mode
} {
  puts $mode
}
";
        let pos = at(src, "$mode", 0) + 1;
        let edits = edits_of(src, pos, "kind");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("  kind"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
    }

    #[test]
    fn rename_proc_arg_updates_attribute_ident() {
        // `@requires(has_a)` in the sig references the sibling arg
        // by name; renaming `has_a` should update that reference.
        let src = "\
proc f {
  has_a
  @requires(has_a) has_b
} {
  puts $has_a
}
";
        let pos = at(src, "has_a", 0); // decl of has_a
        let edits = edits_of(src, pos, "has_x");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("  has_x\n"), "{renamed}");
        assert!(renamed.contains("@requires(has_x)"), "{renamed}");
        assert!(renamed.contains("puts $has_x"), "{renamed}");
    }

    #[test]
    fn rename_foreach_iterator() {
        let src = "\
proc f {} {
  foreach item [list 1 2 3] {
    puts $item
  }
}
";
        let pos = at(src, "item", 0); // cursor on iter decl
        let edits = edits_of(src, pos, "elem");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("foreach elem "), "{renamed}");
        assert!(renamed.contains("puts $elem"), "{renamed}");
    }

    #[test]
    fn rename_upvar_local() {
        let src = "\
proc f {} {
  upvar 1 remote local
  puts $local
}
";
        let pos = at(src, "local", 0); // upvar's local half
        let edits = edits_of(src, pos, "here");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("upvar 1 remote here"), "{renamed}");
        assert!(renamed.contains("puts $here"), "{renamed}");
    }

    #[test]
    fn rename_covers_uses_inside_if_body() {
        let src = "\
proc f {} {
  set mode fast
  if {$mode == fast} {
    puts $mode
  }
}
";
        let pos = at(src, "mode", 0);
        let edits = edits_of(src, pos, "kind");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("set kind fast"), "{renamed}");
        // Both the condition and the body refs get updated via the
        // brace-body reparse.
        assert!(renamed.contains("if {$kind == fast}"), "{renamed}");
        assert!(renamed.contains("puts $kind"), "{renamed}");
    }

    #[test]
    fn rename_does_not_leak_into_nested_proc_scope() {
        // Outer `set foo` and inner `proc g { foo }` share a name
        // but are unrelated scopes. Renaming the outer must not
        // touch the inner.
        let src = "\
proc outer {} {
  set foo 1
  proc g {foo} {
    puts $foo
  }
  puts $foo
}
";
        let pos = at(src, "foo", 0); // outer set decl
        let edits = edits_of(src, pos, "bar");
        let renamed = apply(src, &edits);
        // Outer decl + outer use renamed.
        assert!(renamed.contains("set bar 1"), "{renamed}");
        // Inner proc's arg and its body ref stay `foo`.
        assert!(renamed.contains("proc g {foo}"), "{renamed}");
        assert!(renamed.contains("puts $foo\n  }"), "{renamed}");
    }

    #[test]
    fn refuse_invalid_new_name() {
        let src = "proc f {} { set x 1; puts $x }\n";
        let pos = at(src, "set x", 0) + 4;
        let parsed = parse(src);
        assert!(rename_at(&parsed.document, src, pos, "").is_none());
        assert!(rename_at(&parsed.document, src, pos, "1foo").is_none());
        assert!(rename_at(&parsed.document, src, pos, "foo-bar").is_none());
        assert!(rename_at(&parsed.document, src, pos, "ns::var").is_none());
    }

    #[test]
    fn refuse_when_cursor_on_proc_name() {
        let src = "\
proc greet {} { puts hi }
greet
";
        // Cursor on `greet` (the proc name decl).
        let pos = at(src, "greet", 0);
        let parsed = parse(src);
        assert!(rename_at(&parsed.document, src, pos, "hello").is_none());
    }

    #[test]
    fn refuse_when_cursor_on_call_site() {
        // Same as above but at the call site. We don't rename procs.
        let src = "\
proc greet {} { puts hi }
greet
";
        let pos = at(src, "greet", 1);
        let parsed = parse(src);
        assert!(rename_at(&parsed.document, src, pos, "hello").is_none());
    }

    #[test]
    fn rename_top_level_set() {
        let src = "\
set root /tmp
puts $root
";
        let pos = at(src, "root", 0);
        let edits = edits_of(src, pos, "dir");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("set dir /tmp"), "{renamed}");
        assert!(renamed.contains("puts $dir"), "{renamed}");
    }

    #[test]
    fn rename_from_cursor_on_dollar_sign() {
        // Cursor on the `$` itself, not the letter after.
        let src = "\
proc f {} {
  set x 1
  puts $x
}
";
        let pos = at(src, "$x", 0);
        let edits = edits_of(src, pos, "y");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("set y 1"), "{renamed}");
        assert!(renamed.contains("puts $y"), "{renamed}");
    }

    #[test]
    fn is_valid_ident_smoke() {
        assert!(is_valid_tcl_ident("foo"));
        assert!(is_valid_tcl_ident("_foo"));
        assert!(is_valid_tcl_ident("foo_bar"));
        assert!(is_valid_tcl_ident("f1"));
        assert!(!is_valid_tcl_ident(""));
        assert!(!is_valid_tcl_ident("1foo"));
        assert!(!is_valid_tcl_ident("foo-bar"));
        assert!(!is_valid_tcl_ident("foo bar"));
        assert!(!is_valid_tcl_ident("foo::bar"));
    }
}
