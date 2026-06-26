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
                let Some(name) = proc.name.as_deref() else {
                    continue;
                };
                let Some(sig) = proc.signature.as_ref() else {
                    continue;
                };
                let qualified = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}::{name}")
                };
                table.insert(qualified, sig);
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
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
            // Verbatim, but reconstructed word-by-word so that any
            // `[ … ]` substitution inside the command gets its own
            // commands lowered through the same pipeline (extern
            // rewrites, multi-line bracket flattening).
            //
            // No keyword→positional rewrite here: htcl is keyword-
            // only at the call site, and the rewrite from `-flag
            // value` pairs to local variables happens at runtime
            // in the wrapper's `::vw::kwargs $args { ... }` prelude
            // (emitted by `lower_proc_decl`). That lets call sites
            // anywhere — top-level, inside a proc body, inside a
            // `[ ... ]`, inside an `eval` — work uniformly without
            // the lowerer needing to see every call site.
            lower_words(&cmd.words, source, table)
        }
    }
}

/// Lower a `namespace eval` block: recurse on each inner statement
/// (so inner proc declarations get their htcl attributes stripped
/// and gain the `::vw::kwargs` runtime prelude) and wrap the
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

/// Lower a `proc` declaration into a Tcl proc whose runtime
/// signature is `args` (variadic). The first line of the body is a
/// generated `::vw::kwargs $args { name default ... }` call that
/// parses the caller's `-flag value` pairs into local variables
/// matching the declared parameter names — defaults applied where
/// the caller didn't supply a flag. After the prelude the original
/// body runs unchanged, using `$name`, `$dir`, etc. just as if
/// they were standard Tcl parameters.
///
/// Why this shape: htcl is keyword-only at the call site. Doing
/// the parse at runtime (in the wrapper) means every call site
/// works the same — top-level, inside a proc body, inside a
/// `[ ... ]` substitution, inside an `eval`. The previous
/// architecture rewrote `-flag value` → positional at compile
/// time, but only for top-level calls the lowerer could see; calls
/// inside proc bodies stayed verbatim and broke at runtime against
/// a positional-only wrapper proc.
///
/// Procs without a parsed signature (parser couldn't extract one
/// from the args list, e.g. mid-edit syntax error) pass through as
/// plain Tcl: `proc name { <raw args text> } { <body> }`. The
/// `::vw::kwargs` prelude is only emitted when we know what
/// parameters to declare.
fn lower_proc_decl(proc: &Proc, source: &str) -> String {
    let name = proc.name.as_deref().unwrap_or("");
    let body = proc.body_span.slice(source);
    let Some(sig) = proc.signature.as_ref() else {
        // Couldn't parse a structured signature — emit the proc
        // verbatim. Tcl will accept it if the raw arg text is
        // valid Tcl; otherwise the user already has a parse-error
        // diagnostic from the upstream parser.
        let args_list = proc.args_span.slice(source);
        return format!("proc {name} {{{args_list}}} {{{body}}}");
    };
    let sig_dict = build_kwargs_sig_dict(sig);
    format!(
        "proc {name} {{args}} {{\n  ::vw::kwargs $args {{{sig_dict}}}\n{body}}}"
    )
}

/// Render the parameter list as a flat `name default name default
/// ...` Tcl dict for [`::vw::kwargs`] to consume. The default for
/// an arg without `@default` is the empty string `""` — at which
/// point the validator has already complained at compile time
/// about missing required args. Quote each default through
/// [`AttributeValue::to_tcl_literal`] so integers, idents, and
/// strings all round-trip correctly.
fn build_kwargs_sig_dict(sig: &ProcSignature) -> String {
    let mut out = String::new();
    for (i, arg) in sig.args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&arg.name);
        out.push(' ');
        let default = arg
            .attribute("default")
            .and_then(|attr| attr.values.first())
            .map(|v| v.to_tcl_literal())
            .unwrap_or_else(|| "\"\"".to_string());
        out.push_str(&default);
    }
    out
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

/// Rewrite every `extern::<name>` in `text` to `::<name>` — the
/// Tcl-absolute form that anchors the lookup at the global
/// namespace. Returns the rewritten text plus the unique, sorted
/// set of names seen.
///
/// Anchoring at `::` matters because htcl wrappers live inside
/// `namespace eval vivado { … }`. Inside that namespace a bare
/// `create_project` resolution searches the *current* namespace
/// first and finds `vivado::create_project` (the wrapper itself!) —
/// infinite recursion. The leading `::` skips the current-
/// namespace search and goes straight to the global, where the
/// unshadowed Vivado native lives.
///
/// The rewrite is text-level, not AST-level. Proc bodies lower
/// as raw text, and a textual pass cleanly catches calls at any
/// nesting depth — inside `[ … ]`, inside multi-arm
/// `if {…} { … extern::foo … } else { … }`, etc. Word-boundary
/// detection on the leading side prevents `not_extern::foo` from
/// triggering; the trailing identifier is parsed greedily so
/// `extern::a::b::c` rewrites as one unit (→ `::a::b::c`).
pub fn rewrite_externs(text: &str) -> ExternRewrite {
    let mut out = String::with_capacity(text.len());
    let mut names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + EXTERN_PREFIX.len() <= bytes.len()
            && &bytes[i..i + EXTERN_PREFIX.len()] == EXTERN_PREFIX.as_bytes()
            && (i == 0 || !is_extern_ident_byte(bytes[i - 1]))
        {
            let name_start = i + EXTERN_PREFIX.len();
            let name_end = scan_extern_name_end(bytes, name_start);
            if name_end > name_start {
                let name = &text[name_start..name_end];
                // Leading `::` makes the lookup absolute (global
                // namespace) — necessary inside `namespace eval
                // vivado { … }` so the wrapper body doesn't
                // recurse on itself.
                out.push_str("::");
                out.push_str(name);
                names.insert(name.to_string());
                i = name_end;
                continue;
            }
        }
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
            && bytes.get(i + 2).copied().is_some_and(is_extern_ident_byte)
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

/// Historically emitted a rename prelude that aliased each Vivado
/// native to a mangled name so wrappers could forward to the
/// underlying proc without recursing on themselves. With wrappers
/// now living in the `vivado::` namespace and no longer shadowing
/// the globals they wrap, no rename is needed — `extern::foo`
/// just rewrites to bare `foo`, which Tcl resolves to the global
/// native. Kept as a public symbol so callers don't have to track
/// the layering change; returns the empty string.
pub fn extern_rename_prelude(_names: &[String]) -> String {
    String::new()
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

fn lower_word(word: &Word, source: &str, table: &SignatureTable<'_>) -> String {
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
    fn proc_decl_emits_kwargs_prelude() {
        // Every htcl proc lowers to `proc name {args} { ::vw::kwargs
        // ... ; body }` — the runtime helper parses the caller's
        // `-flag value` pairs into local variables matching the
        // declared param names, with defaults applied where the
        // caller didn't supply a flag. Body text passes through
        // unchanged.
        let src = "proc f {\n  @default(0) a\n  @default(1) b\n} { puts hi }\n";
        let out = lowered(src);
        assert!(
            out[0].starts_with("proc f {args} {"),
            "wrong arg-list form: {}",
            out[0]
        );
        assert!(
            out[0].contains("::vw::kwargs $args {a 0 b 1}"),
            "missing or wrong kwargs prelude: {}",
            out[0]
        );
        assert!(out[0].contains("puts hi"), "lost body: {}", out[0]);
    }

    #[test]
    fn call_with_flags_ships_verbatim() {
        // No more compile-time keyword→positional rewrite. The call
        // ships as the user typed it; the wrapper proc parses the
        // keywords at runtime via its kwargs prelude.
        let src = "proc f {\n  a\n  b\n} { puts hi }\nf -b 22 -a 11\n";
        let out = lowered(src);
        assert_eq!(out[1], "f -b 22 -a 11");
    }

    #[test]
    fn call_with_omitted_arg_ships_verbatim() {
        // The wrapper's default is wired in at runtime by
        // ::vw::kwargs; the call site doesn't need to fill it.
        let src = "proc f {\n  @default(7) a\n  b\n} { puts hi }\nf -b 22\n";
        let out = lowered(src);
        assert_eq!(out[1], "f -b 22");
    }

    #[test]
    fn inner_call_inside_brackets_ships_verbatim() {
        // What this test used to assert (`[make xc foo]` —
        // keyword→positional rewrite for the inner call) is no
        // longer the architecture. The inner call ships verbatim;
        // the wrapper parses `-part`/`-name` at runtime. The only
        // transformation we still apply is multi-line bracket
        // flattening.
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
        assert_eq!(out.len(), 2, "{:?}", out);
        let set_line = &out[1];
        // Inner call stays keyword-form: `make -part xc -name foo`.
        assert!(
            set_line.contains("[make -part xc -name foo]"),
            "inner call should ship verbatim; got: {set_line}"
        );
        // Multi-line bracket body still collapses to one line.
        assert!(
            !set_line.contains('\n'),
            "expected single line; got: {set_line:?}"
        );
    }

    #[test]
    fn call_inside_proc_body_ships_verbatim() {
        // Regression guard for the create_bd_design bug: a
        // keyword-form call to a known wrapper, nested inside
        // another proc's body, must NOT be rewritten. In the old
        // architecture the lowerer only saw top-level call sites
        // and silently failed to translate this one, so at runtime
        // Tcl handed `-name cips` to a positional-only wrapper
        // proc and errored "wrong # args". Now the wrapper parses
        // keywords at runtime, so we just ship the call as-is.
        let src = "proc create_bd_design { @default(\"\") name } { puts ok }\n\
                   proc configure_cips {} {\n  \
                     create_bd_design -name cips\n\
                   }\n";
        let out = lowered(src);
        // The configure_cips proc decl is the second statement.
        // Its body should still contain the keyword-form call —
        // we don't touch it at compile time.
        assert!(
            out[1].contains("create_bd_design -name cips"),
            "call inside proc body should ship verbatim; got:\n{}",
            out[1]
        );
    }

    #[test]
    fn proc_with_no_default_emits_empty_string_default() {
        // An htcl arg without `@default` is implicitly required —
        // the validator catches a missing-flag call at compile
        // time. At runtime we still need a placeholder default so
        // `::vw::kwargs` doesn't blow up when the variable is
        // referenced before the (missing) `-flag` would have set
        // it; we use `""` (empty string).
        let src = "proc f {\n  required_arg\n} { puts hi }\n";
        let out = lowered(src);
        assert!(
            out[0].contains("::vw::kwargs $args {required_arg \"\"}"),
            "wrong default for required arg: {}",
            out[0]
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
        let src =
            "set x [\n  foo\n    -a [\n      bar\n        -b 1\n    ]\n]\n";
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
    fn rewrite_externs_anchors_at_global_namespace() {
        let r = rewrite_externs(
            "set cmd [list extern::set_property]\n\
             extern::create_project -name foo\n",
        );
        // Leading `::` anchors the lookup at Tcl's global
        // namespace, which is where unshadowed Vivado natives
        // live — necessary so wrapper bodies inside `namespace
        // eval vivado { … }` don't recurse on themselves.
        assert!(r.text.contains("[list ::set_property]"), "{}", r.text);
        assert!(r.text.contains("::create_project -name foo"), "{}", r.text);
        assert!(!r.text.contains("extern::"), "{}", r.text);
        assert_eq!(r.names, vec!["create_project", "set_property"]);
    }

    #[test]
    fn rewrite_externs_preserves_namespaced_names() {
        let r = rewrite_externs("extern::common::send_msg_id A B C\n");
        // Same anchoring for multi-segment names —
        // `::common::send_msg_id` resolves the leading namespace
        // search from the global root.
        assert!(r.text.contains("::common::send_msg_id A B C"), "{}", r.text);
        assert_eq!(r.names, vec!["common::send_msg_id"]);
    }

    #[test]
    fn rewrite_externs_respects_word_boundary() {
        let r = rewrite_externs("set x not_extern::foo\n");
        assert_eq!(r.text, "set x not_extern::foo\n");
        assert!(r.names.is_empty());
    }

    #[test]
    fn extern_rename_prelude_is_empty() {
        // Wrappers no longer shadow globals (they live in the
        // `vivado::` namespace), so the historical rename plumbing
        // is unnecessary. The helper still exists for API stability
        // but always returns empty.
        let p = extern_rename_prelude(&["set_property".to_string()]);
        assert!(p.is_empty(), "{p}");
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
    fn string_default_quotes_correctly_in_kwargs_sig() {
        // Defaults are stamped into the proc's kwargs-prelude sig
        // dict, not into the call site. A `@default("hi")` becomes
        // the literal `"hi"` (quoted) in the dict — `::vw::kwargs`
        // sets `$greeting` to it when the caller omits the flag.
        let src = "proc f {\n  @default(\"hi\") greeting\n} { puts hi }\n";
        let out = lowered(src);
        assert!(
            out[0].contains("::vw::kwargs $args {greeting \"hi\"}"),
            "default should appear quoted in the sig dict: {}",
            out[0]
        );
    }
}
