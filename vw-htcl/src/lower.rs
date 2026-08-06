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
use crate::line_index::LineIndex;

pub type SignatureTable<'a> = HashMap<String, &'a ProcSignature>;

/// Empty putr rewrite map used as the default when a caller
/// doesn't have one. See [`lower_command_with_putr`] for the
/// keyed-lookup semantics.
fn empty_putr_map() -> &'static crate::putr::RewriteMap {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<crate::putr::RewriteMap> = OnceLock::new();
    EMPTY.get_or_init(crate::putr::RewriteMap::new)
}

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
/// backend. See [`lower_command_with_putr`] for the variant that
/// takes a `putr` rewrite map; callers with no putr calls (or who
/// don't care) can invoke this simpler form.
///
/// Builds a fresh [`LineIndex`] every call — fine for one-off
/// uses (tests, single-statement lowering) but pathological when
/// looped over a document with thousands of top-level procs. Bulk
/// callers should build the index once and use
/// [`lower_command_with_putr_and_index`] to skip the per-call
/// rebuild — a 19MB flat document has 1000× the newline-scan
/// cost of a single-statement fragment.
pub fn lower_command(
    cmd: &Command,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    let line_index = LineIndex::new(source);
    lower_command_with_putr_and_index(
        cmd,
        source,
        table,
        empty_putr_map(),
        &line_index,
    )
}

/// Lower one top-level command, consulting `putr_map` first: when
/// `cmd.span` is a key in the map, the map's replacement Tcl is
/// used verbatim in place of the standard lowering. This is how
/// `putr $x` becomes `puts [T::repr -v $x]` at emit time without
/// mutating the source string.
///
/// Recurses into proc bodies / namespace-eval bodies / cmd-subst
/// bodies with the same `putr_map`, so `putr` calls buried inside
/// any of those still get the rewrite.
pub fn lower_command_with_putr(
    cmd: &Command,
    source: &str,
    table: &SignatureTable<'_>,
    putr_map: &crate::putr::RewriteMap,
) -> String {
    let line_index = LineIndex::new(source);
    lower_command_with_putr_and_index(cmd, source, table, putr_map, &line_index)
}

/// Bulk-friendly variant of [`lower_command_with_putr`] that
/// accepts a pre-built [`LineIndex`] instead of constructing one
/// per call. Callers looping over thousands of top-level
/// statements MUST use this form — every `lower_proc_decl`
/// consult needs line-of-offset lookups, and rebuilding the
/// index over a 19MB flat source per proc was the O(procs ×
/// source_len) accidental quadratic that made auto-loading a
/// large `.htcl` take minutes.
pub fn lower_command_with_putr_and_index(
    cmd: &Command,
    source: &str,
    table: &SignatureTable<'_>,
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    // Fast path: if this command IS a putr rewrite target, emit
    // the replacement verbatim. No further descent needed — the
    // replacement is a complete Tcl expression.
    if let Some(replacement) = putr_map.get(&cmd.span) {
        return replacement.clone();
    }
    match &cmd.kind {
        CommandKind::Proc(proc) => {
            lower_proc_decl(proc, source, table, putr_map, line_index)
        }
        CommandKind::NamespaceEval(ns) => {
            lower_namespace_eval(ns, source, table, putr_map, line_index)
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
        // Newtype declarations are compile-time only — they feed the
        // analyzer / printer machinery but ship nothing to Vivado.
        // Drop entirely (empty Tcl, no whitespace).
        CommandKind::TypeDecl(_) => String::new(),
        // Enum declarations are also compile-time-only at this
        // layer — the codegen path (vw-htcl/src/repr.rs) emits the
        // auto-generated `namespace eval <Enum>` block separately
        // through the same wrap-with-repr pipeline used for the
        // primitive prelude. The decl itself ships nothing.
        CommandKind::EnumDecl(_) => String::new(),
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
            lower_words(&cmd.words, source, table, putr_map, line_index)
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
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    let name = ns.name.as_deref().unwrap_or("");
    let mut body = String::new();
    for stmt in &ns.body {
        let Stmt::Command(cmd) = stmt else { continue };
        let line = lower_command_with_putr_and_index(
            cmd, source, table, putr_map, line_index,
        );
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
fn lower_proc_decl(
    proc: &Proc,
    source: &str,
    table: &SignatureTable<'_>,
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    lower_proc_decl_with_name_and_index(
        proc, source, table, None, putr_map, line_index,
    )
}

/// Like [`lower_proc_decl`] but uses `name_override` as the emitted
/// proc name instead of `proc.name`. Used by the REPL when lowering
/// an enum-overload specialization under its mangled name
/// (`__<public>__<Variant>`) — the source name on the parsed proc
/// is the user-visible public name (`handle_prop`), but the
/// dispatcher needs the specialization to live under its mangled
/// alias so the runtime switch can find it.
pub fn lower_proc_decl_with_name(
    proc: &Proc,
    source: &str,
    table: &SignatureTable<'_>,
    name_override: Option<&str>,
    putr_map: &crate::putr::RewriteMap,
) -> String {
    let line_index = LineIndex::new(source);
    lower_proc_decl_with_name_and_index(
        proc,
        source,
        table,
        name_override,
        putr_map,
        &line_index,
    )
}

/// Bulk-friendly variant: takes a pre-built [`LineIndex`] instead
/// of constructing one over the entire source per call. The old
/// unindexed form was the O(procs × source_len) hotspot that made
/// `prepare` for a 19MB flat document take ~85s.
pub fn lower_proc_decl_with_name_and_index(
    proc: &Proc,
    source: &str,
    table: &SignatureTable<'_>,
    name_override: Option<&str>,
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    let name = name_override.or(proc.name.as_deref()).unwrap_or("");
    // Re-emit the body by walking its parsed statements rather
    // than slicing raw text. This is what gives htcl's "newlines
    // inside `[ … ]` are whitespace" semantics inside proc bodies
    // too — verbatim slicing leaves Tcl to interpret the
    // newlines as command separators, which silently splits a
    // multi-line `set x [ foo \n  -a 1 \n  -b 2 \n]` into four
    // separate calls and drops every flag arg.
    //
    // Critically, we pad the emitted body with blank lines so
    // each lowered statement lands on the SAME line it occupied
    // in the source. Tcl's `info frame` reports body lines
    // relative to the script text it was given — without padding,
    // collapsing a 5-line `[ ... ]` to one line shifts every
    // subsequent statement upward and the stack trace's
    // "line N in proc X" ends up pointing at unrelated source
    // lines. With padding, Tcl's body line N == source body
    // line N == `body_start_file_line + N - 1`, which is what
    // the REPL's `ProcLocation::resolve_body_line` already
    // assumes.
    let body_open_line = line_index.position(proc.body_span.start).line; // 0-based
    let body = if proc.body.is_empty() {
        proc.body_span.slice(source).to_string()
    } else {
        let mut out = String::new();
        // First emitted body line corresponds to one line after
        // the line containing `{`. We track 0-based file lines
        // throughout.
        let mut cur_line = body_open_line + 1;
        for stmt in &proc.body {
            let Stmt::Command(cmd) = stmt else { continue };
            let stmt_line = line_index.position(cmd.span.start).line;
            while cur_line < stmt_line {
                out.push('\n');
                cur_line += 1;
            }
            let line = lower_command_with_putr_and_index(
                cmd, source, table, putr_map, line_index,
            );
            if line.is_empty() {
                continue;
            }
            out.push_str(&line);
            out.push('\n');
            cur_line += 1 + line.matches('\n').count() as u32;
        }
        out
    };
    let Some(sig) = proc.signature.as_ref() else {
        // Couldn't parse a structured signature — emit the proc
        // verbatim. Tcl will accept it if the raw arg text is
        // valid Tcl; otherwise the user already has a parse-error
        // diagnostic from the upstream parser.
        let args_list = proc.args_span.slice(source);
        return format!("proc {name} {{{args_list}}} {{{body}}}");
    };
    let sig_dict = build_kwargs_sig_dict(sig);
    // Put `::vw::kwargs` on the SAME line as the opening `{` so
    // it doesn't eat the first source line of the body and shift
    // subsequent statements. Tcl treats "the line containing `{`"
    // as body line 1 — putting the kwargs preamble there means
    // body line 2 onward maps 1:1 to source lines, matching
    // what the padding loop above produced.
    format!(
        "proc {name} {{args}} {{ ::vw::kwargs $args {{{sig_dict}}}\n{body}}}"
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
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    // Preserve source-level adjacency between consecutive words.
    // The parser splits `{*}$var` into two AST words ({*} as a
    // braced "*", $var as a bare word) but their source spans
    // touch — Tcl reads them as the expand-prefix operator.
    // Joining with a literal space would force `{*} $var`, which
    // Tcl reinterprets as a literal-`*`-arg followed by `$var`.
    // Checking adjacency keeps the no-space form for `{*}$var`
    // while still spacing genuinely-whitespace-separated words.
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            let prev_end = words[i - 1].span.end;
            if w.span.start > prev_end {
                out.push(' ');
            }
        }
        out.push_str(&lower_word(w, source, table, putr_map, line_index));
    }
    out
}

fn lower_word(
    word: &Word,
    source: &str,
    table: &SignatureTable<'_>,
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    match word.form {
        WordForm::Bare => {
            lower_word_parts(&word.parts, source, table, putr_map, line_index)
        }
        WordForm::Quoted => {
            let inner = lower_word_parts(
                &word.parts,
                source,
                table,
                putr_map,
                line_index,
            );
            format!("\"{inner}\"")
        }
        WordForm::Braced => word.span.slice(source).to_string(),
    }
}

fn lower_word_parts(
    parts: &[WordPart],
    source: &str,
    table: &SignatureTable<'_>,
    putr_map: &crate::putr::RewriteMap,
    line_index: &LineIndex,
) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::VarRef { name, braced, .. } => {
                // Preserve the source's braced form. Emitting a
                // plain `$name` where the source had `${name}`
                // breaks interpolations like `"${ip}_wrapper.vhd"`:
                // Tcl reads `$ip_wrapper` as one greedy ident
                // and errors with "no such variable ip_wrapper".
                // Preserving the braces also happens to be a
                // no-op for typical `$var` refs — we only wrap
                // when the source did.
                if *braced {
                    out.push_str("${");
                    out.push_str(name);
                    out.push('}');
                } else {
                    out.push('$');
                    out.push_str(name);
                }
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
                            Some(lower_command_with_putr_and_index(
                                c, source, table, putr_map, line_index,
                            ))
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
    fn braced_var_ref_preserves_braces_in_output() {
        // Regression: `"${ip}_wrapper.vhd"` in htcl source used to
        // lower to `"$ip_wrapper.vhd"`, which Tcl reads as
        // `$ip_wrapper` (one greedy identifier) — dereferencing a
        // non-existent variable. Preserving the braces is the fix.
        let src = "puts \"${ip}_wrapper.vhd\"\n";
        let out = lowered(src);
        assert_eq!(
            out[0], "puts \"${ip}_wrapper.vhd\"",
            "braced form must round-trip",
        );
    }

    #[test]
    fn bare_var_ref_still_uses_bare_form() {
        // The braces are a source-level distinction; unadorned
        // `$var` should still emit as `$var`, not `${var}` (Tcl
        // handles both but the bare form is the idiomatic one).
        let src = "puts \"$ip.bd\"\n";
        let out = lowered(src);
        assert_eq!(out[0], "puts \"$ip.bd\"");
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
