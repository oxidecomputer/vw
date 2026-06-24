// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Lower htcl to plain Tcl for the EDA backend.
//!
//! Phase 2 lowering:
//!
//! - Structured `proc` declarations emit `proc name {arg1 arg2 ...}
//!   body`, where the arg list is the declared canonical order with
//!   no attributes (Vivado's Tcl doesn't understand `@default` etc.).
//! - Call sites to a known structured proc rewrite their `-flag
//!   value` form to a positional list in the canonical order, with
//!   defaults filled in for omitted args.
//! - Everything else (comments, unknown commands, calls to commands
//!   without a structured signature) passes through verbatim.
//!
//! Limitation: only top-level proc declarations and top-level call
//! sites are lowered. Calls *inside* a proc body are not rewritten —
//! the body text is shipped as-is. Phase 3+ will recursively lower
//! nested commands once we have static analysis of proc bodies.

use std::collections::HashMap;

use crate::ast::{
    Command, CommandKind, Document, NamespaceEval, Proc, ProcSignature, Stmt,
    Word, WordForm, WordPart,
};

pub type SignatureTable<'a> = HashMap<String, &'a ProcSignature>;

/// Walk `doc` and collect every proc's signature — top-level and
/// nested inside `namespace eval` blocks. Namespaced procs register
/// under their qualified name (`<ns>::<proc>`), matching the
/// signature table the validator builds so call-site lowering works
/// uniformly for both shapes.
pub fn signature_table(doc: &Document) -> SignatureTable<'_> {
    let mut table = HashMap::new();
    collect_into(&doc.stmts, "", &mut table);
    table
}

fn collect_into<'a>(
    stmts: &'a [Stmt],
    prefix: &str,
    table: &mut SignatureTable<'a>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(name) = proc.name.as_deref() else { continue };
                let Some(sig) = proc.signature.as_ref() else { continue };
                let qualified = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}::{name}")
                };
                table.insert(qualified, sig);
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else { continue };
                let nested = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}::{name}")
                };
                collect_into(&ns.body, &nested, table);
            }
            _ => {}
        }
    }
}

/// Lower one top-level command into its Tcl equivalent for the EDA
/// backend.
pub fn lower_command(
    cmd: &Command,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    match &cmd.kind {
        CommandKind::Proc(proc) => lower_proc_decl(proc, source),
        CommandKind::NamespaceEval(ns) => {
            lower_namespace_eval(ns, source, table)
        }
        // `src` is a module import; by the time we lower we expect the
        // [`crate::loader`] flatten pass to have already inlined every
        // import's contents and dropped the `src` statements. Anything
        // that slips through here we render as a no-op comment so the
        // emitted Tcl is still well-formed.
        CommandKind::Src(import) => {
            let path = import.path.as_deref().unwrap_or("<dynamic>");
            format!("# vw: unresolved `src {path}` — loader bypass")
        }
        _ => {
            let call_name = cmd.words.first().and_then(Word::as_text);
            if let Some(name) = call_name {
                if let Some(sig) = table.get(name) {
                    return lower_call(name, cmd, sig, source, table);
                }
            }
            // Verbatim, but reconstructed word-by-word so that any
            // `[ … ]` substitution inside the command gets its own
            // commands lowered through the same pipeline — keyword
            // → positional rewriting still applies to calls nested
            // inside a `set proj [ create_project … ]`, and multi-
            // line brackets collapse to one Tcl statement.
            lower_words(&cmd.words, source, table)
        }
    }
}

/// Lower a `namespace eval` block: recurse on each inner statement
/// (so inner proc declarations get their htcl attributes stripped
/// and inner calls get keyword→positional rewriting) and wrap the
/// result in `namespace eval <name> { ... }`. Output is a single
/// Tcl-valid string the EDA backend can `eval` directly.
fn lower_namespace_eval(
    ns: &NamespaceEval,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    let name = ns.name.as_deref().unwrap_or("");
    let mut body = String::new();
    for stmt in &ns.body {
        let Stmt::Command(cmd) = stmt else { continue };
        let line = lower_command(cmd, source, table);
        if !line.is_empty() {
            body.push_str(&line);
            body.push('\n');
        }
    }
    format!("namespace eval {name} {{\n{body}}}")
}

fn lower_proc_decl(proc: &Proc, source: &str) -> String {
    let name = proc.name.as_deref().unwrap_or("");
    let body = proc.body_span.slice(source);
    let args_list = match proc.signature.as_ref() {
        Some(sig) => sig
            .args
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(" "),
        None => proc.args_span.slice(source).to_string(),
    };
    format!("proc {name} {{{args_list}}} {{{body}}}")
}

fn lower_call(
    name: &str,
    cmd: &Command,
    sig: &ProcSignature,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    // Collect keyword args. Anything that doesn't look like a `-flag
    // value` pair is silently dropped here — the validator already
    // diagnosed it.
    let mut values: HashMap<String, String> = HashMap::new();
    let mut idx = 1usize;
    while idx < cmd.words.len() {
        let word = &cmd.words[idx];
        let flag_name = match word.as_text() {
            Some(t) if t.starts_with('-') => &t[1..],
            _ => {
                idx += 1;
                continue;
            }
        };
        let Some(value_word) = cmd.words.get(idx + 1) else {
            idx += 1;
            continue;
        };
        // Lower the value word through the same reconstruction the
        // verbatim path uses, so a value like `[create_project
        // -name foo]` gets its inner call rewritten too.
        let v = lower_word(value_word, source, table);
        values.insert(flag_name.to_string(), v);
        idx += 2;
    }

    let mut positional = Vec::with_capacity(sig.args.len());
    for arg in &sig.args {
        if let Some(v) = values.remove(&arg.name) {
            positional.push(v);
        } else if let Some(default) = arg.attribute("default") {
            let lit = default
                .values
                .first()
                .map(|v| v.to_tcl_literal())
                .unwrap_or_else(|| "{}".to_string());
            positional.push(lit);
        } else {
            // No value, no default. Validator should have flagged
            // this; emit an empty list so the Tcl proc at least gets
            // the right arity.
            positional.push("{}".to_string());
        }
    }
    format!("{name} {}", positional.join(" "))
}

/// The syntactic prefix that marks a call to a runtime-Tcl proc
/// (an "extern") rather than an htcl-defined proc. Anywhere in
/// lowered text, `extern::name` rewrites to a mangled Tcl symbol
/// the lowering's prelude has aliased to the underlying proc.
pub const EXTERN_PREFIX: &str = "extern::";

/// Result of [`rewrite_externs`]: the lowered text with every
/// `extern::name` reference replaced by its mangled Tcl form, plus
/// the deduplicated set of external names that were referenced.
/// Callers feed `names` to [`extern_rename_prelude`] to build the
/// one-time setup that exposes each extern at its mangled name.
#[derive(Clone, Debug)]
pub struct ExternRewrite {
    pub text: String,
    pub names: Vec<String>,
}

/// Replace every `extern::<name>` occurrence in `text` with the
/// mangled Tcl symbol `__vw_extern_<name>` (with `::` inside the
/// name folded to `__` so the resulting identifier is single-
/// segment Tcl). Returns the new text plus the unique, sorted set
/// of names seen — those are what the caller renames into place
/// in the prelude.
///
/// The rewrite is text-level, not AST-level. We have to handle
/// proc bodies (which lower as raw text), and a textual pass cleanly
/// catches calls at any nesting depth — inside `[ … ]`,
/// inside multi-arm `if {…} { … extern::foo … } else { … }`, etc.
/// Word-boundary detection on the leading side prevents a token
/// like `not_extern::foo` from triggering; the trailing identifier
/// is parsed greedily so `extern::a::b::c` rewrites as one unit.
pub fn rewrite_externs(text: &str) -> ExternRewrite {
    let mut out = String::with_capacity(text.len());
    let mut names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for the prefix at a word boundary.
        if i + EXTERN_PREFIX.len() <= bytes.len()
            && &bytes[i..i + EXTERN_PREFIX.len()]
                == EXTERN_PREFIX.as_bytes()
            && (i == 0 || !is_extern_ident_byte(bytes[i - 1]))
        {
            let name_start = i + EXTERN_PREFIX.len();
            let name_end = scan_extern_name_end(bytes, name_start);
            if name_end > name_start {
                let name = &text[name_start..name_end];
                out.push_str("__vw_extern_");
                out.push_str(&name.replace("::", "__"));
                names.insert(name.to_string());
                i = name_end;
                continue;
            }
        }
        // Copy a single character through. We can't index bytes
        // directly because the text may contain multi-byte UTF-8
        // (proc doc comments, string literals); advance to the
        // next char boundary instead.
        let ch_end = next_char_boundary(text, i);
        out.push_str(&text[i..ch_end]);
        i = ch_end;
    }
    ExternRewrite {
        text: out,
        names: names.into_iter().collect(),
    }
}

fn is_extern_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn scan_extern_name_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        if is_extern_ident_byte(bytes[i]) {
            i += 1;
        } else if bytes[i] == b':'
            && bytes.get(i + 1).copied() == Some(b':')
            && bytes
                .get(i + 2)
                .copied()
                .is_some_and(is_extern_ident_byte)
        {
            i += 2;
        } else {
            break;
        }
    }
    i
}

fn next_char_boundary(s: &str, start: usize) -> usize {
    let mut end = start + 1;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    end
}

/// Build the one-time setup Tcl that exposes each extern at its
/// mangled name. Idempotent — re-running across REPL inputs is
/// harmless because each rename is guarded.
pub fn extern_rename_prelude(names: &[String]) -> String {
    let mut out = String::new();
    for name in names {
        let mangled = format!("__vw_extern_{}", name.replace("::", "__"));
        out.push_str(&format!(
            "if {{[info commands {mangled}] eq \"\" && \
             [info commands {name}] ne \"\"}} {{ \
             rename {name} {mangled} }}\n"
        ));
    }
    out
}

/// True when `call_name` is the explicit `extern::…` form — used
/// by the validator to skip the unknown-call check for these
/// deliberately-external invocations.
pub fn is_extern_call(call_name: &str) -> bool {
    call_name.starts_with(EXTERN_PREFIX)
}

/// Reconstruct a command's words as lowered Tcl text. Splits the
/// problem along the AST's natural boundaries so each piece is
/// handled by the right rules:
///
/// - Bare and quoted words are rebuilt part-by-part. Plain text,
///   `$var` references, and `\x` escapes go through verbatim;
///   `[ … ]` substitutions recurse into the lowering pipeline so
///   keyword → positional rewriting applies to calls *inside* a
///   `set proj [ create_project … ]`, and multi-line bracket
///   bodies collapse to one Tcl statement by construction.
/// - Braced words are literal text — Tcl never substitutes inside
///   `{ … }`, so the parser doesn't even surface `CmdSubst` parts
///   for them; we ship them as raw source.
fn lower_words(
    words: &[Word],
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    words
        .iter()
        .map(|w| lower_word(w, source, table))
        .collect::<Vec<_>>()
        .join(" ")
}

fn lower_word(
    word: &Word,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    match word.form {
        WordForm::Bare => lower_word_parts(&word.parts, source, table),
        WordForm::Quoted => {
            let inner = lower_word_parts(&word.parts, source, table);
            format!("\"{inner}\"")
        }
        WordForm::Braced => word.span.slice(source).to_string(),
    }
}

fn lower_word_parts(
    parts: &[WordPart],
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::VarRef { name, .. } => {
                out.push('$');
                out.push_str(name);
            }
            WordPart::Escape { value, .. } => {
                out.push('\\');
                out.push(*value);
            }
            WordPart::CmdSubst { body, .. } => {
                let lowered: Vec<String> = body
                    .iter()
                    .filter_map(|s| match s {
                        Stmt::Command(c) => {
                            Some(lower_command(c, source, table))
                        }
                        _ => None,
                    })
                    .filter(|s| !s.trim().is_empty())
                    .collect();
                out.push('[');
                out.push_str(&lowered.join("; "));
                out.push(']');
            }
        }
    }
    out
}

/// Helper retained for symmetry with future analyzers that want to
/// inspect a word's literal form without re-walking its parts.
#[allow(dead_code)]
fn word_text(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::Escape { value, .. } => out.push(*value),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn lowered(src: &str) -> Vec<String> {
        let parsed = parse(src);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let table = signature_table(&parsed.document);
        parsed
            .document
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Command(c) => Some(lower_command(c, src, &table)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn proc_decl_drops_attributes() {
        let src = "proc f {\n  @default(0) a\n  @default(1) b\n} { puts hi }\n";
        let out = lowered(src);
        assert_eq!(out[0], "proc f {a b} { puts hi }");
    }

    #[test]
    fn call_with_all_flags_reorders_to_positional() {
        let src = "proc f {\n  a\n  b\n} { puts hi }\nf -b 22 -a 11\n";
        let out = lowered(src);
        assert_eq!(out[1], "f 11 22");
    }

    #[test]
    fn call_with_omitted_arg_uses_default() {
        let src = "proc f {\n  @default(7) a\n  b\n} { puts hi }\nf -b 22\n";
        let out = lowered(src);
        assert_eq!(out[1], "f 7 22");
    }

    #[test]
    fn inner_call_inside_brackets_is_rewritten_to_positional() {
        // The shape that broke metroid/project.htcl: `set proj [
        // some_known_proc -k v ]`. The outer `set` is verbatim,
        // but the inner `some_known_proc` call must be rewritten
        // from keyword form to positional — otherwise Tcl will pass
        // the literal `-k v` words to the lowered Tcl proc (which
        // takes positional args after lowering).
        let src = "proc make {
  @default(\"\") part
  name
} { puts ok }
set proj [
  make
    -part xc
    -name foo
]
";
        let out = lowered(src);
        // Two top-level statements: the proc declaration and the
        // set call. We care about the set call's lowered form.
        assert_eq!(out.len(), 2, "{:?}", out);
        let set_line = &out[1];
        // The inner `make` must be positional, not `-part xc -name foo`.
        assert!(
            set_line.contains("[make xc foo]"),
            "expected inner call rewritten; got: {set_line}"
        );
        // And the whole thing collapses to a single line.
        assert!(
            !set_line.contains('\n'),
            "expected single line; got: {set_line:?}"
        );
    }

    #[test]
    fn multiline_bracket_substitution_collapses_to_one_line() {
        // The exact shape that broke the REPL: an outer call whose
        // sole arg is a `[ … ]` substitution spanning multiple
        // source lines. Tcl would parse the bracket body as N
        // separate commands; we have to flatten the newlines.
        let src = "set proj [\n  create_project\n    -in_memory 1\n    -name foo\n]\n";
        let out = lowered(src);
        assert_eq!(out.len(), 1);
        // No literal newline inside the brackets after lowering.
        let between = out[0]
            .split_once('[')
            .and_then(|(_, rest)| rest.rsplit_once(']'))
            .map(|(inner, _)| inner)
            .unwrap();
        assert!(!between.contains('\n'), "lowered: {:?}", out[0]);
        // The full call must still parse as `set proj [ ... ]`.
        assert!(out[0].starts_with("set proj ["));
        assert!(out[0].trim_end().ends_with(']'));
    }

    #[test]
    fn nested_multiline_brackets_all_collapse() {
        // `[outer [inner ...] ...]` — newlines inside both layers
        // become spaces; the parser sees nested CmdSubst so the
        // recursive collection covers both.
        let src = "set x [\n  foo\n    -a [\n      bar\n        -b 1\n    ]\n]\n";
        let out = lowered(src);
        assert!(!out[0].contains('\n'), "lowered: {:?}", out[0]);
    }

    #[test]
    fn newlines_inside_braced_groups_stay_intact() {
        // Inside `{ … }` the brackets are literal, not a
        // substitution. The parser doesn't emit a `CmdSubst` for
        // them so we must not strip newlines from braced bodies.
        let src = "proc f {} {\n  puts a\n  puts b\n}\n";
        let out = lowered(src);
        // The proc-decl lowering builds its own output (not the
        // verbatim path), so it preserves body newlines.
        assert!(out[0].contains('\n'), "lowered: {:?}", out[0]);
    }

    #[test]
    fn rewrite_externs_mangles_call_sites() {
        let r = rewrite_externs(
            "set cmd [list extern::set_property]\n\
             extern::create_project -name foo\n",
        );
        assert!(r.text.contains("__vw_extern_set_property"), "{}", r.text);
        assert!(r.text.contains("__vw_extern_create_project"), "{}", r.text);
        // Original tokens are gone from the rewritten text.
        assert!(!r.text.contains("extern::"), "{}", r.text);
        // Names returned sorted + unique.
        assert_eq!(r.names, vec!["create_project", "set_property"]);
    }

    #[test]
    fn rewrite_externs_handles_namespaced_names() {
        let r = rewrite_externs("extern::common::send_msg_id A B C\n");
        // `::` inside the name folds to `__` so the mangled symbol
        // is single-segment Tcl.
        assert!(
            r.text.contains("__vw_extern_common__send_msg_id"),
            "{}",
            r.text
        );
        assert_eq!(r.names, vec!["common::send_msg_id"]);
    }

    #[test]
    fn rewrite_externs_respects_word_boundary() {
        // A token like `not_extern::foo` must NOT rewrite — the
        // prefix is in the middle of a word.
        let r = rewrite_externs("set x not_extern::foo\n");
        assert_eq!(r.text, "set x not_extern::foo\n");
        assert!(r.names.is_empty());
    }

    #[test]
    fn extern_rename_prelude_is_idempotent_per_name() {
        let p = extern_rename_prelude(&["set_property".to_string()]);
        // The `if` guard makes re-running safe.
        assert!(p.contains("info commands __vw_extern_set_property"));
        assert!(p.contains("info commands set_property"));
        assert!(p.contains("rename set_property __vw_extern_set_property"));
    }

    #[test]
    fn is_extern_call_recognizes_prefix() {
        assert!(is_extern_call("extern::set_property"));
        assert!(is_extern_call("extern::common::send_msg_id"));
        assert!(!is_extern_call("set_property"));
        assert!(!is_extern_call("not_extern::foo"));
    }

    #[test]
    fn unknown_command_passes_through() {
        let src = "puts \"hello $x\"\n";
        let out = lowered(src);
        assert_eq!(out[0], "puts \"hello $x\"");
    }

    #[test]
    fn string_default_quotes_correctly() {
        let src = "proc f {\n  @default(\"hi\") greeting\n} { puts hi }\nf\n";
        let out = lowered(src);
        assert_eq!(out[1], "f \"hi\"");
    }
}
