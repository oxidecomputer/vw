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

use crate::ast::Document;
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
    if !is_valid_tcl_ident_or_qualified(new_name) {
        return None;
    }
    // Route through the [`crate::references`] core so proc /
    // type / enum-variant renames flow through the same
    // identify + collect pipeline as `textDocument/references`.
    // Locals + proc args are still file-local; the LSP layer
    // decides whether to also scan sibling files.
    let target = crate::references::identify_at(document, source, offset)?;
    let spans =
        crate::references::find_references_in(document, source, &target);
    if spans.is_empty() {
        return None;
    }
    let replacement = replacement_for(&target, new_name);
    let edits = spans
        .into_iter()
        .map(|span| RenameEdit {
            span,
            new_text: replacement.clone(),
        })
        .collect();
    Some(finalize_edits(edits))
}

/// Pick the exact text to substitute at each ref span for a
/// given target. Straightforward for locals / proc args / type
/// names (the new bare name goes in verbatim). Procs preserve
/// their namespace prefix so a rename of a call site like
/// `vivado_cmd::create_bd_cell` targets the LAST segment only
/// when the user typed a bare name.
fn replacement_for(
    target: &crate::references::ReferenceTarget,
    new_name: &str,
) -> String {
    use crate::references::ReferenceTarget;
    match target {
        ReferenceTarget::Proc { name }
            if name.contains("::") && !new_name.contains("::") =>
        {
            // Preserve the namespace prefix from the original.
            let ns_prefix =
                name.rsplit_once("::").map(|(ns, _)| ns).unwrap_or("");
            format!("{ns_prefix}::{new_name}")
        }
        ReferenceTarget::EnumVariant { enum_name, .. } => {
            // Enum variant refs span the whole `Enum::Variant`
            // form in call-head/qualified positions, so we
            // preserve the enum prefix.
            format!("{enum_name}::{new_name}")
        }
        _ => new_name.to_string(),
    }
}

/// A rename target's new text is either a plain identifier or a
/// fully qualified path (`ns::name`). Reject anything that
/// doesn't parse as one of those so the substituted text stays
/// valid htcl.
fn is_valid_tcl_ident_or_qualified(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    for seg in s.split("::") {
        if seg.is_empty() {
            return false;
        }
        if !is_valid_tcl_ident(seg) {
            return false;
        }
    }
    true
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
        // Bad syntax for a Tcl identifier still gets refused. The
        // qualified-name `ns::var` form is now accepted (proc /
        // enum-variant renames need to write it).
        let src = "proc f {} { set x 1; puts $x }\n";
        let pos = at(src, "set x", 0) + 4;
        let parsed = parse(src);
        assert!(rename_at(&parsed.document, src, pos, "").is_none());
        assert!(rename_at(&parsed.document, src, pos, "1foo").is_none());
        assert!(rename_at(&parsed.document, src, pos, "foo-bar").is_none());
    }

    #[test]
    fn rename_proc_from_decl_covers_call_sites() {
        // Cursor on the proc name at its decl → both the decl and
        // every call to `greet` in the same file get rewritten.
        let src = "\
proc greet {} { puts hi }
greet
proc other {} { greet }
";
        let pos = at(src, "greet", 0);
        let edits = edits_of(src, pos, "hello");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("proc hello {}"), "{renamed}");
        assert_eq!(renamed.matches("hello").count(), 3, "{renamed}");
    }

    #[test]
    fn rename_proc_from_call_site() {
        // Cursor on a call site → same set of edits as from the
        // decl; the identify pass just picks the same target.
        let src = "\
proc greet {} { puts hi }
greet
";
        let pos = at(src, "greet", 1);
        let edits = edits_of(src, pos, "hello");
        let renamed = apply(src, &edits);
        assert!(renamed.contains("proc hello {}"), "{renamed}");
        assert!(!renamed.contains("greet"), "{renamed}");
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
