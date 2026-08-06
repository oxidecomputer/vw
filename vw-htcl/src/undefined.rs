// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Undefined-variable error pass.
//!
//! Emits a [`Diagnostic`] with severity [`Severity::Error`] for every
//! `$name` reference whose name isn't in the enclosing scope's decl
//! set. Same scope model as [`crate::unused`] (proc args + `set` +
//! `foreach` + `upvar`, with body-host bodies contributing decls to
//! the enclosing scope), inverted: uses that lack a matching decl.
//!
//! **What we catch.** The motivating case is the typo
//! `set _dcmac [ … ]; return $dcmac` from dcmac.htcl — obvious
//! misspelling, previously only caught at runtime by Vivado.
//!
//! **What we don't catch.** Full Tcl-style scoping (deep `upvar N`
//! traversal, `interp` / child interpreters, `global`/`variable`
//! cross-frame refs) is out of scope. Cross-file globals aren't
//! tracked either — the pass looks at one document at a time.
//!
//! **Escape hatches.** Scope-leak suppression (same policy as
//! `unused.rs`): a scope containing `eval $x` / `uplevel N $x` /
//! `apply $x` / `info exists $var` is opaque, so we emit no undef
//! errors for it. A small implicit-name whitelist (`env`,
//! `errorInfo`, `errorCode`, `tcl_platform`, `tcl_version`, `argv`,
//! `argv0`, `_`) covers Tcl's environment-provided names.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Command, CommandKind, Document, Stmt, Word, WordForm, WordPart,
};
use crate::hover::is_body_host;
use crate::span::Span;
use crate::unused::{
    collect_decls, reparse_braced_body, scope_is_leaked, DeclSite,
};
use crate::validate::{Diagnostic, Severity};

/// Names pre-defined in every Tcl scope. Referencing any of these
/// never produces an undefined-variable error.
const IMPLICITS: &[&str] = &[
    "env",
    "errorInfo",
    "errorCode",
    "tcl_platform",
    "tcl_version",
    "tcl_pkgPath",
    "tcl_library",
    "tcl_patchLevel",
    "argv",
    "argv0",
    "argc",
    "_",
];

/// Collect the names of every variable defined at the document's
/// top level. Same collection rules as the undef pass — `set`
/// LHS, `foreach` iterator vars, `upvar` locals, `catch` result
/// vars, `regexp`/`regsub` capture vars, and any of the above
/// inside body-host `if`/`while`/… bodies that run in the top-
/// level frame. Used by the REPL to accumulate a name set across
/// batches so `set p …` in batch N-1 doesn't cause a false-
/// positive `undefined variable $p` in batch N.
pub fn top_level_var_names(
    document: &Document,
    source: &str,
) -> HashSet<String> {
    let mut decls: HashMap<String, DeclSite> = HashMap::new();
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        crate::unused::collect_decls(cmd, source, &mut decls);
    }
    decls.into_keys().collect()
}

/// Companion to [`top_level_var_names`] that also returns the
/// inferred type of each top-level `set VAR <value>` binding when
/// the RHS's type is statically knowable. Used by the REPL to
/// carry variable-type context across batches so `putr $foo` in
/// batch N sees the type that batch N-1's `set foo […]` produced.
///
/// Only the whole-word `[proc-call]`, `$var-copy`, and bare
/// `true`/`false` shapes are typed — everything else stays out
/// (matches [`crate::validate::value_type`]'s coverage). Missing
/// entries are fine: the caller merges these into an initial
/// `VarTypeTable` and falls back to plain `puts` when a name
/// isn't present.
pub fn top_level_var_types(
    document: &Document,
    sig_table: &HashMap<String, &crate::ast::ProcSignature>,
) -> HashMap<String, crate::ast::TypeExpr> {
    use crate::ast::CommandKind;
    // Threaded var_table so a later `set y $x` picks up the type
    // an earlier `set x [typed_proc]` recorded — matches the
    // rewrite walker's own scope discipline for consistency.
    let mut var_table = crate::validate::VarTypeTable::new();
    // Proc table for return-type INFERENCE on unannotated procs.
    // A user proc like `proc configure_gtm {} { set cfg [typed]; …; return $cfg }`
    // has no annotated return type — `value_type` alone would
    // report None for `[configure_gtm]`. The proc-table lookup
    // lets `value_type_with_procs` walk the body's last `return`
    // to figure out the type flows out. Without this, `putr
    // $_gtm` after `set _gtm [configure_gtm]` falls to plain
    // puts and dumps the raw tagged tree.
    let proc_table = crate::validate::build_proc_table(document);
    for stmt in &document.stmts {
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
        let Some(name) = name_word.as_text() else {
            continue;
        };
        if let Some(ty) = crate::validate::value_type_with_procs(
            value_word,
            sig_table,
            &var_table,
            Some(&proc_table),
        ) {
            var_table.insert(name.to_string(), ty);
        }
    }
    var_table
}

/// Top-level entry. Walks the document as one scope (for top-level
/// `set`/`$var` references), then recurses into every proc body /
/// namespace-eval body as an independent scope.
pub fn validate_undefined_vars(
    document: &Document,
    source: &str,
    diags: &mut Vec<Diagnostic>,
) {
    validate_undefined_vars_with_extras(
        document,
        source,
        &HashSet::new(),
        diags,
    );
}

/// Same as [`validate_undefined_vars`], with an extra pool of
/// top-level variable names the caller injects as "already
/// defined" — used by the REPL so a `set p …` in batch N-1 makes
/// `$p` in batch N legal. Only applies at the DOCUMENT top level;
/// proc bodies start with just their own args + `set`s (Tcl
/// semantics — top-level vars aren't visible inside a proc
/// without `global`/`upvar`, so leaking session state in would
/// mask real bugs).
pub fn validate_undefined_vars_with_extras(
    document: &Document,
    source: &str,
    extra_top_level: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    walk_scope(&document.stmts, source, extra_top_level, diags);
}

/// One scope pass — collect decls, walk uses (with spans), emit
/// errors for each use that has no matching decl and isn't implicit.
/// The `extra_decls` param is only non-empty for the document top
/// level (see [`validate_undefined_vars_with_extras`]); recursive
/// scope walks pass an empty set.
fn walk_scope(
    stmts: &[Stmt],
    source: &str,
    extra_decls: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    let mut decls: HashMap<String, DeclSite> = HashMap::new();
    for name in extra_decls {
        decls.insert(
            name.clone(),
            DeclSite {
                span: Span::new(0, 0),
                kind: crate::unused::DeclKind::Set,
            },
        );
    }
    let mut use_sites: Vec<(String, Span)> = Vec::new();
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        collect_decls(cmd, source, &mut decls);
        collect_use_sites_in_command(cmd, source, &mut use_sites);
    }
    if !scope_is_leaked(stmts, source) {
        emit_undefined(&decls, &use_sites, diags);
    }
    // Descend into fresh-frame children (proc bodies, namespace eval
    // bodies) regardless — the outer scope's leak doesn't taint them.
    // Extras only apply to the document top level; recursive walks
    // pass an empty set (see [`validate_undefined_vars_with_extras`]
    // docstring for the rationale).
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        descend_scopes(cmd, source, diags);
    }
}

/// Recurse into scope-establishing children of `cmd`. Nested procs
/// and `namespace eval` bodies each get their own `walk_scope`.
/// Mirrors `unused::descend_scopes` but with the undef pass's decl
/// seeding + use collection.
fn descend_scopes(cmd: &Command, source: &str, diags: &mut Vec<Diagnostic>) {
    match &cmd.kind {
        CommandKind::Proc(proc) => {
            let mut decls: HashMap<String, DeclSite> = HashMap::new();
            let mut use_sites: Vec<(String, Span)> = Vec::new();
            if let Some(sig) = proc.signature.as_ref() {
                for arg in &sig.args {
                    decls.insert(
                        arg.name.clone(),
                        DeclSite {
                            span: arg.name_span,
                            // The DeclKind values are private to
                            // unused.rs's warning-message dispatch;
                            // we never read them here, but the field
                            // must be set to something. Use ProcArg
                            // as the closest match — this DeclSite
                            // exists purely as a "name is defined"
                            // marker for our lookup.
                            kind: crate::unused::DeclKind::ProcArg,
                        },
                    );
                }
            }
            for stmt in &proc.body {
                let Stmt::Command(inner) = stmt else { continue };
                collect_decls(inner, source, &mut decls);
                collect_use_sites_in_command(inner, source, &mut use_sites);
            }
            if !scope_is_leaked(&proc.body, source) {
                emit_undefined(&decls, &use_sites, diags);
            }
            for stmt in &proc.body {
                let Stmt::Command(inner) = stmt else { continue };
                descend_scopes(inner, source, diags);
            }
        }
        CommandKind::NamespaceEval(ns) => {
            walk_scope(&ns.body, source, &HashSet::new(), diags);
        }
        _ => {}
    }
}

/// Walk `cmd.words` (and any body-host braced-body interiors) and
/// record every `WordPart::VarRef` as a (name, span) pair.
///
/// Body-host bodies run in the enclosing frame, so their `$foo`
/// references count as uses of the current scope. Skip `proc` and
/// `namespace eval` — those open fresh scopes and `descend_scopes`
/// handles them.
fn collect_use_sites_in_command(
    cmd: &Command,
    source: &str,
    use_sites: &mut Vec<(String, Span)>,
) {
    // `set foo` (exactly 2 words) is a read of $foo — mirror the
    // unused pass's convention.
    if matches!(cmd.kind, CommandKind::Set) && cmd.words.len() == 2 {
        if let Some(name) = cmd.words[1].as_text() {
            use_sites.push((name.to_string(), cmd.words[1].span));
        }
    }
    for word in &cmd.words {
        collect_use_sites_in_word(word, source, use_sites);
    }
    // Body-host recursion for uses. Same shape as `collect_decls`'s
    // recursion — proc / namespace eval are excluded.
    if let Some(head) = cmd.words.first().and_then(Word::as_text) {
        if is_body_host(head)
            && !matches!(
                &cmd.kind,
                CommandKind::Proc(_) | CommandKind::NamespaceEval(_)
            )
        {
            for word in cmd.words.iter().skip(1) {
                if let Some(stmts) = reparse_braced_body(word, source) {
                    for stmt in &stmts {
                        let Stmt::Command(inner) = stmt else { continue };
                        collect_use_sites_in_command(inner, source, use_sites);
                    }
                }
            }
        }
    }
}

fn collect_use_sites_in_word(
    word: &Word,
    source: &str,
    use_sites: &mut Vec<(String, Span)>,
) {
    // Braced literals are opaque in Tcl — `puts {$foo}` prints the
    // literal string `$foo`, not the value of `$foo`. The parser
    // encodes this by making `Braced` words carry a single Text
    // part with no VarRef sub-parts, so the loop below naturally
    // skips them.
    for part in &word.parts {
        match part {
            WordPart::VarRef { name, span, .. } => {
                use_sites.push((name.clone(), *span));
            }
            WordPart::CmdSubst { body, .. } => {
                for stmt in body {
                    let Stmt::Command(inner) = stmt else { continue };
                    collect_use_sites_in_command(inner, source, use_sites);
                }
            }
            WordPart::Text { .. } | WordPart::Escape { .. } => {}
        }
    }
    // Defensive: on the off-chance a Braced word carries VarRef
    // sub-parts (parser change / future extension), skip them —
    // Tcl braced-word semantics prevail. If future parsers surface
    // sub-refs from a `Quoted` word, they'll flow through above.
    if word.form == WordForm::Braced {
        // No-op: the loop above already handled the (only) Text
        // part. This block exists as documentation and a place to
        // add a diagnostic if the invariant ever breaks.
    }
}

/// For each use-site whose name isn't defined and isn't implicit,
/// emit an `undefined variable` error. The message includes a
/// "did you mean" hint when a decl within edit-distance 2 exists.
fn emit_undefined(
    decls: &HashMap<String, DeclSite>,
    use_sites: &[(String, Span)],
    diags: &mut Vec<Diagnostic>,
) {
    // Dedupe by span so a name referenced twice at the same span
    // (shouldn't happen, but cheap insurance) only emits once.
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    // Emit in source order for stable output.
    let mut items: Vec<&(String, Span)> = use_sites.iter().collect();
    items.sort_by_key(|(_, sp)| (sp.start, sp.end));
    for (name, span) in items {
        if !seen.insert((name.clone(), span.start, span.end)) {
            continue;
        }
        // The base name for `arr(key)` subscripts — if `arr` is
        // defined, the subscript form is a legal reference to it.
        let base = match name.find('(') {
            Some(idx) => &name[..idx],
            None => name.as_str(),
        };
        if decls.contains_key(base) || decls.contains_key(name) {
            continue;
        }
        if is_implicit(base) || is_implicit(name) {
            continue;
        }
        // Numeric names ($1, $2) — regex submatch refs and the
        // like. Skip.
        if name.parse::<u32>().is_ok() {
            continue;
        }
        let hint = suggest(name, decls);
        let message = match hint {
            Some(sug) => {
                format!("undefined variable `${name}`; did you mean `${sug}`?")
            }
            None => format!("undefined variable `${name}`"),
        };
        diags.push(Diagnostic {
            severity: Severity::Error,
            message,
            span: *span,
        });
    }
}

fn is_implicit(name: &str) -> bool {
    if IMPLICITS.contains(&name) {
        return true;
    }
    // Compiler-provided kwargs presence flags. The vw-htcl lowering
    // injects a `::vw::kwargs` shim call at proc entry that sets a
    // `__vw_kw_<arg>_set` boolean for every optional kwarg, so
    // proc bodies (both hand-written and generator-emitted) can
    // check `${__vw_kw_foo_set}` to see whether the user passed a
    // value. These never appear as source-level decls; treat as
    // pre-defined in every scope. See vw-ip/src/generate.rs's
    // `emit_dict_proc` for the emission side.
    if let Some(rest) = name.strip_prefix("__vw_kw_") {
        if rest.ends_with("_set") {
            return true;
        }
    }
    false
}

/// Levenshtein-distance suggestion — returns the closest decl name
/// within distance 2, only for names of length ≥ 3 (below that,
/// every 2-char name is within distance 2 of every other, which
/// produces noisy hints).
fn suggest(name: &str, decls: &HashMap<String, DeclSite>) -> Option<String> {
    if name.len() < 3 {
        return None;
    }
    let mut best: Option<(usize, String)> = None;
    for candidate in decls.keys() {
        if candidate.len() < 3 {
            continue;
        }
        let d = edit_distance(name, candidate);
        if d > 2 {
            continue;
        }
        if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            best = Some((d, candidate.clone()));
        }
    }
    best.map(|(_, s)| s)
}

/// Iterative Levenshtein distance. Small strings only — no need
/// to optimize further for our decl-set sizes.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] =
                (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn errors(src: &str) -> Vec<Diagnostic> {
        let parsed = parse(src);
        assert!(
            parsed.errors.is_empty(),
            "unexpected parse errors: {:?}",
            parsed.errors
        );
        let mut out = Vec::new();
        validate_undefined_vars(&parsed.document, src, &mut out);
        out.into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn dcmac_typo_flagged() {
        // The motivating case: `set _dcmac …` then `return $dcmac`.
        let src = "proc f {} { set _dcmac 1; return $dcmac }\n";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(
            e[0].message.contains("undefined variable `$dcmac`"),
            "{}",
            e[0].message
        );
        assert!(
            e[0].message.contains("did you mean `$_dcmac`"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn defined_local_clean() {
        let src = "proc f {} { set x 1; puts $x }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn proc_arg_clean() {
        let src = "proc f {x} { puts $x }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn foreach_var_clean() {
        let src = "proc f {xs} { foreach i $xs { puts $i } }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn foreach_iterator_available_after_loop() {
        // Tcl semantics: foreach iterator persists after the loop.
        let src = "proc f {xs} { foreach i $xs { }; puts $i }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn braced_ref_not_flagged() {
        // `puts {$foo}` — braced content is literal in Tcl, so
        // `$foo` here is just the string `$foo`, not a var ref.
        let src = "proc f {} { puts {$foo} }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn if_else_union_clean() {
        // Tcl runs `if`/`else` bodies in the enclosing frame, so a
        // `set x` inside either branch defines `x` in the outer
        // scope. Reading `$x` after the `if` is legal.
        let src = "\
proc f {c} {
  if { $c } { set x 1 } else { set x 2 }
  puts $x
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn underscore_prefix_no_alias() {
        // `_foo` and `foo` are distinct names. The unused-var
        // pass's `_`-prefix escape hatch does NOT create a $foo
        // alias for a set _foo decl.
        let src = "proc f {} { set _foo 1; puts $foo }\n";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(
            e[0].message.contains("undefined variable `$foo`"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn implicit_env_clean() {
        // `$env(HOME)` is a subscript on the `env` array, which is
        // pre-defined by Tcl.
        let src = "proc f {} { puts $env(HOME) }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn implicit_errorinfo_clean() {
        let src = "proc f {} { puts $errorInfo }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn dynamic_eval_suppresses() {
        // Scope leak — we can't tell what `eval $script` might
        // reference, so suppress the whole scope's undef check.
        let src = "proc f {} { eval $script; puts $mystery }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn top_level_flagged() {
        let src = "set x 1\nputs $y\n";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(
            e[0].message.contains("undefined variable `$y`"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn top_level_defined_clean() {
        // Matches the ~/sketch/metroid/project.htcl pattern:
        // top-level `set proj [...]` then `... -proj $proj`.
        let src = "set proj 1\nputs $proj\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn upvar_local_defined() {
        let src = "proc f {} { upvar 1 remote local; puts $local }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn subscript_base_defined() {
        // `$arr(key)` — `arr` is defined, so the array subscript
        // reference is fine.
        let src = "proc f {} { set arr 1; puts $arr(key) }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn cmd_subst_in_body() {
        let src = "proc f {x} { set y [list $x]; puts $y }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn nested_scope_isolated() {
        // Inner proc's `x` arg is distinct from outer scope.
        // Outer `$outer_var` is undefined; inner uses `$x` cleanly.
        let src = "\
proc outer {} {
  proc inner {x} { puts $x }
  puts $outer_var
}
";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(
            e[0].message.contains("undefined variable `$outer_var`"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn numeric_names_skipped() {
        // `$1`, `$2` are typically regex submatch refs. Not flagged.
        let src = "proc f {} { puts $1 }\n";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn typo_with_multiple_candidates() {
        // Closest match by edit distance wins.
        let src = "\
proc f {} {
  set apple 1
  set apricot 2
  puts $applet
}
";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(
            e[0].message.contains("did you mean `$apple`"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn port_htcl_repro_minimal() {
        // Simplest form of the port.htcl false-positive: an `if
        // {...} { continue }` in the foreach body between the
        // `foreach` line and the `set val`.
        let src = "\
proc f {obj} {
  foreach prop [list $obj] {
    if {$prop eq \"\"} { continue }
    set val 1
    puts $val
  }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn port_htcl_repro() {
        // The exact shape from ~/src/htcl/amd/vivado-cmd/port.htcl:116-124.
        let src = "\
proc f {obj} {
  foreach prop [list $obj] {
    if {![string match \"CONFIG.*\" $prop]} { continue }
    if {[regexp {^CONFIG\\.foo$} $prop]} { continue }
    set val [list $prop $obj]
    if {$val eq \"\"} { continue }
    puts $val
  }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn dict_for_binds_key_and_value_vars() {
        // `dict for {lib srcs} $deps { … }` — the two brace-list
        // names bind for the body's lifetime, same as a `foreach
        // {k v} $pairs { … }`. Before the fix, `$lib` / `$srcs`
        // inside the body were flagged as undefined.
        let src = "\
proc f {deps} {
  dict for {lib srcs} $deps {
    puts $lib
    puts $srcs
  }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn set_inside_foreach_body_defines_in_outer_scope() {
        // Repro for the port.htcl false-positive: `foreach x [...] {
        // set val [...]; puts $val }`. The `set val` is inside the
        // foreach body, which runs in the enclosing frame per Tcl
        // semantics, so `$val` on the next line is a legal ref.
        let src = "\
proc f {xs} {
  foreach x $xs {
    set val 1
    puts $val
  }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn catch_result_var_is_a_decl() {
        // `catch { … } n` — Tcl's catch binds the caught body's
        // result into `n` (the enclosing scope's frame). Repro for
        // lift.htcl:29 false-positive.
        let src = "\
proc f {raw} {
  if {[catch {llength $raw} n]} { return 0 }
  return $n
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn regexp_var_captures_are_decls() {
        // `regexp {pattern} $s var1 var2 …` — every trailing var
        // arg is a decl. Repro for props.htcl:121.
        let src = "\
proc f {s} {
  if {[regexp {(.*)=(.*)} $s _all key val]} {
    puts $key
    puts $val
  }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn vw_kwargs_flags_are_implicit() {
        // The vw-htcl lowering injects `__vw_kw_<arg>_set` boolean
        // sentinels via a `::vw::kwargs` shim at proc entry — every
        // optional kwarg gets one. These never appear as source-
        // level decls, and generator output relies on them
        // heavily. Treat as pre-defined in every scope.
        let src = "\
proc f {} {
  if {${__vw_kw_config_c0_set}} { puts hi }
}
";
        assert!(errors(src).is_empty(), "{:?}", errors(src));
    }

    #[test]
    fn short_names_no_suggestion_noise() {
        // Names < 3 chars don't participate in the suggestion.
        let src = "proc f {} { set ab 1; puts $xy }\n";
        let e = errors(src);
        assert_eq!(e.len(), 1, "{:?}", e);
        assert!(!e[0].message.contains("did you mean"), "{}", e[0].message);
    }
}
