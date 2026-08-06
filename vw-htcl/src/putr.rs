// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Compile-time rewrite of `putr <expr>` call sites into
//! `puts [<T>::repr -v <expr>]` (typed) or `puts <expr>`
//! (untyped fallback).
//!
//! `putr` is a compile-time-only shim — by the time source reaches
//! Tcl, every occurrence has been rewritten in place. This lets one
//! syntactic form cover the ergonomic case (`putr $cpm5_cfg` at the
//! REPL prompt to dump a typed value's `repr` output) without
//! needing a runtime `putr` proc, and without depending on Tcl-level
//! shape detection.
//!
//! ## Walker
//!
//! The rewrite pass walks the FULL parsed AST — top-level
//! statements, proc bodies (populated by
//! [`crate::parser::populate_procs`]), namespace-eval bodies, and
//! command-substitution interiors. It mirrors the scope discipline
//! in `validate::validate_stmts`:
//!
//! - Proc bodies push a fresh [`VarTypeTable`] frame seeded with
//!   the proc's typed parameters (from `ProcArg.type_annotation`).
//! - Namespace-eval bodies and command-substitution bodies share
//!   the enclosing frame (mirrors Tcl semantics — `namespace eval`
//!   creates a namespace but not a fresh local-variable scope, and
//!   `[…]` runs in the caller's frame).
//! - `set VAR <value>` records the value's inferred type in the
//!   current frame so downstream `putr $VAR` sees it.
//!
//! ## Rewriting
//!
//! Rewrites are exposed as a `HashMap<Span, String>` keyed by the
//! putr command's source span. `crate::lower::lower_command`
//! consults the map at emit time — when the current command's
//! span matches a key, it emits the replacement Tcl instead of
//! lowering the original. This avoids mutating the source
//! string, which would shift byte offsets and break
//! `LoadedProgram::locate_span` for anything after the rewrite
//! site.
//!
//! ## Fallbacks
//!
//! - `putr $x` where `x`'s type is unknown → `puts $x` (plain
//!   fallback; no worse than what the user would type directly).
//! - `putr` with zero args or more than one → left untouched. The
//!   analyzer's builtin recognition still accepts them; if the
//!   caller wanted something else the diagnostic layer flags it
//!   through the normal path.
//! - `putr <literal-string>` → falls to the untyped path. Literal
//!   strings don't carry `T::repr` targets; `puts "hello"` is what
//!   the user gets and what they probably want.

use std::collections::HashMap;

use crate::ast::{
    Command, CommandKind, Document, Proc, ProcSignature, Stmt, WordPart,
};
use crate::span::Span;
use crate::validate::{
    build_proc_table, build_signature_table, value_type_with_procs,
    VarTypeTable,
};

/// A map from putr command span → replacement Tcl. `crate::lower`
/// consults this at emit time so the lowered proc body and the
/// lowered top-level statements both pick up the rewrite. Empty
/// when the input contained no `putr` calls; safe to build and
/// pass into lowering unconditionally.
pub type RewriteMap = HashMap<Span, String>;

/// Build the rewrite map for every `putr <expr>` command in
/// `document`, dispatching through the argument's type's `repr`
/// proc when the type is statically knowable and falling back to
/// plain `puts` when it isn't. Equivalent to
/// [`rewrite_with_extras`] with an empty extras map — most
/// callers that don't have prior-batch state use this.
pub fn rewrite(source: &str, document: &Document) -> RewriteMap {
    rewrite_with_extras(source, document, &HashMap::new(), &HashMap::new())
}

/// Same as [`rewrite`] but accepts prior-batch context so the
/// REPL can see variable types that came from earlier commits.
///
/// - `extra_sigs`: signatures from prior batches, merged into the
///   local signature table so `putr [prior_batch_proc]` resolves.
/// - `extra_var_types`: prior-batch top-level variable bindings
///   (from `Session::top_level_var_types()`). Seeded into the
///   walker's initial `VarTypeTable` frame so `putr $prior_var`
///   dispatches through the right `T::repr`. Later `set` bindings
///   in the current document shadow.
///
/// `source` must be the same source `document` was parsed from —
/// the walker uses AST spans as byte offsets into it.
pub fn rewrite_with_extras(
    source: &str,
    document: &Document,
    extra_sigs: &HashMap<String, &ProcSignature>,
    extra_var_types: &HashMap<String, crate::ast::TypeExpr>,
) -> RewriteMap {
    let mut sig_diags = Vec::new();
    let mut table = build_signature_table(document, &mut sig_diags);
    // Prior-batch signatures fill in the gaps; the current
    // document's entries win (entry().or_insert is a no-op on
    // present keys).
    for (name, sig) in extra_sigs {
        table.entry(name.clone()).or_insert(*sig);
    }
    // Proc table for return-type inference on unannotated procs
    // — lets `putr [some_proc]` and `set x [some_proc]; putr $x`
    // both dispatch through the right `T::repr` even when
    // `some_proc` has no `-> T` annotation. Built from the
    // current document only; prior-batch procs aren't inferrable
    // here (their bodies aren't in `document`), but their
    // annotated returns already flow through `extra_sigs`.
    let proc_table = build_proc_table(document);
    let mut rewrites: RewriteMap = HashMap::new();
    let mut top_var_table: VarTypeTable = extra_var_types.clone();
    walk_stmts(
        source,
        &document.stmts,
        &table,
        &proc_table,
        &mut top_var_table,
        &mut rewrites,
    );
    rewrites
}

fn walk_stmts(
    source: &str,
    stmts: &[Stmt],
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &Proc>,
    var_table: &mut VarTypeTable,
    rewrites: &mut RewriteMap,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // `set VAR <value>` binding — seed the var-type table
        // before recursing so downstream `putr $VAR` sees the
        // type. Same shape as validate.rs's set-binding hook.
        if matches!(cmd.kind, CommandKind::Set) {
            if let (Some(name_word), Some(value_word)) =
                (cmd.words.get(1), cmd.words.get(2))
            {
                if let Some(name) = name_word.as_text() {
                    if let Some(ty) = value_type_with_procs(
                        value_word,
                        sig_table,
                        var_table,
                        Some(proc_table),
                    ) {
                        var_table.insert(name.to_string(), ty);
                    }
                }
            }
        }
        // `putr <arg>` — the actual work.
        if let Some(replacement) =
            try_rewrite_putr(source, cmd, sig_table, proc_table, var_table)
        {
            rewrites.insert(cmd.span, replacement);
        }
        // Recurse into structured commands.
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                // Fresh scope per proc body, seeded with typed
                // parameters. Same pattern as
                // `validate::validate_stmts`'s proc handling.
                let mut proc_scope = VarTypeTable::new();
                if let Some(sig) = &proc.signature {
                    for a in &sig.args {
                        if let Some(ty) = &a.type_annotation {
                            proc_scope.insert(a.name.clone(), ty.clone());
                        }
                    }
                }
                walk_stmts(
                    source,
                    &proc.body,
                    sig_table,
                    proc_table,
                    &mut proc_scope,
                    rewrites,
                );
            }
            CommandKind::NamespaceEval(ns) => {
                walk_stmts(
                    source, &ns.body, sig_table, proc_table, var_table,
                    rewrites,
                );
            }
            _ => {}
        }
        // Descend into `[ … ]` command substitution bodies on any
        // word — matches how validate.rs walks these. `putr`
        // buried inside a `[…]` still gets rewritten.
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    walk_stmts(
                        source, body, sig_table, proc_table, var_table,
                        rewrites,
                    );
                }
            }
            // Descend into control-flow braced bodies (foreach,
            // dict for, for, while, if, catch — populated by
            // `parser::populate_control_flow_bodies`). Without
            // this, `putr` inside a `foreach { … }` body would
            // reach Tcl as a literal command call.
            if let Some(body) = &word.body {
                walk_stmts(
                    source, body, sig_table, proc_table, var_table, rewrites,
                );
            }
        }
    }
}

/// If `cmd` is a `putr <arg>` call, return the replacement Tcl
/// source. `None` for non-putr commands and for `putr` with the
/// wrong arg count (which we leave untouched — the analyzer will
/// flag the arity issue through its normal path).
fn try_rewrite_putr(
    source: &str,
    cmd: &Command,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &Proc>,
    var_table: &VarTypeTable,
) -> Option<String> {
    let head = cmd.words.first()?;
    if head.as_text() != Some("putr") {
        return None;
    }
    // Exactly one argument. `putr` matches `puts`'s single-value
    // shape; multi-word invocations get left as-is.
    if cmd.words.len() != 2 {
        return None;
    }
    let arg = &cmd.words[1];
    // `value_type_with_procs` covers `$var`, `[proc-call]` (with
    // return-type inference for unannotated procs via proc_table),
    // and bare `true`/`false`. Everything else returns None and
    // lands in the plain-puts fallback.
    let inferred =
        value_type_with_procs(arg, sig_table, var_table, Some(proc_table));
    let arg_source = arg.span.slice(source);
    Some(match inferred {
        Some(ty) => {
            let dispatch = crate::repr::dispatch_name(&ty);
            format!("puts [{dispatch} -v {arg_source}]")
        }
        None => {
            // Fallback: `puts <arg-source>`. Same behavior the
            // caller would get from typing `puts $x` directly —
            // no worse than that, and consistent with what happens
            // for `putr <literal>`.
            format!("puts {arg_source}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// Test helper: apply the rewrite map to the source top-down
    /// so tests can assert on the fully-substituted result. The
    /// real emit path doesn't do this — it consults the map
    /// per-command during lowering — but for unit tests it's the
    /// clearest way to see what came out.
    fn rewrite_str(input: &str) -> String {
        let parsed = parse(input);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors,
        );
        let map = rewrite(input, &parsed.document);
        // Apply in reverse span order to preserve earlier byte
        // offsets.
        let mut entries: Vec<_> = map.into_iter().collect();
        entries.sort_by_key(|(s, _)| std::cmp::Reverse(s.start));
        let mut out = input.to_string();
        for (span, replacement) in entries {
            out.replace_range(
                span.start as usize..span.end as usize,
                &replacement,
            );
        }
        out
    }

    #[test]
    fn typed_var_dispatches_through_repr() {
        // `configure_x` is annotated → return type flows to `x`
        // via `set x [configure_x]` → `putr $x` sees x's type.
        let src = "\
proc configure_x {} MyType { return foo }
set x [configure_x]
putr $x
";
        let out = rewrite_str(src);
        // MyType::repr with the -v envelope.
        assert!(
            out.contains("puts [MyType::repr -v $x]"),
            "expected MyType::repr dispatch, got:\n{out}",
        );
        // Original putr line is gone.
        assert!(!out.contains("putr $x"), "putr $x still present:\n{out}");
    }

    #[test]
    fn untyped_var_falls_back_to_plain_puts() {
        // No proc annotation → var type unknown → plain `puts $x`.
        let src = "\
proc make_something {} { return foo }
set x [make_something]
putr $x
";
        let out = rewrite_str(src);
        assert!(out.contains("puts $x"), "expected plain puts, got:\n{out}",);
        // The rewrite should NOT have introduced a repr dispatch.
        assert!(
            !out.contains("::repr -v $x"),
            "unexpected repr dispatch for untyped var:\n{out}",
        );
    }

    #[test]
    fn inline_proc_call_uses_return_type() {
        // `putr [make]` — no intermediate binding; the arg is a
        // direct `[proc-call]`, `value_type` sees the return type.
        let src = "\
proc make {} MyType { return foo }
putr [make]
";
        let out = rewrite_str(src);
        assert!(
            out.contains("puts [MyType::repr -v [make]]"),
            "expected inline dispatch, got:\n{out}",
        );
    }

    #[test]
    fn inside_proc_body_uses_arg_type() {
        // Proc parameter with a type annotation → visible to the
        // walker's per-proc scope frame.
        let src = "\
proc show { v: MyType } {
    putr $v
}
";
        let out = rewrite_str(src);
        assert!(
            out.contains("puts [MyType::repr -v $v]"),
            "expected in-body dispatch, got:\n{out}",
        );
    }

    #[test]
    fn wrong_arity_left_alone() {
        // `putr` with zero args — outside the rewrite's target
        // shape, we leave the source untouched. Analyzer will
        // handle the arity complaint through its normal path.
        let src = "putr\n";
        let out = rewrite_str(src);
        assert_eq!(out, src);
    }

    #[test]
    fn unannotated_proc_return_type_inferred_from_body() {
        // `wrapper` has no return-type annotation, but its body
        // ends with `return $inner` where `inner` was set from a
        // typed proc. The rewrite should still resolve
        // `wrapper`'s return type via body inference, then flow
        // it into `$x`, then dispatch `putr $x` through the
        // right repr — the specific pattern that made
        // `putr $_gtm` fall to plain puts before this fix.
        let src = "\
proc typed_ctor {} MyType { return foo }
proc wrapper {} {
  set inner [typed_ctor]
  return $inner
}
set x [wrapper]
putr $x
";
        let out = rewrite_str(src);
        assert!(
            out.contains("puts [MyType::repr -v $x]"),
            "expected inferred MyType dispatch, got:\n{out}",
        );
    }
}
