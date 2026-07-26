// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Signature-aware call-site validation.
//!
//! Builds a {proc_name → ProcSignature} table from the top-level
//! procs in a document, then walks every call site in the same
//! document and checks the keyword arguments against the declared
//! signature. Diagnostics are language-neutral; downstream (the LSP,
//! `vw check`) maps them to the appropriate display form.

use std::collections::HashMap;

use crate::ast::{
    Attribute, AttributeValue, Command, CommandKind, Document, EnumDecl,
    OverloadInfo, OverloadVariant, Proc, ProcArg, ProcSignature, Stmt,
    TypeDecl, TypeExpr, Word, WordPart,
};
use crate::span::Span;

/// Side-table produced alongside the signature table by
/// [`build_signature_table_with_overloads`]. Maps each public proc
/// name that resolves to an enum-overload set to its [`OverloadInfo`].
/// Names not in this map are regular (non-overloaded) procs.
pub type OverloadTable = HashMap<String, OverloadInfo>;

/// Mangle a specialization's internal name. The `__` prefix is
/// reserved (the validator rejects user procs whose names start
/// with `__`) so mangled names don't collide with anything
/// user-written.
///
/// For namespaced public names (`Property::as_nested`), the
/// prefix goes on the LEAF, not the whole name — otherwise the
/// mangled form (`__Property::as_nested__Nested`) puts the proc
/// in a fictional `__Property` namespace Tcl hasn't created, and
/// `proc` errors with "unknown namespace." Keeping the leaf-only
/// prefix (`Property::__as_nested__Nested`) places the
/// specialization inside the SAME namespace as its public
/// dispatcher, which the enum prelude or user `namespace eval`
/// already declared.
pub fn mangle_specialization(public_name: &str, variant: &str) -> String {
    match public_name.rsplit_once("::") {
        Some((ns, leaf)) => format!("{ns}::__{leaf}__{variant}"),
        None => format!("__{public_name}__{variant}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
}

pub fn validate(document: &Document, source: &str) -> Vec<Diagnostic> {
    validate_with_signatures(document, source, &HashMap::new())
}

/// Same as [`validate`], but resolves unknown calls against an
/// additional pool of signatures supplied by the caller — used by
/// the REPL to make procs declared in earlier session batches
/// visible to a new batch without re-parsing the whole prelude.
///
/// Merge rules:
///
/// - The document's own signatures shadow `extra`. Redefining a
///   proc in `document` overrides the prior version (Tcl
///   semantics — a second `proc` redefines).
/// - Duplicate-definition diagnostics only fire for collisions
///   **within** `document`. A new batch that re-`src`s a wrapper
///   already loaded earlier shouldn't warn on every input.
pub fn validate_with_signatures<'doc>(
    document: &'doc Document,
    source: &str,
    extra: &HashMap<String, &'doc ProcSignature>,
) -> Vec<Diagnostic> {
    validate_with_extras(document, source, extra, &HashMap::new())
}

/// Full validation entry point: same as [`validate_with_signatures`]
/// but also takes a pool of newtype declarations from prior session
/// batches. Lets the REPL drop a `proc bd_cell::repr` in batch N
/// without re-tripping "type bd_cell missing repr" diagnostics for
/// the `type bd_cell = string` declaration in batch N-1.
pub fn validate_with_extras<'doc>(
    document: &'doc Document,
    source: &str,
    extra_sigs: &HashMap<String, &'doc ProcSignature>,
    extra_types: &HashMap<String, &'doc TypeDecl>,
) -> Vec<Diagnostic> {
    validate_with_all_extras(
        document,
        source,
        extra_sigs,
        extra_types,
        &HashMap::new(),
    )
}

/// Full validation entry point. Accepts a prior-batch pool of
/// signatures, type declarations, AND enum declarations, so the
/// REPL can split an `enum E = …` decl across batches from the
/// procs that dispatch on it.
pub fn validate_with_all_extras<'doc>(
    document: &'doc Document,
    source: &str,
    extra_sigs: &HashMap<String, &'doc ProcSignature>,
    extra_types: &HashMap<String, &'doc TypeDecl>,
    extra_enums: &HashMap<String, &'doc EnumDecl>,
) -> Vec<Diagnostic> {
    validate_with_all_extras_and_vars(
        document,
        source,
        extra_sigs,
        extra_types,
        extra_enums,
        &std::collections::HashSet::new(),
        &std::collections::HashSet::new(),
    )
}

/// Same as [`validate_with_all_extras`], plus:
///
/// - `extra_top_level_vars` — top-level variable names known to
///   be defined in prior batches. The undef-variable pass merges
///   these into its top-level decl set, so a `set p …` in REPL
///   batch N-1 makes `$p` in batch N legal. Proc-body scopes
///   ignore the pool (Tcl locals don't inherit top-level scope),
///   so this only affects the document's own top-level statements.
/// - `extra_dep_names` — workspace-dependency names the caller
///   registered with its `vw_htcl::Resolver`. Each `src @<name>`
///   statement in the document is checked against this pool; a
///   name that's not in the set fires an `Error` diagnostic
///   spanned to the `@<name>` text. Empty set → the check
///   no-ops (unit tests and non-workspace-aware callers).
pub fn validate_with_all_extras_and_vars<'doc>(
    document: &'doc Document,
    source: &str,
    extra_sigs: &HashMap<String, &'doc ProcSignature>,
    extra_types: &HashMap<String, &'doc TypeDecl>,
    extra_enums: &HashMap<String, &'doc EnumDecl>,
    extra_top_level_vars: &std::collections::HashSet<String>,
    extra_dep_names: &std::collections::HashSet<String>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    // Type-table FIRST — its keys feed the overloaded-proc-arm
    // detection in `build_signature_table_with_overloads` so a proc
    // whose first arg is a newtype-Qualified name (e.g.
    // `-config: versal_cips::PsPmcConfig` inside `namespace eval
    // versal_cips`) isn't misclassified as an enum-overload arm.
    // Duplicate-decl diagnostics from this pass are held aside and
    // re-emitted after signature collection so ordering matches the
    // pre-refactor rendering.
    let mut type_prescan_diags = Vec::new();
    let mut type_table =
        build_type_decl_table(document, &mut type_prescan_diags);
    for (name, td) in extra_types {
        type_table.entry(name.clone()).or_insert(*td);
    }
    let newtype_qualified_names: std::collections::HashSet<String> =
        type_table.keys().cloned().collect();
    let (mut table, _overloads) = build_signature_table_with_overloads(
        document,
        &newtype_qualified_names,
        &mut diags,
    );
    // Prior-batch signatures fill in the gaps. The doc's own entries
    // win because `entry().or_insert(...)` is a no-op on present keys.
    for (name, sig) in extra_sigs {
        table.entry(name.clone()).or_insert(*sig);
    }
    diags.extend(type_prescan_diags);
    let mut enum_table = build_enum_decl_table(document, &mut diags);
    for (name, ed) in extra_enums {
        enum_table.entry(name.clone()).or_insert(*ed);
    }
    validate_type_decl_triplets(&type_table, &table, &mut diags);
    validate_enum_decls(&enum_table, &type_table, &mut diags);
    // Set of every declared newtype's qualified name — passed to
    // the qualified-position validator so `Qualified` references
    // that resolve to a real newtype pass through instead of
    // hitting the enum-variant-focused reject.
    let newtype_names: std::collections::HashSet<String> =
        type_table.keys().cloned().collect();
    validate_qualified_positions(document, &newtype_names, &mut diags);
    let mut var_table = VarTypeTable::new();
    // Build a proc table once so the return-type check can walk
    // any proc's body without re-scanning the document per call.
    let proc_table = build_proc_table(document);
    validate_stmts(
        &document.stmts,
        source,
        &table,
        &proc_table,
        &newtype_qualified_names,
        &mut var_table,
        &mut diags,
    );
    // Undefined-variable check. Errors (fail `vw check` / red LSP
    // squiggle), mirror shape to unused-var pass but with the
    // set-operation flipped. `extra_top_level_vars` seeds the
    // top-level decl set so REPL batches see prior-batch vars.
    crate::undefined::validate_undefined_vars_with_extras(
        document,
        source,
        extra_top_level_vars,
        &mut diags,
    );
    // Warning-level pass: unused-variable check. Runs last so the
    // hard-error diagnostics keep priority visually and any short-
    // circuit in earlier passes is unaffected by the walk here.
    crate::unused::validate_unused_vars(document, source, &mut diags);
    // Undefined-src-module check. `src @<name>` where `<name>`
    // isn't a registered dep name in the caller's `Resolver` fires
    // a spanned Error diagnostic; without this the LSP silently
    // drops the import (workspace.rs::collect_imports) and `vw
    // check` only surfaces the failure via the loader's hard-abort
    // path (no span). The check no-ops when `extra_dep_names` is
    // empty — unit tests and non-workspace callers skip it.
    validate_src_imports(document, extra_dep_names, &mut diags);
    validate_test_attributes(document, &mut diags);
    diags
}

/// `@test` semantic checks. Fires warnings (not errors) so
/// misused tags surface in the LSP + `vw check` without blocking
/// execution. Rules:
///
/// - `@test(X)` where X isn't the literal ident `dedicated-eda`
///   (the only recognized value today).
/// - `@test` on a proc with a non-empty parameter list — tests
///   are zero-arg for the MVP runner.
/// - `@test` on a nested proc (declared inside another proc's
///   body) — only top-level `@test` procs are discoverable by
///   `vw test`.
///
/// Doesn't check for `@test` on non-proc statements — that's
/// caught at parse time with a spanned error.
fn validate_test_attributes(document: &Document, diags: &mut Vec<Diagnostic>) {
    walk_procs_for_test_check(
        &document.stmts,
        /*inside_proc=*/ false,
        diags,
    );
}

fn walk_procs_for_test_check(
    stmts: &[Stmt],
    inside_proc: bool,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(attr) = proc.attribute("test") {
                    if inside_proc {
                        diags.push(Diagnostic {
                            severity: Severity::Warning,
                            message: "`@test` on a nested proc — only \
                                      top-level `@test`-annotated procs \
                                      are discoverable by `vw test`"
                                .into(),
                            span: attr.span,
                        });
                    }
                    let mut has_dedicated = false;
                    let mut target_key_present = false;
                    let mut variant_key_present = false;
                    for value in &attr.values {
                        match value {
                            crate::ast::AttributeValue::Ident {
                                value: v,
                                ..
                            } if v == "dedicated-eda" => {
                                has_dedicated = true;
                            }
                            crate::ast::AttributeValue::Keyed {
                                key, ..
                            } if key == "target" => {
                                target_key_present = true;
                            }
                            crate::ast::AttributeValue::Keyed {
                                key, ..
                            } if key == "variant" => {
                                variant_key_present = true;
                            }
                            crate::ast::AttributeValue::Keyed {
                                key, ..
                            } => {
                                diags.push(Diagnostic {
                                    severity: Severity::Warning,
                                    message: format!(
                                        "`@test(...)` — unrecognized key \
                                         `{key}` (recognized: `target=<part>`, \
                                         `variant=<name>`)"
                                    ),
                                    span: attr.span,
                                });
                            }
                            _ => {
                                diags.push(Diagnostic {
                                    severity: Severity::Warning,
                                    message: format!(
                                        "`@test(…)` value must be the \
                                         `dedicated-eda` marker, \
                                         `target=<part>`, or `variant=<name>`; \
                                         got `{}`",
                                        render_attribute_value(value),
                                    ),
                                    span: attr.span,
                                });
                            }
                        }
                    }
                    if target_key_present && !has_dedicated {
                        diags.push(Diagnostic {
                            severity: Severity::Warning,
                            message: "`@test(target=…)` requires the \
                                      `dedicated-eda` marker — shared-bucket \
                                      tests cannot override the \
                                      auto-project's `-part`"
                                .into(),
                            span: attr.span,
                        });
                    }
                    if variant_key_present && !has_dedicated {
                        diags.push(Diagnostic {
                            severity: Severity::Warning,
                            message: "`@test(variant=…)` requires the \
                                      `dedicated-eda` marker — shared-bucket \
                                      tests cannot switch design surfaces"
                                .into(),
                            span: attr.span,
                        });
                    }
                    if target_key_present && variant_key_present {
                        diags.push(Diagnostic {
                            severity: Severity::Warning,
                            message: "`@test(...)` — pick one of \
                                      `target=<part>` or `variant=<name>`, \
                                      not both (variants own their parts)"
                                .into(),
                            span: attr.span,
                        });
                    }
                    if let Some(sig) = &proc.signature {
                        if !sig.args.is_empty() {
                            diags.push(Diagnostic {
                                severity: Severity::Warning,
                                message: "`@test` procs must take zero \
                                          arguments — parameterized tests \
                                          aren't supported yet"
                                    .into(),
                                span: attr.span,
                            });
                        }
                    }
                }
                walk_procs_for_test_check(&proc.body, true, diags);
            }
            CommandKind::NamespaceEval(ns) => {
                walk_procs_for_test_check(&ns.body, inside_proc, diags);
            }
            _ => {}
        }
    }
}

fn render_attribute_value(v: &crate::ast::AttributeValue) -> String {
    v.to_tcl_literal()
}

/// Walk every top-level `src @<name>` statement in `document` and
/// emit an Error diagnostic for any `<name>` not present in
/// `known_deps`. Relative and absolute path imports (non-`@`
/// forms) are skipped — those get their existence checked
/// downstream by the loader's filesystem probe.
fn validate_src_imports(
    document: &Document,
    known_deps: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    // The empty-set case covers unit tests (no workspace) and
    // downstream callers that don't hook up a Resolver. Short-
    // circuit rather than walking every statement for nothing.
    if known_deps.is_empty() {
        return;
    }
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Src(src) = &cmd.kind else {
            continue;
        };
        // Missing path (contains `$var` / `[cmd]` substitution) —
        // handled by other passes; don't double-flag here.
        let Some(path) = src.path.as_deref() else {
            continue;
        };
        let classified = crate::src_path::classify(path);
        let crate::src_path::PathKind::Named { name, subpath } =
            classified.kind
        else {
            continue;
        };
        if known_deps.contains(&name) {
            continue;
        }
        // Message mirrors `ResolveError::UnknownDependency`'s text
        // in `src_path.rs` — same hint keeps the CLI hard-abort
        // path (which still fires) and the analyzer diagnostic
        // pointing at the same fix.
        let subpath_hint = if subpath.is_empty() {
            String::new()
        } else {
            format!("/{subpath}")
        };
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "unknown src module `{name}` in `src @{name}{subpath_hint}`; \
                 add a `[dependencies.{name}]` entry to your workspace's \
                 vw.toml or run `vw add` to fetch it"
            ),
            span: src.path_span,
        });
    }
}

/// Validate every command in `stmts`, descending into proc bodies so
/// that calls nested inside a proc are checked just like top-level
/// ones. The signature table is document-wide, so a call resolves to
/// its (top-level) proc at any depth.
/// Variable-type table keyed by name. Populated as `validate_stmts`
/// walks `set VAR <value>` and proc-parameter bindings; consulted by
/// `value_type` when it hits a `$var` reference at a call site.
///
/// Nominal / strict: entries hold the DECLARED `TypeExpr` (from a
/// proc's `return_type` or a `ProcArg.type_annotation`) without any
/// alias-walking. Comparing two entries via [`types_match`] gives
/// newtype identity — `Quad0Ch1Props` and `Quad1Ch0Props` are
/// distinct even though both alias to `Properties`.
///
/// Scope discipline: each proc body owns its own table (created in
/// the `CommandKind::Proc` arm of `validate_stmts`). Nested
/// `namespace eval` blocks share the enclosing table (matches Tcl
/// semantics — `namespace eval` creates a namespace but doesn't
/// open a new local-variable scope). Bodies of `[ … ]` command
/// substitutions also share the enclosing table so `set` inside
/// brackets is visible outside.
pub(crate) type VarTypeTable = HashMap<String, crate::ast::TypeExpr>;

/// Infer the type of a value word — the argument on the right of a
/// `-flag` at a call site, or the RHS of a `set VAR`.
///
/// Covers the two forms the call-site type-check actually needs:
///
/// - A whole-word command substitution `[proc-call …]` returns the
///   called proc's `return_type`. Multi-command bodies (`[a; b]`)
///   take the LAST command's return type — matches Tcl's "value of
///   the last command wins" for `[…]` substitution.
/// - A whole-word variable reference `$foo` / `${foo}` returns the
///   type recorded in `var_table` (from a prior `set` or a proc
///   parameter with a `type_annotation`).
///
/// Anything else — literals, quoted strings, mixed compounds like
/// `prefix-$var` — returns `None`. Callers treat `None` as "unknown
/// type, skip the check" (gradual typing). We don't error on what
/// we can't infer.
pub(crate) fn value_type(
    word: &crate::ast::Word,
    sig_table: &HashMap<String, &ProcSignature>,
    var_table: &VarTypeTable,
) -> Option<crate::ast::TypeExpr> {
    value_type_with_procs(word, sig_table, var_table, None)
}

/// Companion to [`value_type`] that also has access to a proc
/// table for return-type inference on unannotated procs. When a
/// `[proc-call]` word hits a signature whose `return_type` is
/// `None`, the caller can supply the corresponding [`Proc`] node
/// via `proc_table` and this function walks the body's last
/// `return` statement to infer the type — handles the common
/// pattern where a user proc doesn't declare a return type but
/// its body ends with `return $x` or `return [typed_proc]`.
pub(crate) fn value_type_with_procs(
    word: &crate::ast::Word,
    sig_table: &HashMap<String, &ProcSignature>,
    var_table: &VarTypeTable,
    proc_table: Option<&HashMap<String, &crate::ast::Proc>>,
) -> Option<crate::ast::TypeExpr> {
    use crate::ast::{Stmt, TypeExpr, WordPart};
    match word.parts.as_slice() {
        [WordPart::VarRef { name, .. }] => var_table.get(name).cloned(),
        [WordPart::CmdSubst { body, .. }] => {
            let last_cmd = body.iter().rev().find_map(|s| match s {
                Stmt::Command(c) => Some(c),
                _ => None,
            })?;
            let call_name = last_cmd.words.first()?.as_text()?;
            let sig = sig_table.get(call_name)?;
            // Annotated return type wins.
            if let Some(ty) = &sig.return_type {
                return Some(ty.clone());
            }
            // Fallback: walk the proc body to infer. Only fires
            // when a proc_table is supplied — top-level callers
            // (per-batch var-type builders, putr rewrite) pass one
            // through; internal callers that just want fast
            // annotation-based lookup pass `None`.
            let procs = proc_table?;
            let proc = procs.get(call_name)?;
            infer_return_type_from_body(proc, sig_table, procs)
        }
        // Bare `true` / `false` literals — the ONLY textual values
        // whose type we infer, and only because they're the
        // canonical HTCL bool literals. Everything else (bare
        // words, quoted strings, mixed compounds) stays untyped
        // (gradual typing). Position matters: this makes the
        // check symmetric so `set flag true` binds `flag: bool`
        // and a subsequent `-slot $flag` at a `bool` arg matches.
        [WordPart::Text { value, .. }]
            if value == "true" || value == "false" =>
        {
            Some(TypeExpr::Named {
                name: "bool".into(),
                span: word.span,
            })
        }
        _ => None,
    }
}

/// Validate that every `return X` statement in `proc`'s body
/// produces a value whose type matches the proc's declared
/// return type. Only fires when the proc has a `return_type`
/// annotation (untyped procs skip the check entirely).
///
/// Descends into the braced bodies of `if`/`elseif`/`else`/
/// `while`/`for`/`foreach`/`catch` — a `return` buried inside
/// an early-exit branch still gets checked. Nested control
/// blocks recurse through the same walker so an arbitrary
/// depth of `if { if { return X } }` still catches wrong-typed
/// returns.
///
/// Bare `return` (no argument) in an annotated proc is a hard
/// error — the annotation is a promise to produce a value.
pub(crate) fn validate_proc_returns(
    proc: &crate::ast::Proc,
    source: &str,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    newtype_names: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    // Newtype-triplet exemption: `T::from`, `T::to`, `T::repr`,
    // `T::empty` are compiler-emitted (or generator-emitted)
    // identity conversions between a newtype and its underlying.
    // Under strict nominal identity, `return $v` where `$v: string`
    // in a proc returning `T` would flag as a mismatch — but
    // that's the WHOLE POINT of the from/to/repr layer: cross
    // the newtype boundary via `return $v` (Tcl-level identity).
    // Skip the check when the proc name is `T::<suffix>` for a
    // declared newtype `T` and a triplet suffix. Applies to both
    // annotated and unannotated forms — hand-written triplets
    // often omit the annotation and rely on the identity-shape.
    if let Some(name) = proc.name.as_deref() {
        if is_newtype_triplet_name(name, newtype_names) {
            return;
        }
    }
    let Some(declared) = &proc.return_type else {
        // No return-type annotation → the proc is implicitly a
        // side-effect-only op. `return X` in a side-effect proc is
        // a structural mismatch: the caller has nothing to receive
        // it, and the missing annotation tells readers the same. Flag
        // every `return X` with a value. Bare `return` is fine.
        walk_returns_without_annotation(&proc.body, source, diags);
        return;
    };
    // Enum-overload-arm exemption: `proc f {v: E::A} string { … }`
    // is the specialization shape for the overload dispatcher —
    // `v` is the enum variant's payload, which at Tcl runtime is
    // just the underlying type's raw value (a `string` for
    // `E::A: string`). Returning it as its underlying is
    // structurally identity, same rationale as the newtype
    // triplet. Detect by first-arg type being `Qualified` —
    // that's the only shape overload arms use.
    if let Some(sig) = &proc.signature {
        if let Some(first) = sig.args.first() {
            if matches!(
                first.type_annotation,
                Some(crate::ast::TypeExpr::Qualified { .. })
            ) {
                return;
            }
        }
    }
    // Seed a var table with typed parameters. Walker updates it
    // as it visits `set` bindings so downstream `return $VAR`
    // resolves via the same scope-aware inference the outer
    // arg-type check uses.
    let mut local_vars = VarTypeTable::new();
    if let Some(sig) = &proc.signature {
        for arg in &sig.args {
            if let Some(ty) = &arg.type_annotation {
                local_vars.insert(arg.name.clone(), ty.clone());
            }
        }
    }
    walk_returns(
        &proc.body,
        source,
        sig_table,
        proc_table,
        &mut local_vars,
        declared,
        diags,
    );

    // Must-return: an annotated proc that isn't `unit` must
    // reach a `return` on every path (or end with a
    // last-expression whose type matches the annotation, per
    // Tcl's implicit-return rule). Runs AFTER walk_returns so
    // the per-return type errors surface first if both fire.
    let is_unit = matches!(
        declared,
        crate::ast::TypeExpr::Named { name, .. } if name == "unit"
    );
    if !is_unit {
        // walk_returns mutated local_vars — snapshot a fresh
        // seed for the must-return pass so we start from the
        // proc's typed parameters, matching the walker's own
        // starting state.
        let mut fresh_vars = VarTypeTable::new();
        if let Some(sig) = &proc.signature {
            for arg in &sig.args {
                if let Some(ty) = &arg.type_annotation {
                    fresh_vars.insert(arg.name.clone(), ty.clone());
                }
            }
        }
        if !paths_always_return(
            &proc.body,
            source,
            declared,
            sig_table,
            proc_table,
            &mut fresh_vars,
        ) {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "proc annotated `{}` may fall through without \
                     returning a value of the right type; every code \
                     path must end with `return $X` (or a final \
                     expression whose type matches)",
                    render_type_inline(declared),
                ),
                span: proc.name_span,
            });
        }
    }
}

/// True when `proc_name` matches the newtype-triplet pattern
/// `<T>::<suffix>` where `T` is a declared newtype and `suffix`
/// is one of `from`, `to`, `repr`, `empty`. These procs are
/// identity conversions across the newtype boundary, so their
/// `return $v` bodies would trip the strict-nominal check by
/// design; the check exempts them.
fn is_newtype_triplet_name(
    proc_name: &str,
    newtype_names: &std::collections::HashSet<String>,
) -> bool {
    for suffix in ["from", "to", "repr", "empty"] {
        let marker = format!("::{suffix}");
        if let Some(prefix) = proc_name.strip_suffix(&marker) {
            if newtype_names.contains(prefix) {
                return true;
            }
        }
    }
    false
}

/// Recursive walker used by [`validate_proc_returns`]. Tracks
/// `set` bindings, records `return X` statements it finds, and
/// descends into parsed control-flow bodies.
fn walk_returns(
    stmts: &[crate::ast::Stmt],
    source: &str,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    var_table: &mut VarTypeTable,
    declared: &crate::ast::TypeExpr,
    diags: &mut Vec<Diagnostic>,
) {
    use crate::ast::{Stmt, WordForm};
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // Track `set VAR <value>` bindings the same way the
        // rewrite walker does — later `return $VAR` needs to see
        // the type.
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
        // `return X` — the actual check.
        let head_text = cmd.words.first().and_then(|w| w.as_text());
        if head_text == Some("return") {
            check_return(
                cmd, sig_table, proc_table, var_table, declared, diags,
            );
        }
        // Descend into control-flow braced bodies. Heuristic:
        // for known control-flow heads, parse every WordForm::
        // Braced word as a candidate body. Condition-shaped
        // braces (like `if {$x == 1}`) parse without errors but
        // don't contain `return` calls, so they contribute
        // nothing — a benign no-op. `for INIT COND NEXT BODY`'s
        // INIT and NEXT can hold `set` calls that would affect
        // var_table if we tracked them; we don't, since the
        // walker isn't a live-execution simulator and Tcl's
        // control-flow scope semantics are already muddy.
        if matches!(
            head_text,
            Some(
                "if" | "elseif"
                    | "else"
                    | "while"
                    | "for"
                    | "foreach"
                    | "catch"
            )
        ) {
            for word in cmd.words.iter().skip(1) {
                if word.form != WordForm::Braced {
                    continue;
                }
                // The word's span covers `{...}` including the
                // outer braces. Strip 1 byte from each end for
                // the interior text; parse as a fragment; shift
                // spans by the interior start.
                let word_start = word.span.start as usize;
                let word_end = word.span.end as usize;
                if word_end <= word_start + 2 {
                    // `{}` — empty body, nothing to check.
                    continue;
                }
                let interior_start = word_start + 1;
                let interior_end = word_end - 1;
                let body_text = &source[interior_start..interior_end];
                let (mut body_stmts, _errs) = crate::parser::parse_fragment(
                    body_text,
                    crate::parser::Mode::Toplevel,
                );
                for s in &mut body_stmts {
                    crate::parser::shift_stmt(s, interior_start as u32);
                }
                // Populate procs INSIDE the parsed body so nested
                // structures behave — mostly irrelevant for
                // returns but keeps recursion consistent.
                crate::parser::populate_procs(
                    &mut body_stmts,
                    source,
                    &mut Vec::new(),
                );
                walk_returns(
                    &body_stmts,
                    source,
                    sig_table,
                    proc_table,
                    var_table,
                    declared,
                    diags,
                );
            }
        }
    }
}

/// Walk `stmts` looking for `return X` statements with a value in a
/// proc that has NO declared return type. Every such `return X` is
/// an error — the proc's shape declares "side effects only," and a
/// value-carrying return contradicts that.
///
/// Descends into control-flow braced bodies the same way
/// [`walk_returns`] does — a value-return buried in an `if`/`else`
/// branch is caught. Bare `return` is fine (side-effect procs
/// naturally use it for early exits).
fn walk_returns_without_annotation(
    stmts: &[crate::ast::Stmt],
    source: &str,
    diags: &mut Vec<Diagnostic>,
) {
    use crate::ast::{Stmt, WordForm};
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let head_text = cmd.words.first().and_then(|w| w.as_text());
        if head_text == Some("return") && cmd.words.len() >= 2 {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: "`return` with a value in a proc that has no \
                          declared return type — add a return-type \
                          annotation (`proc NAME { args } TYPE { ... }`) \
                          or drop the returned value"
                    .to_string(),
                span: cmd.span,
            });
        }
        if matches!(
            head_text,
            Some(
                "if" | "elseif"
                    | "else"
                    | "while"
                    | "for"
                    | "foreach"
                    | "catch"
            )
        ) {
            for word in cmd.words.iter().skip(1) {
                if word.form != WordForm::Braced {
                    continue;
                }
                let word_start = word.span.start as usize;
                let word_end = word.span.end as usize;
                if word_end <= word_start + 2 {
                    continue;
                }
                let interior_start = word_start + 1;
                let interior_end = word_end - 1;
                let body_text = &source[interior_start..interior_end];
                let (mut body_stmts, _errs) = crate::parser::parse_fragment(
                    body_text,
                    crate::parser::Mode::Toplevel,
                );
                for s in &mut body_stmts {
                    crate::parser::shift_stmt(s, interior_start as u32);
                }
                crate::parser::populate_procs(
                    &mut body_stmts,
                    source,
                    &mut Vec::new(),
                );
                walk_returns_without_annotation(&body_stmts, source, diags);
            }
        }
    }
}

/// Check a single `return X` (or bare `return`) against the
/// declared return type. Emits diagnostics directly.
fn check_return(
    cmd: &crate::ast::Command,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    var_table: &VarTypeTable,
    declared: &crate::ast::TypeExpr,
    diags: &mut Vec<Diagnostic>,
) {
    // `return` alone — bare — violates the annotation's promise
    // to produce a value UNLESS the declared type is `unit`,
    // which means "no meaningful value" and matches bare-return
    // semantics. Common in side-effecting procs that early-out.
    let Some(arg) = cmd.words.get(1) else {
        let is_unit = matches!(
            declared,
            crate::ast::TypeExpr::Named { name, .. } if name == "unit"
        );
        if !is_unit {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "bare `return` in proc annotated `{}` — the return \
                     type requires a value; use `return $X`",
                    render_type_inline(declared),
                ),
                span: cmd.span,
            });
        }
        return;
    };
    // Try to infer the returned expression's type. If we can't,
    // skip — gradual typing (same policy as the arg-type check).
    let Some(actual) =
        value_type_with_procs(arg, sig_table, var_table, Some(proc_table))
    else {
        return;
    };
    if !types_match(declared, &actual) {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "return type mismatch: proc declared `{}`, but this \
                 `return` produces `{}`",
                render_type_inline(declared),
                render_type_inline(&actual),
            ),
            span: cmd.span,
        });
    }
}

/// Whether every code path through `stmts` reaches an explicit
/// `return`, OR ends with a final statement whose result type
/// matches `declared` (Tcl's implicit-last-expression return).
///
/// Empty `stmts` → false. This is the primary failure case for
/// the must-return check: a proc annotated with a non-`unit`
/// return type whose body is completely empty (or ends with a
/// side-effecting `puts`) has no path that produces a value.
///
/// Descends into control-flow braced bodies via
/// `parser::parse_fragment` + `shift_stmt` — the same reparse
/// dance `walk_returns` already uses (see the identical pattern
/// at line ~570).
///
/// Coverage of control commands:
///
/// - `return` (any form) → this path terminates.
/// - `set VAR X` → records VAR's inferred type in `local_vars`
///   so a downstream implicit-last-expression `$VAR` gets typed.
/// - `if COND BODY [elseif COND BODY]* [else BODY]` → terminates
///   iff there IS an `else` AND every branch body terminates.
/// - `while` / `for` / `foreach` → conservative false (body may
///   not execute at runtime).
/// - `switch X { pat body pat body … [default body] }` →
///   terminates iff a `default` arm exists AND every arm's body
///   terminates. Fallthrough arms (`pat -`) inherit the next
///   arm's body.
/// - `catch` → conservative false (body may error out).
/// - Anything else → non-terminating individually; keep scanning.
fn paths_always_return(
    stmts: &[Stmt],
    source: &str,
    declared: &crate::ast::TypeExpr,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    local_vars: &mut VarTypeTable,
) -> bool {
    if stmts.is_empty() {
        return false;
    }
    let mut last_cmd_index: Option<usize> = None;
    for (i, stmt) in stmts.iter().enumerate() {
        let Stmt::Command(cmd) = stmt else { continue };
        let head_text = cmd.words.first().and_then(|w| w.as_text());

        // Track `set VAR <value>` bindings so a trailing `$VAR`
        // (implicit last-expression) sees the right type.
        if matches!(cmd.kind, CommandKind::Set) {
            if let (Some(name_word), Some(value_word)) =
                (cmd.words.get(1), cmd.words.get(2))
            {
                if let Some(name) = name_word.as_text() {
                    if let Some(ty) = value_type_with_procs(
                        value_word,
                        sig_table,
                        local_vars,
                        Some(proc_table),
                    ) {
                        local_vars.insert(name.to_string(), ty);
                    }
                }
            }
        }
        last_cmd_index = Some(i);

        // Explicit `return` — the path terminates here.
        if head_text == Some("return") {
            return true;
        }
        // `error` — unwinds the stack, so control never falls
        // through. Counts as terminating for the must-return
        // analysis (the proc can't reach its end after this).
        if head_text == Some("error") {
            return true;
        }
        // `if` — check if all branches terminate AND an `else` exists.
        if head_text == Some("if")
            && if_command_terminates(
                cmd, source, declared, sig_table, proc_table, local_vars,
            )
        {
            return true;
        }
        // `switch` — check for `default` arm and all-arm termination.
        if head_text == Some("switch")
            && switch_command_terminates(
                cmd, source, declared, sig_table, proc_table, local_vars,
            )
        {
            return true;
        }
        // `try { body } [on ... handler]* [finally script]` —
        // terminates iff the body AND every handler
        // path-terminate. This is what the generator's
        // wrap-body pattern emits (`try { return X } on error
        // { error "prefix.$msg" }`).
        if head_text == Some("try")
            && try_command_terminates(
                cmd, source, declared, sig_table, proc_table, local_vars,
            )
        {
            return true;
        }
        // `while` / `for` / `foreach` / `catch` — never
        // guaranteed to run their body, so they can't be sole
        // terminators. Keep scanning.
    }

    // Implicit-last-expression rule: if we've made it here
    // without hitting a `return`, look at the last command's
    // last word. If that word's type matches `declared`, this
    // path counts as terminating.
    let Some(last_idx) = last_cmd_index else {
        return false;
    };
    let Stmt::Command(last_cmd) = &stmts[last_idx] else {
        return false;
    };
    let expr_word = if last_cmd.words.len() == 1 {
        last_cmd.words.first()
    } else {
        // A multi-word command (e.g. `set _ [...]; $_`) parses
        // as a single `$_` command with one word — but a
        // multi-word command like `puts $x` has 2 words. Use
        // the FIRST word only when there's a single word (a
        // pure `$var` or `[proc-call]` expression); otherwise
        // treat the last statement as an expression via its
        // *head* word only when that head IS a proc-call —
        // handled below.
        last_cmd.words.first()
    };
    let Some(expr_word) = expr_word else {
        return false;
    };
    // For a bare command like `some_typed_proc arg1 arg2`, we
    // want to check the CALL's return type. `value_type_with_procs`
    // won't help directly on the head word (it's a bare-text
    // word, not a CmdSubst). But we CAN look up the head in the
    // sig_table. If the head is a known proc AND its return
    // type matches `declared`, treat as implicit return.
    if let Some(head) = last_cmd.words.first().and_then(|w| w.as_text()) {
        // `extern::name` is the caller's opt-out: "this is a raw
        // Tcl proc; I'm not declaring its type, trust me." Same
        // policy as `validate_command`'s check at line 1946.
        // Trailing extern call = trust for the must-return check.
        if crate::lower::is_extern_call(head) {
            return true;
        }
        if let Some(sig) = sig_table.get(head) {
            if let Some(ret_ty) = &sig.return_type {
                if types_match(declared, ret_ty) {
                    return true;
                }
            }
        }
    }
    // Otherwise: try value_type_with_procs on the expression
    // word itself (handles `$var` and `[proc-call]` shapes).
    if let Some(ty) = value_type_with_procs(
        expr_word,
        sig_table,
        local_vars,
        Some(proc_table),
    ) {
        if types_match(declared, &ty) {
            return true;
        }
    }
    false
}

/// Does a single `if COND BODY [elseif COND BODY]* [else BODY]`
/// command terminate? Yes iff every branch body terminates AND
/// there IS an `else` (a chain with no `else` may fall through).
///
/// Word layout, positions counted from 0:
/// - [0] = "if"
/// - [1] = condition
/// - [2] = body
/// - [3] = "elseif" | "else" (or end)
/// - [4] = condition (after elseif) | body (after else)
/// - …
fn if_command_terminates(
    cmd: &crate::ast::Command,
    source: &str,
    declared: &crate::ast::TypeExpr,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    local_vars: &VarTypeTable,
) -> bool {
    let mut i = 1;
    let mut has_else = false;
    let mut branch_bodies: Vec<&crate::ast::Word> = Vec::new();
    while i < cmd.words.len() {
        // Skip the condition word.
        if i + 1 >= cmd.words.len() {
            return false;
        }
        branch_bodies.push(&cmd.words[i + 1]);
        i += 2;
        if i >= cmd.words.len() {
            break;
        }
        match cmd.words[i].as_text() {
            Some("elseif") => {
                i += 1;
                // Loop continues with i at condition position.
            }
            Some("else") => {
                has_else = true;
                i += 1;
                if i >= cmd.words.len() {
                    return false;
                }
                branch_bodies.push(&cmd.words[i]);
                break;
            }
            _ => {
                // Something unexpected — conservative: don't
                // treat as terminating.
                return false;
            }
        }
    }
    if !has_else {
        return false;
    }
    for body_word in branch_bodies {
        if !branch_body_terminates(
            body_word, source, declared, sig_table, proc_table, local_vars,
        ) {
            return false;
        }
    }
    true
}

/// Does a `switch X { pat body … [default body] }` terminate?
/// Yes iff there's a `default` arm AND every arm's body
/// terminates. Fallthrough arms (`pat -` where the body word is
/// literally `-`) inherit the next arm's body.
fn switch_command_terminates(
    cmd: &crate::ast::Command,
    source: &str,
    declared: &crate::ast::TypeExpr,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    local_vars: &VarTypeTable,
) -> bool {
    // Find the switch body — the LAST braced word in the command
    // (the argument list before it is `[options] value`, which
    // we don't parse).
    let body_word = cmd
        .words
        .iter()
        .rev()
        .find(|w| w.form == crate::ast::WordForm::Braced);
    let Some(body_word) = body_word else {
        return false;
    };
    let word_start = body_word.span.start as usize;
    let word_end = body_word.span.end as usize;
    if word_end <= word_start + 2 {
        return false;
    }
    let interior_start = word_start + 1;
    let interior_end = word_end - 1;
    let body_text = &source[interior_start..interior_end];
    let (mut arm_stmts, _errs) =
        crate::parser::parse_fragment(body_text, crate::parser::Mode::Toplevel);
    for s in &mut arm_stmts {
        crate::parser::shift_stmt(s, interior_start as u32);
    }
    // Each arm parses as one command with words = [pat, body].
    // Collect them as pairs, resolving fallthrough (`pat -`)
    // arms to the next arm's body.
    let mut pairs: Vec<(String, &crate::ast::Word)> = Vec::new();
    let mut pending_pats: Vec<String> = Vec::new();
    for stmt in &arm_stmts {
        let Stmt::Command(arm_cmd) = stmt else {
            continue;
        };
        if arm_cmd.words.len() < 2 {
            return false;
        }
        let pat = match arm_cmd.words[0].as_text() {
            Some(p) => p.to_string(),
            None => return false,
        };
        let body_arg = &arm_cmd.words[1];
        if body_arg.as_text() == Some("-") {
            // Fallthrough: this pattern inherits the next
            // resolved arm's body.
            pending_pats.push(pat);
            continue;
        }
        // Resolve pending fallthroughs to this arm's body too.
        for pending in pending_pats.drain(..) {
            pairs.push((pending, body_arg));
        }
        pairs.push((pat, body_arg));
    }
    if !pending_pats.is_empty() {
        // Trailing `pat -` without a resolving arm — malformed.
        return false;
    }
    // Must have a `default` arm.
    let has_default = pairs.iter().any(|(p, _)| p == "default");
    if !has_default {
        return false;
    }
    for (_pat, body_word) in &pairs {
        if !branch_body_terminates(
            body_word, source, declared, sig_table, proc_table, local_vars,
        ) {
            return false;
        }
    }
    true
}

/// Does a `try BODY [on CODE VAR HANDLER]* [trap PATS VAR HANDLER]*
/// [finally SCRIPT]` terminate? Yes iff BODY terminates AND every
/// handler body terminates. `finally` doesn't participate in
/// termination (it runs REGARDLESS of what the body/handlers did,
/// so it can't turn a non-terminating structure into a terminating
/// one — but it also doesn't invalidate one).
fn try_command_terminates(
    cmd: &crate::ast::Command,
    source: &str,
    declared: &crate::ast::TypeExpr,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    local_vars: &VarTypeTable,
) -> bool {
    // Body is the first argument (word[1]).
    let Some(body_word) = cmd.words.get(1) else {
        return false;
    };
    if !branch_body_terminates(
        body_word, source, declared, sig_table, proc_table, local_vars,
    ) {
        return false;
    }
    // Walk the remaining words looking for handler bodies. Each
    // `on CODE VAR HANDLER` or `trap PATS VAR HANDLER` clause
    // occupies 4 words; each `finally SCRIPT` occupies 2.
    let mut i = 2;
    while i < cmd.words.len() {
        let head = cmd.words[i].as_text();
        match head {
            Some("on") | Some("trap") => {
                // handler body is at position i+3.
                let Some(handler_body) = cmd.words.get(i + 3) else {
                    return false;
                };
                if !branch_body_terminates(
                    handler_body,
                    source,
                    declared,
                    sig_table,
                    proc_table,
                    local_vars,
                ) {
                    return false;
                }
                i += 4;
            }
            Some("finally") => {
                // Finally script doesn't affect the terminator
                // analysis — skip past its word.
                i += 2;
            }
            _ => {
                // Malformed / something we don't recognize;
                // conservative false.
                return false;
            }
        }
    }
    true
}

/// Does the branch body word (a `WordForm::Braced` script)
/// terminate? Reparses the interior as a fragment and delegates
/// to `paths_always_return`. Non-braced body words (unusual —
/// only shows up when the parser hits malformed input) → false.
fn branch_body_terminates(
    body_word: &crate::ast::Word,
    source: &str,
    declared: &crate::ast::TypeExpr,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    local_vars: &VarTypeTable,
) -> bool {
    if body_word.form != crate::ast::WordForm::Braced {
        return false;
    }
    let word_start = body_word.span.start as usize;
    let word_end = body_word.span.end as usize;
    if word_end <= word_start + 2 {
        return false;
    }
    let interior_start = word_start + 1;
    let interior_end = word_end - 1;
    let body_text = &source[interior_start..interior_end];
    let (mut body_stmts, _errs) =
        crate::parser::parse_fragment(body_text, crate::parser::Mode::Toplevel);
    for s in &mut body_stmts {
        crate::parser::shift_stmt(s, interior_start as u32);
    }
    crate::parser::populate_procs(&mut body_stmts, source, &mut Vec::new());
    // Branch bodies share the outer scope's var table (Tcl
    // control-flow doesn't create a new frame).
    let mut branch_vars = local_vars.clone();
    paths_always_return(
        &body_stmts,
        source,
        declared,
        sig_table,
        proc_table,
        &mut branch_vars,
    )
}

/// Walk `proc`'s body left-to-right, tracking `set VAR <value>`
/// bindings via [`value_type_with_procs`], then find the last
/// `return X` statement and resolve `X`'s type. `None` when the
/// body doesn't end with an inferrable return.
///
/// Recursion depth is bounded implicitly by the finite proc-set
/// in a document: we don't cache visited procs here since v1
/// documents don't hit deep recursion in practice. If a real
/// program starts driving this in circles, add a `HashSet<&str>`
/// guard on the proc name.
fn infer_return_type_from_body(
    proc: &crate::ast::Proc,
    sig_table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
) -> Option<crate::ast::TypeExpr> {
    use crate::ast::{CommandKind, Stmt};
    let mut local_vars = VarTypeTable::new();
    // Seed the local var table with the proc's typed parameters —
    // so a body like `proc pass_through {x: MyType} { return $x }`
    // resolves through `$x` to `MyType`.
    if let Some(sig) = &proc.signature {
        for arg in &sig.args {
            if let Some(ty) = &arg.type_annotation {
                local_vars.insert(arg.name.clone(), ty.clone());
            }
        }
    }
    // Walk `set` bindings in body order.
    let mut return_word: Option<&crate::ast::Word> = None;
    for stmt in &proc.body {
        let Stmt::Command(cmd) = stmt else { continue };
        // Track `set VAR <value>`.
        if matches!(cmd.kind, CommandKind::Set) {
            if let (Some(name_word), Some(value_word)) =
                (cmd.words.get(1), cmd.words.get(2))
            {
                if let Some(name) = name_word.as_text() {
                    if let Some(ty) = value_type_with_procs(
                        value_word,
                        sig_table,
                        &local_vars,
                        Some(proc_table),
                    ) {
                        local_vars.insert(name.to_string(), ty);
                    }
                }
            }
        }
        // Track the last `return <word>` we see. Body execution
        // ordinarily halts at `return`, but syntactically it's
        // valid to have more code after (dead code); take the
        // LAST occurrence since that's what the user's intent
        // most likely reflects when reading the body.
        if cmd.words.first().and_then(|w| w.as_text()) == Some("return") {
            return_word = cmd.words.get(1);
        }
    }
    let word = return_word?;
    value_type_with_procs(word, sig_table, &local_vars, Some(proc_table))
}

/// Build a name → `Proc` lookup for return-type inference. Walks
/// top-level statements plus namespace-eval bodies (using the
/// namespace as a `<ns>::<name>` prefix, matching how
/// [`build_signature_table`] qualifies proc names).
pub(crate) fn build_proc_table(
    document: &crate::ast::Document,
) -> HashMap<String, &crate::ast::Proc> {
    let mut out = HashMap::new();
    collect_procs(&document.stmts, "", &mut out);
    out
}

fn collect_procs<'doc>(
    stmts: &'doc [Stmt],
    prefix: &str,
    out: &mut HashMap<String, &'doc crate::ast::Proc>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(name) = proc.name.as_deref() {
                    let qualified = qualify(prefix, name);
                    // Later `proc` shadows earlier — same as sig_table.
                    out.insert(qualified, proc);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let ns_name = ns.name.as_deref().unwrap_or("");
                let new_prefix = qualify(prefix, ns_name);
                collect_procs(&ns.body, &new_prefix, out);
            }
            _ => {}
        }
    }
}

/// True when a `TypeExpr` names the HTCL `bool` primitive. Kept
/// as a helper (rather than inlined) so the check has a single
/// point of change if we ever alias `bool` under a namespace
/// (e.g. `htcl::bool`).
fn is_bool_type(ty: &crate::ast::TypeExpr) -> bool {
    matches!(
        ty,
        crate::ast::TypeExpr::Named { name, .. } if name == "bool"
    )
}

fn validate_stmts(
    stmts: &[Stmt],
    source: &str,
    table: &HashMap<String, &ProcSignature>,
    proc_table: &HashMap<String, &crate::ast::Proc>,
    newtype_names: &std::collections::HashSet<String>,
    var_table: &mut VarTypeTable,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // Bind `set VAR <value>` into the var table BEFORE
        // recursing — so downstream `$VAR` references in the same
        // scope see the type. `validate_command` bails on
        // `CommandKind::Set` (not a "call"), so this is the only
        // place set-binding is observed.
        if matches!(cmd.kind, CommandKind::Set) {
            if let (Some(name_word), Some(value_word)) =
                (cmd.words.get(1), cmd.words.get(2))
            {
                if let Some(name) = name_word.as_text() {
                    if let Some(ty) = value_type(value_word, table, var_table) {
                        var_table.insert(name.to_string(), ty);
                    }
                }
            }
        }
        validate_command(cmd, source, table, var_table, diags);
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                // Return-type check: fires only when the proc has
                // an annotated return type. Every `return X` in
                // the body (including inside control-flow braced
                // bodies) must produce a value whose inferred
                // type matches the annotation.
                validate_proc_returns(
                    proc,
                    source,
                    table,
                    proc_table,
                    newtype_names,
                    diags,
                );
                // Fresh scope per proc body. Seed with typed
                // parameters so `-slot $arg` inside the body knows
                // `arg`'s declared type without the caller having
                // to `set` it locally.
                let mut proc_scope = VarTypeTable::new();
                if let Some(sig) = &proc.signature {
                    for a in &sig.args {
                        if let Some(ty) = &a.type_annotation {
                            proc_scope.insert(a.name.clone(), ty.clone());
                        }
                    }
                }
                validate_stmts(
                    &proc.body,
                    source,
                    table,
                    proc_table,
                    newtype_names,
                    &mut proc_scope,
                    diags,
                );
            }
            CommandKind::NamespaceEval(ns) => {
                // Calls inside the namespace body are validated the
                // same way; the signature-table is document-wide so
                // a call to `project::set_target_language` from
                // anywhere resolves to the same entry. (Bare,
                // sibling-relative calls inside a namespace body
                // aren't auto-qualified yet — write the qualified
                // name explicitly.) Var scope is shared with the
                // enclosing frame, matching Tcl's rule that
                // `namespace eval` creates a namespace but not a
                // fresh local-variable scope.
                validate_stmts(
                    &ns.body,
                    source,
                    table,
                    proc_table,
                    newtype_names,
                    var_table,
                    diags,
                );
            }
            _ => {}
        }
        // Also descend into any `[ … ]` command substitutions on this
        // command's words so calls written inline get validated the
        // same as top-level ones. Var-table shared with the
        // enclosing scope — a `set X …` inside `[…]` is visible
        // outside, per Tcl.
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    validate_stmts(
                        body,
                        source,
                        table,
                        proc_table,
                        newtype_names,
                        var_table,
                        diags,
                    );
                }
            }
        }
    }
}

/// Build a name → signature map from every proc declaration in
/// the document, including those nested inside `namespace eval`
/// blocks (which register under `<ns>::<proc>`, matching Tcl's
/// namespace semantics). Duplicate names raise a diagnostic and the
/// later declaration wins, again matching Tcl (a second `proc`
/// redefines).
pub fn build_signature_table<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> HashMap<String, &'doc ProcSignature> {
    let (table, _overloads) = build_signature_table_with_overloads(
        document,
        &std::collections::HashSet::new(),
        diags,
    );
    table
}

/// Same as [`build_signature_table`] but also returns the
/// [`OverloadTable`] side-map. Callers that need to know whether a
/// given proc name resolves through enum-overload dispatch (codegen,
/// hover, signature help) consult this.
pub fn build_signature_table_with_overloads<'doc>(
    document: &'doc Document,
    newtype_qualified_names: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) -> (HashMap<String, &'doc ProcSignature>, OverloadTable) {
    // First pass: collect every proc decl per qualified name,
    // preserving order so a "first wins" / "last wins" choice is
    // unambiguous when we have to make one. Multi-decl entries are
    // candidate overload sets; single-decl entries are normal
    // procs.
    let mut multi: HashMap<String, Vec<(&'doc Proc, &'doc ProcSignature)>> =
        HashMap::new();
    collect_signatures_multi(
        &document.stmts,
        "",
        newtype_qualified_names,
        &mut multi,
        diags,
    );

    let mut table: HashMap<String, &'doc ProcSignature> = HashMap::new();
    let mut overloads: OverloadTable = HashMap::new();

    for (qualified, decls) in multi {
        match decls.len() {
            0 => { /* impossible */ }
            1 => {
                let (proc, sig) = decls[0];
                check_reserved_proc_name(&qualified, proc.name_span, diags);
                table.insert(qualified, sig);
            }
            _ => {
                // Multi-decl: classify as enum-overload OR emit
                // hard error for ad-hoc overloading.
                match classify_overload_set(&qualified, &decls, diags) {
                    Some(info) => {
                        // Each specialization registers under its
                        // mangled name so analyzer drill-down + the
                        // dispatcher's runtime switch can find it.
                        for v in &info.variants {
                            // Find the decl whose first arg is this
                            // variant. We computed the mangled name
                            // from it during classify, so the order
                            // matches by construction.
                            for (_proc, sig) in &decls {
                                let Some(first) = sig.args.first() else {
                                    continue;
                                };
                                if matches!(
                                    &first.type_annotation,
                                    Some(TypeExpr::Qualified { variant, .. })
                                        if variant == &v.variant_name
                                ) {
                                    // Mangled names are compiler-
                                    // generated — they're allowed to
                                    // start with `__` (that's the
                                    // whole point). Skip the
                                    // reserved-name check here.
                                    table.insert(
                                        v.mangled_proc_name.clone(),
                                        sig,
                                    );
                                }
                            }
                        }
                        // Public name resolves to the first overload's
                        // sig as a representative. Analyzer / callers
                        // that want the "true" public interface
                        // consult `overloads`.
                        let (proc, sig) = decls[0];
                        check_reserved_proc_name(
                            &qualified,
                            proc.name_span,
                            diags,
                        );
                        table.insert(qualified.clone(), sig);
                        overloads.insert(qualified, info);
                    }
                    None => {
                        // classify_overload_set already emitted the
                        // diagnostic; for table consistency, fall
                        // back to "last wins" so downstream
                        // validation keeps working. Check the
                        // reserved prefix on each.
                        let (proc, sig) = *decls.last().unwrap();
                        check_reserved_proc_name(
                            &qualified,
                            proc.name_span,
                            diags,
                        );
                        table.insert(qualified, sig);
                    }
                }
            }
        }
    }

    (table, overloads)
}

/// User procs whose qualified name starts with `__` would collide
/// with the compiler's overload-specialization mangling
/// (`__<public>__<Variant>`). Reject them up front.
fn check_reserved_proc_name(
    qualified: &str,
    name_span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    // Look at the last segment after the final `::`. Tcl's
    // namespace separator is part of the qualified name, so e.g.
    // `vivado_cmd::__foo` has its "leaf" name as `__foo` — the
    // collision risk is on the leaf, not the prefix.
    let leaf = qualified.rsplit("::").next().unwrap_or(qualified);
    if leaf.starts_with("__") {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "proc name `{qualified}` is reserved: names starting with \
                 `__` are used by the compiler for overload-specialization \
                 mangling (e.g. `__handle_prop__Scalar`). Rename to avoid \
                 collisions."
            ),
            span: name_span,
        });
    }
}

fn collect_signatures_multi<'doc>(
    stmts: &'doc [Stmt],
    prefix: &str,
    newtype_qualified_names: &std::collections::HashSet<String>,
    multi: &mut HashMap<String, Vec<(&'doc Proc, &'doc ProcSignature)>>,
    diags: &mut Vec<Diagnostic>,
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
                // v1 restriction: enum-overloaded procs must be
                // declared at the top level. Inside a `namespace
                // eval` block, the REPL's batch-prepare layer
                // doesn't re-route to the mangled-name + dispatcher
                // pipeline, so an overload arm inside a namespace
                // would silently lose its dispatch semantics.
                //
                // Only ENUM-Qualified first args (`Foo::Variant` where
                // `Foo` is a declared enum) count as overload-arm
                // shape. A `Qualified` type that resolves to a
                // declared NEWTYPE (e.g. `-config: versal_cips::Config`
                // in a `namespace eval versal_cips { proc create … }`
                // block) is just a typed newtype ref — legal
                // everywhere. The newtype-set peeked in by the caller
                // disambiguates.
                let overload_shape_first = sig
                    .args
                    .first()
                    .and_then(|a| a.type_annotation.as_ref())
                    .and_then(|t| match t {
                        TypeExpr::Qualified {
                            namespace, variant, ..
                        } => Some(format!("{namespace}::{variant}")),
                        _ => None,
                    })
                    .filter(|qname| !newtype_qualified_names.contains(qname))
                    .is_some();
                if overload_shape_first && !prefix.is_empty() {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "overloaded proc `{name}` is declared inside \
                             `namespace eval {prefix}` — v1 enum-overloads \
                             must be declared at the top level. Move the \
                             overload arms out of the namespace block."
                        ),
                        span: proc.name_span,
                    });
                    continue;
                }
                let qualified = qualify(prefix, name);
                multi.entry(qualified).or_default().push((proc, sig));
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                // `extern` is reserved by htcl's lowering as the
                // prefix for runtime-Tcl-proc disambiguation
                // (`extern::foo` → `__vw_extern_foo`). A user-
                // defined namespace named `extern` would silently
                // collide with that rewrite at call sites; reject
                // it up front.
                if name == "extern" {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: "`extern` is a reserved namespace name in \
                             htcl (used for runtime-Tcl-proc \
                             disambiguation); pick a different name"
                            .into(),
                        span: ns.name_span,
                    });
                    continue;
                }
                let nested = qualify(prefix, name);
                collect_signatures_multi(
                    &ns.body,
                    &nested,
                    newtype_qualified_names,
                    multi,
                    diags,
                );
            }
            _ => {}
        }
    }
}

/// Classify a multi-decl proc-name set. Returns `Some(OverloadInfo)`
/// if every member's first arg is a distinct variant of the same
/// enum AND the tail args / return type agree; returns `None` and
/// emits a diagnostic if it's not a valid overload (ad-hoc
/// overloading, missing variant, tail mismatch, etc.).
fn classify_overload_set<'doc>(
    public_name: &str,
    decls: &[(&'doc Proc, &'doc ProcSignature)],
    diags: &mut Vec<Diagnostic>,
) -> Option<OverloadInfo> {
    // Each decl's first arg must be `Qualified { namespace: E, variant: V }`.
    // Collect (enum_name, variant_name, dispatch_arg_span) per decl.
    let mut dispatch_infos: Vec<(String, String, Span, &Proc, &ProcSignature)> =
        Vec::with_capacity(decls.len());
    for (proc, sig) in decls {
        let Some(first) = sig.args.first() else {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "proc `{public_name}` is declared multiple times; for \
                     this to be a valid enum-overload set, every \
                     declaration's first argument must be annotated with \
                     a qualified variant type like `E::V`. This one has \
                     no arguments."
                ),
                span: proc.name_span,
            });
            return None;
        };
        match &first.type_annotation {
            Some(TypeExpr::Qualified {
                namespace, variant, ..
            }) => {
                dispatch_infos.push((
                    namespace.clone(),
                    variant.clone(),
                    first.name_span,
                    proc,
                    sig,
                ));
            }
            _ => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "proc `{public_name}` is declared multiple times \
                         with first-arg types that aren't all variants \
                         of a common enum; ad-hoc overloading on arbitrary \
                         types is not supported. Use an enum or rename \
                         one of the procs."
                    ),
                    span: first.name_span,
                });
                return None;
            }
        }
    }
    // All overloads must dispatch on the same enum.
    let enum_name = dispatch_infos[0].0.clone();
    for (ns, _, sp, _, _) in &dispatch_infos[1..] {
        if ns != &enum_name {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "overload set for proc `{public_name}` mixes enums: \
                     `{enum_name}` and `{ns}`. All overloads in a set \
                     must dispatch on the same enum."
                ),
                span: *sp,
            });
            return None;
        }
    }
    // Variants must be distinct.
    {
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for (_, v, sp, _, _) in &dispatch_infos {
            if !seen.insert(v.as_str()) {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "overload set for proc `{public_name}` has two \
                         arms dispatching on the same variant \
                         `{enum_name}::{v}`. Each variant must have at \
                         most one arm."
                    ),
                    span: *sp,
                });
                return None;
            }
        }
    }
    // Tail-arg agreement: v1 restricts every arm to exactly one
    // arg (the dispatched variant). Multi-arg overloads are
    // future work — kwargs / specialization-binding interactions
    // get hairy and the property-display motivating case doesn't
    // need them.
    let (_, _, _, first_proc, first_sig) = &dispatch_infos[0];
    if first_sig.args.len() != 1 {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "overload arm `{public_name}` declares {} args; v1 \
                 enum-overloads support exactly ONE arg (the dispatched \
                 variant). Additional tail args are future work — model \
                 the tail as a payload field on the enum variant for now.",
                first_sig.args.len()
            ),
            span: first_sig.span,
        });
        return None;
    }
    for (ns, v, _, _, sig) in &dispatch_infos[1..] {
        if sig.args.len() != 1 {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "overload arm `{public_name}` for `{ns}::{v}` declares \
                     {} args; v1 enum-overloads support exactly ONE arg \
                     (the dispatched variant).",
                    sig.args.len()
                ),
                span: sig.span,
            });
            return None;
        }
    }
    // Return-type agreement: every annotated return type must match.
    // Mixed annotated/unannotated → error.
    let first_ret = first_sig.return_type.as_ref();
    for (ns, v, _, _, sig) in &dispatch_infos[1..] {
        match (first_ret, sig.return_type.as_ref()) {
            (None, None) => {}
            (Some(a), Some(b)) if types_match(a, b) => {}
            _ => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "overload arm `{public_name}` for `{ns}::{v}` \
                         declares a different return type than the other \
                         arms. All arms must agree on the return type \
                         (annotate every arm with the same type, or none)."
                    ),
                    span: sig.span,
                });
                return None;
            }
        }
    }
    // Arg-name agreement: every arm must use the same first-arg
    // name so the dispatcher can pass the payload via kwargs as
    // `-<shared_name> <payload>`. Cheaper than per-arm dispatch
    // tracking and matches user convention (everyone writes `v`).
    let dispatch_arg_name = first_sig.args[0].name.clone();
    for (ns, v, _, _, sig) in &dispatch_infos[1..] {
        if sig.args[0].name != dispatch_arg_name {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "overload arm `{public_name}` for `{ns}::{v}` names its \
                     dispatch arg `{}`; other arms name it `{dispatch_arg_name}`. \
                     All arms must use the same arg name (convention: `v`).",
                    sig.args[0].name
                ),
                span: sig.args[0].name_span,
            });
            return None;
        }
    }
    // Build the OverloadInfo. Variant order matches source order
    // of the overloads.
    let variants = dispatch_infos
        .iter()
        .map(|(_, v, sp, _, _)| OverloadVariant {
            variant_name: v.clone(),
            mangled_proc_name: mangle_specialization(public_name, v),
            dispatch_arg_span: *sp,
        })
        .collect();
    Some(OverloadInfo {
        public_name: public_name.to_string(),
        enum_name,
        dispatch_arg_name,
        variants,
        anchor_span: first_proc.name_span,
    })
}

// `tails_match` / `attr_values_equal` lived here for the multi-arg
// overload tail-agreement check. v1 restricts overloads to a single
// arg (see `classify_overload_set`), so we don't compare tails. The
// helpers are kept as a record in git history; restore when adding
// multi-arg overloads.

/// Collect every `type NAME = UNDERLYING` declaration in `document`,
/// qualified by enclosing `namespace eval` prefix (so a `type widget`
/// declared inside `namespace eval foo {}` registers as `foo::widget`,
/// matching how procs already qualify). Duplicate declarations emit
/// a warning and the later one wins — same shape as duplicate-proc
/// handling above.
pub fn build_type_decl_table<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> HashMap<String, &'doc TypeDecl> {
    let mut table = HashMap::new();
    collect_type_decls(&document.stmts, "", &mut table, diags);
    table
}

fn collect_type_decls<'doc>(
    stmts: &'doc [Stmt],
    prefix: &str,
    table: &mut HashMap<String, &'doc TypeDecl>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::TypeDecl(td) => {
                let Some(name) = td.name.as_deref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                if table.insert(qualified.clone(), td).is_some() {
                    diags.push(Diagnostic {
                        severity: Severity::Warning,
                        message: format!(
                            "duplicate definition of type {qualified}; \
                             later definition wins"
                        ),
                        span: td.name_span,
                    });
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                if name == "extern" {
                    continue;
                }
                let nested = qualify(prefix, name);
                collect_type_decls(&ns.body, &nested, table, diags);
            }
            CommandKind::Proc(proc) => {
                // Nested type decls inside proc bodies are unusual
                // but not illegal — walk them so they register.
                collect_type_decls(&proc.body, prefix, table, diags);
            }
            _ => {}
        }
    }
}

/// Mirror of [`build_type_decl_table`] for `enum NAME = { ... }`
/// declarations. Duplicate enums warn and the later one wins —
/// same shape as type-decl handling.
pub fn build_enum_decl_table<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> HashMap<String, &'doc EnumDecl> {
    let mut table = HashMap::new();
    collect_enum_decls(&document.stmts, "", &mut table, diags);
    table
}

fn collect_enum_decls<'doc>(
    stmts: &'doc [Stmt],
    prefix: &str,
    table: &mut HashMap<String, &'doc EnumDecl>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::EnumDecl(ed) => {
                let Some(name) = ed.name.as_deref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                if table.insert(qualified.clone(), ed).is_some() {
                    diags.push(Diagnostic {
                        severity: Severity::Warning,
                        message: format!(
                            "duplicate definition of enum {qualified}; \
                             later definition wins"
                        ),
                        span: ed.name_span,
                    });
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                if name == "extern" {
                    continue;
                }
                let nested = qualify(prefix, name);
                collect_enum_decls(&ns.body, &nested, table, diags);
            }
            CommandKind::Proc(proc) => {
                collect_enum_decls(&proc.body, prefix, table, diags);
            }
            _ => {}
        }
    }
}

/// Per-enum sanity checks. v1: variants must have distinct names;
/// payload types are syntactically valid (already enforced by
/// `enum_parse`); a payload that references an unknown user type
/// is a soft warning for now (could be defined cross-batch).
fn validate_enum_decls(
    enum_table: &HashMap<String, &EnumDecl>,
    _type_table: &HashMap<String, &TypeDecl>,
    diags: &mut Vec<Diagnostic>,
) {
    for (qualified, ed) in enum_table {
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for v in &ed.variants {
            if !seen.insert(v.name.as_str()) {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "enum `{qualified}` declares variant `{}` more than \
                         once. Each variant name must be unique within an \
                         enum.",
                        v.name
                    ),
                    span: v.name_span,
                });
            }
        }
    }
}

/// Walk the document and reject [`TypeExpr::Qualified`] anywhere
/// other than as a proc's first-arg type annotation. Qualified
/// types (`E::V`) are only meaningful as overload-dispatch
/// indicators unless they resolve to a declared newtype — those
/// pass through as regular namespaced type references.
///
/// `newtype_names` carries the qualified names of every declared
/// newtype in the document (built via
/// [`build_type_decl_table`]); it's the disambiguator between
/// enum-variant refs and namespaced newtype refs.
fn validate_qualified_positions(
    document: &Document,
    newtype_names: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    fn walk_stmts(
        stmts: &[Stmt],
        newtype_names: &std::collections::HashSet<String>,
        diags: &mut Vec<Diagnostic>,
    ) {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            match &cmd.kind {
                CommandKind::Proc(proc) => {
                    if let Some(sig) = proc.signature.as_ref() {
                        for (i, arg) in sig.args.iter().enumerate() {
                            if let Some(ty) = arg.type_annotation.as_ref() {
                                // The first arg may be Qualified;
                                // tail args may NOT (unless a
                                // known-newtype ref, handled by the
                                // reject fn itself).
                                let allow_qualified = i == 0;
                                reject_nested_qualified(
                                    ty,
                                    allow_qualified,
                                    newtype_names,
                                    diags,
                                );
                            }
                        }
                        if let Some(ret) = sig.return_type.as_ref() {
                            reject_nested_qualified(
                                ret,
                                false,
                                newtype_names,
                                diags,
                            );
                        }
                    }
                    walk_stmts(&proc.body, newtype_names, diags);
                }
                CommandKind::NamespaceEval(ns) => {
                    walk_stmts(&ns.body, newtype_names, diags);
                }
                CommandKind::TypeDecl(td) => {
                    if let Some(ty) = td.underlying.as_ref() {
                        reject_nested_qualified(
                            ty,
                            false,
                            newtype_names,
                            diags,
                        );
                    }
                }
                CommandKind::EnumDecl(ed) => {
                    for v in &ed.variants {
                        if let Some(ty) = v.payload.as_ref() {
                            reject_nested_qualified(
                                ty,
                                false,
                                newtype_names,
                                diags,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }
    walk_stmts(&document.stmts, newtype_names, diags);
}

fn reject_nested_qualified(
    ty: &TypeExpr,
    allow_top_qualified: bool,
    newtype_names: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    match ty {
        TypeExpr::Named { .. } => {}
        TypeExpr::Generic { args, .. } => {
            // Inside a generic, nested Qualified is never allowed —
            // except for known-newtype references, which are just
            // namespaced type names.
            for a in args {
                reject_nested_qualified(a, false, newtype_names, diags);
            }
        }
        TypeExpr::Qualified {
            namespace,
            variant,
            span,
            ..
        } => {
            // A qualified name that resolves to a declared newtype
            // is a regular namespaced type reference — legal
            // wherever a Named type is legal, including return
            // types and generic args.
            let qualified = format!("{namespace}::{variant}");
            if newtype_names.contains(&qualified) {
                return;
            }
            if !allow_top_qualified {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "qualified type `{namespace}::{variant}` is only \
                         legal as the first-argument type annotation on an \
                         overloaded handler proc. It can't appear as a \
                         return type, generic argument, type-decl \
                         underlying, or enum-variant payload."
                    ),
                    span: *span,
                });
            }
        }
    }
}

/// For each newtype declaration `T`, verify the user provided the
/// required `T::repr`, `T::from`, `T::to` procs with the correct
/// shapes:
///
/// - `T::repr` takes one arg named `v` of type `T` (or untyped),
///   returns `string` (or untyped).
/// - `T::from` takes one arg named `v` of type `<underlying>` (or
///   untyped), returns `T` (or untyped).
/// - `T::to` takes one arg named `v` of type `T` (or untyped),
///   returns `<underlying>` (or untyped).
///
/// Type annotations are *optional* on these procs — an untyped
/// arg or return slot is accepted as a "trust the user" form
/// (some procs ship pre-arg-types and were authored before the
/// shape check existed). The arg COUNT and NAME (`v`) are
/// always enforced; the type slots get a stricter check only
/// when the user opted in by annotating them.
fn validate_type_decl_triplets(
    type_table: &HashMap<String, &TypeDecl>,
    sig_table: &HashMap<String, &ProcSignature>,
    diags: &mut Vec<Diagnostic>,
) {
    use crate::ast::TypeExpr;
    for (qualified_name, td) in type_table {
        let underlying = td.underlying.as_ref();
        let slots: &[(&str, Option<&TypeExpr>, Option<&str>)] = &[
            // (slot, arg type expected, return type expected as
            // type-name).  We pass the type-name via Option<&str>
            // and compare with TypeExpr::Named's name; that's
            // sufficient for the v1 set (all involved types are
            // either named primitives or named newtypes; no
            // generics in repr/from/to signatures).
            (
                "repr",
                // arg should be T
                Some(&named_lit(qualified_name)),
                Some("string"),
            ),
            (
                "from",
                // arg should be <underlying>
                underlying,
                // return should be T
                Some(qualified_name.as_str()),
            ),
            (
                "to",
                // arg should be T
                Some(&named_lit(qualified_name)),
                // return should be <underlying>
                underlying.and_then(|u| match u {
                    TypeExpr::Named { name, .. } => Some(name.as_str()),
                    _ => None,
                }),
            ),
        ];
        for (slot, expected_arg, expected_ret) in slots {
            let want = format!("{qualified_name}::{slot}");
            let Some(sig) = sig_table.get(&want) else {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "newtype `{qualified_name}` is missing required \
                         proc `{qualified_name}::{slot}` (see \
                         docs/htcl-return-types.md)."
                    ),
                    span: td.name_span,
                });
                continue;
            };
            // Arg count + name.
            if sig.args.len() != 1 || sig.args[0].name != "v" {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!(
                        "newtype proc `{qualified_name}::{slot}` must \
                         take exactly one argument named `v`"
                    ),
                    span: sig.span,
                });
                continue;
            }
            // Arg type — only checked when the user annotated it.
            if let (Some(actual), Some(expected)) =
                (sig.args[0].type_annotation.as_ref(), expected_arg)
            {
                if !types_match(actual, expected) {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "newtype proc `{qualified_name}::{slot}`: \
                             arg `v` is declared `{}` but should be \
                             `{}`",
                            render_type_inline(actual),
                            render_type_inline(expected)
                        ),
                        span: sig.args[0].name_span,
                    });
                }
            }
            // Return type — only checked when the user annotated it
            // and we know what to compare against.
            if let (Some(actual), Some(want_name)) =
                (sig.return_type.as_ref(), expected_ret)
            {
                // Compare on the identifier's name. For Qualified
                // (namespaced newtype refs like `dcmac::GtChProps`)
                // we join the parts so the compare matches the
                // qualified-name key `want_name` carries.
                let actual_name_owned: String;
                let actual_name = match actual {
                    TypeExpr::Named { name, .. } => name.as_str(),
                    TypeExpr::Generic { name, .. } => name.as_str(),
                    TypeExpr::Qualified {
                        namespace, variant, ..
                    } => {
                        actual_name_owned = format!("{namespace}::{variant}");
                        actual_name_owned.as_str()
                    }
                };
                if actual_name != *want_name {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "newtype proc `{qualified_name}::{slot}` \
                             returns `{}` but should return `{want_name}`",
                            render_type_inline(actual)
                        ),
                        span: sig.span,
                    });
                }
            }
        }
    }
}

/// Build a one-shot `TypeExpr::Named` literal for comparison
/// purposes. The span is meaningless here — we only ever
/// inspect the name.
fn named_lit(name: &str) -> crate::ast::TypeExpr {
    crate::ast::TypeExpr::Named {
        name: name.to_string(),
        span: Span::new(0, 0),
    }
}

/// Structural equality on type expressions, ignoring spans.
///
/// `Qualified { ns, var }` is treated as equivalent to
/// `Named { name: "ns::var" }` — the two forms describe the same
/// identifier and callers that need to compare an ast-parsed
/// annotation against a synthetic Named expected type (e.g.
/// `named_lit(qualified_name)` inside newtype-triplet validation)
/// shouldn't see a spurious mismatch.
fn types_match(a: &crate::ast::TypeExpr, b: &crate::ast::TypeExpr) -> bool {
    use crate::ast::TypeExpr;
    match (a, b) {
        (
            TypeExpr::Named { name: an, .. },
            TypeExpr::Named { name: bn, .. },
        ) => an == bn,
        (
            TypeExpr::Generic {
                name: an, args: aa, ..
            },
            TypeExpr::Generic {
                name: bn, args: ba, ..
            },
        ) => {
            an == bn
                && aa.len() == ba.len()
                && aa.iter().zip(ba.iter()).all(|(x, y)| types_match(x, y))
        }
        // Cross-form equivalence for qualified newtype references
        // (`dcmac::GtChProps` on one side, `Named("dcmac::GtChProps")`
        // on the other). Commutative.
        (
            TypeExpr::Qualified {
                namespace, variant, ..
            },
            TypeExpr::Named { name, .. },
        )
        | (
            TypeExpr::Named { name, .. },
            TypeExpr::Qualified {
                namespace, variant, ..
            },
        ) => *name == format!("{namespace}::{variant}"),
        (
            TypeExpr::Qualified {
                namespace: ans,
                variant: av,
                ..
            },
            TypeExpr::Qualified {
                namespace: bns,
                variant: bv,
                ..
            },
        ) => ans == bns && av == bv,
        _ => false,
    }
}

/// Render a type expression for inclusion in a diagnostic message.
/// Mirrors `vw-analyzer/src/htcl_backend.rs::render_type` — kept
/// in sync by convention since the analyzer can't depend on
/// validate.rs.
fn render_type_inline(ty: &crate::ast::TypeExpr) -> String {
    use crate::ast::TypeExpr;
    match ty {
        TypeExpr::Named { name, .. } => name.clone(),
        TypeExpr::Generic { name, args, .. } => {
            let inner: Vec<String> =
                args.iter().map(render_type_inline).collect();
            format!("{name}<{}>", inner.join(","))
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => {
            format!("{namespace}::{variant}")
        }
    }
}

/// Tcl core builtins that legitimately take either `-flag`
/// arguments natively (`string match -nocase`, `regexp -line`,
/// `lsort -unique`) or take positional list arguments that
/// commonly start with `-` (e.g. `lappend cmd -ruledeck $x` where
/// `-ruledeck` is being appended as a literal token, not parsed
/// by `lappend`). Calls to anything in this list pass the
/// unknown-call check unconditionally.
///
/// Keep this small but pragmatic: a missed builtin produces a
/// pestering error on calls that work fine; an over-included name
/// hides a real "you forgot to src @x" mistake. The set below is
/// the standard Tcl core surface most htcl bodies actually use.
fn is_known_tcl_builtin(name: &str) -> bool {
    matches!(
        name,
        // Container ops whose positional args often look like flags.
        "lappend"
            | "lset"
            | "linsert"
            | "lreplace"
            | "lrange"
            | "lindex"
            | "list"
            | "llength"
            | "dict"
            | "array"
            | "set"
            | "unset"
            | "incr"
            | "append"
            | "concat"
            // String / regex / sort builtins that accept `-flag`s natively.
            | "string"
            | "regexp"
            | "regsub"
            | "lsort"
            | "lsearch"
            | "switch"
            | "format"
            | "scan"
            | "binary"
            // Flow / introspection / interp.
            | "after"
            | "eval"
            | "uplevel"
            | "upvar"
            | "apply"
            | "info"
            | "package"
            | "catch"
            | "try"
            | "throw"
            | "error"
            | "return"
            | "expr"
            // I/O & filesystem.
            | "puts"
            // `putr` is our compile-time repr-dispatching shim:
            // `putr $x` gets rewritten in `crate::putr::rewrite`
            // to `puts [T::repr -v $x]` when the argument's type
            // is statically known, else to plain `puts $x`. The
            // rewrite fires before any code reaches Tcl, so at
            // eval time `putr` isn't a real command — the
            // analyzer needs to recognize it as a builtin so
            // undefined-proc checks don't flag the call sites.
            | "putr"
            | "gets"
            | "read"
            | "close"
            | "open"
            | "file"
            | "exec"
            | "fconfigure"
            | "fileevent"
            | "flush"
            // Channels / Tk-style.
            | "namespace"
            | "variable"
            | "global"
            | "rename"
            | "interp"
    )
}

/// Compiler-emitted primitive-prelude procs: `<T>::<suffix>` for a
/// primitive type `T` and a conversion suffix. `emit_primitive_prelude`
/// (in `crate::repr`) ships a `namespace eval <T> { proc repr … }`
/// block for each primitive at session start — they are NOT `src`d, so
/// a direct call like `list::repr -v {…}` or `dict::repr -v $d` has no
/// entry in the proc table and would otherwise trip the unknown-call
/// check. Same rationale as `putr` in [`is_known_tcl_builtin`]: the
/// analyzer has to know these exist. Keep the `(type, suffix)` sets in
/// lockstep with `emit_primitive_prelude`.
fn is_primitive_prelude_proc(name: &str) -> bool {
    let Some((ns, leaf)) = name.rsplit_once("::") else {
        return false;
    };
    matches!(ns, "string" | "int" | "bool" | "unit" | "list" | "dict")
        && matches!(leaf, "repr" | "from" | "to" | "to_raw" | "from_raw")
}

/// Join a namespace prefix with a member name using Tcl's `::`
/// separator. The empty prefix yields the bare name (used at the
/// document root where there's no enclosing namespace).
fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn validate_command(
    cmd: &Command,
    source: &str,
    table: &HashMap<String, &ProcSignature>,
    var_table: &VarTypeTable,
    diags: &mut Vec<Diagnostic>,
) {
    let call_name = match &cmd.kind {
        CommandKind::Generic => match cmd.words.first() {
            Some(w) => match w.as_text() {
                Some(t) => t,
                None => return,
            },
            None => return,
        },
        // Don't validate inside declarations themselves — those
        // aren't calls. (NamespaceEval is a declaration; its body's
        // statements are validated by the recursion in
        // `validate_stmts`.)
        CommandKind::Proc(_)
        | CommandKind::Set
        | CommandKind::Src(_)
        | CommandKind::NamespaceEval(_)
        | CommandKind::TypeDecl(_)
        | CommandKind::EnumDecl(_) => {
            return;
        }
    };
    // `extern::name` is the user's opt-out: "this call resolves
    // to a runtime Tcl proc, don't analyze its signature." Lowering
    // strips the prefix and aliases the underlying proc into place.
    if crate::lower::is_extern_call(call_name) {
        return;
    }
    let Some(sig) = table.get(call_name) else {
        // Unknown call. Two paths fire an error:
        //
        // 1. The call uses `-flag` keyword arguments. Almost
        //    always the user meant an htcl wrapper that isn't
        //    loaded — shipping it raw to the EDA backend either
        //    errors cryptically or misinterprets the args.
        //
        // 2. The unqualified name has a matching namespaced
        //    proc in scope (e.g., `get_bd_addr_spaces` when
        //    `vivado_cmd::get_bd_addr_spaces` exists). That's a
        //    missed namespace prefix on the same wrapper the
        //    user is calling elsewhere with the qualified name.
        //    Catching this even for positional-only calls is
        //    what makes the analyzer's behavior consistent —
        //    otherwise `assign_bd_address` errors (has `-flag`
        //    args) but `[get_bd_addr_spaces X]` inside its arg
        //    silently passes, which reads as an analyzer gap.
        //
        // A positional-only call to a bare Tcl builtin (`llength`,
        // `dict`, etc.) still passes cleanly: `is_known_tcl_builtin`
        // filters those, and there's no namespaced homonym.
        let uses_keyword = cmd.words.iter().skip(1).any(|w| {
            w.as_text()
                .is_some_and(|t| t.starts_with('-') && t.len() > 1)
        });
        let namespaced_match = if !call_name.contains("::") {
            let suffix = format!("::{call_name}");
            table.keys().find(|k| k.ends_with(&suffix)).cloned()
        } else {
            None
        };
        // Short-circuit BEFORE the expensive fuzzy sweep — Tcl
        // builtins and the primitive prelude are the fast rejection
        // path and account for the vast majority of calls in a
        // healthy source. The old gate stopped here entirely when
        // the call was positional (no -flag) and had no exact
        // namespaced homonym; that let typos like `generate_dcmacc`
        // slip past. The updated gate falls through to a Levenshtein
        // sweep for genuinely-unknown positional calls.
        if is_known_tcl_builtin(call_name)
            || is_primitive_prelude_proc(call_name)
        {
            return;
        }
        // A Levenshtein-close hit against a proc the table already
        // knows about is strong evidence the call is USER code with a
        // typo. Compute lazily — only after we've confirmed the call
        // isn't a builtin or prelude proc, so `bare_names` isn't
        // allocated on every one of the thousands of legitimate
        // builtin calls in a large workspace.
        //
        // Match against BOTH qualified names (`ip::generate_dcmac`)
        // and their bare suffixes (`generate_dcmac`). Bare calls in
        // the same namespace as the target are the common case —
        // e.g. inside `namespace eval ip { ... }`, a call to
        // `generate_dcmacc` fuzzy-matches the bare `generate_dcmac`
        // at distance 1, but the qualified `ip::generate_dcmac` at
        // distance 5 (past the length-scaled threshold). Rebuild
        // the qualified name for the "did you mean" hint by
        // finding the table entry whose suffix matches.
        let need_fuzzy = !uses_keyword && namespaced_match.is_none();
        let (fuzzy_match, fuzzy_hint) = if need_fuzzy {
            let bare_names: Vec<String> = table
                .keys()
                .map(|k| {
                    k.rsplit_once("::")
                        .map(|(_, suffix)| suffix.to_string())
                        .unwrap_or_else(|| k.clone())
                })
                .collect();
            let suggestion = suggest_name(call_name, table.keys())
                .or_else(|| suggest_name(call_name, bare_names.iter()));
            let hint_name = suggestion.as_ref().map(|s| {
                if s.contains("::") {
                    s.clone()
                } else {
                    let suffix = format!("::{s}");
                    table
                        .keys()
                        .filter(|k| k.ends_with(&suffix) || k.as_str() == s)
                        .min_by_key(|k| k.len())
                        .cloned()
                        .unwrap_or_else(|| s.clone())
                }
            });
            (suggestion, hint_name)
        } else {
            // Non-fuzzy path: `-flag` args or an exact namespaced
            // homonym trip the diagnostic on their own; the
            // suggestion (when the -flag path fires) comes from
            // the plain `suggest_name` sweep below.
            (None, None)
        };
        let should_flag =
            uses_keyword || namespaced_match.is_some() || fuzzy_match.is_some();
        if should_flag {
            // Prefer the exact namespaced match as the "did you
            // mean" — it's a stronger signal than the fuzzy
            // Levenshtein suggestion, which for the bare-name
            // case would surface the same or a nearby name
            // anyway.
            let hint = if let Some(qualified) = &namespaced_match {
                format!(" — did you mean `{qualified}`?")
            } else if let Some(s) = fuzzy_hint {
                format!(" — did you mean `{s}`?")
            } else {
                match suggest_name(call_name, table.keys()) {
                    Some(s) => format!(" — did you mean `{s}`?"),
                    None => String::new(),
                }
            };
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "undefined proc `{call_name}`{hint}; either \
                     `src` a module that defines it or use \
                     `extern::{call_name}` to call the underlying \
                     Tcl proc directly"
                ),
                span: cmd.words[0].span,
            });
        }
        return;
    };

    // Parse keyword args from the command's words. The first word is
    // the call name; the remaining words alternate -flag/value.
    let mut idx = 1usize;
    let mut seen: HashMap<String, Span> = HashMap::new();
    while idx < cmd.words.len() {
        let word = &cmd.words[idx];
        let flag_text = match word.as_text() {
            Some(t) if t.starts_with('-') => &t[1..],
            Some(t) => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!("expected keyword argument, found {t}"),
                    span: word.span,
                });
                idx += 1;
                continue;
            }
            None => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: "expected keyword argument".into(),
                    span: word.span,
                });
                idx += 1;
                continue;
            }
        };
        let flag_name = flag_text.to_string();
        let value_word = cmd.words.get(idx + 1);

        match sig.find(&flag_name) {
            None => {
                let known: Vec<&str> =
                    sig.args.iter().map(|a| a.name.as_str()).collect();
                let hint = if known.is_empty() {
                    String::new()
                } else {
                    format!(". Possible values are {}", known.join(", "))
                };
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!("undefined argument -{flag_name}{hint}"),
                    span: word.span,
                });
            }
            Some(arg) => {
                if let Some(prev) = seen.insert(flag_name.clone(), word.span) {
                    let _ = prev;
                    diags.push(Diagnostic {
                        severity: Severity::Warning,
                        message: format!("duplicate argument -{flag_name}"),
                        span: word.span,
                    });
                }
                if let Some(value) = value_word {
                    validate_value(call_name, arg, value, source, diags);
                    // Nominal type check for the value expression.
                    // Only fires when BOTH sides have known types —
                    // the caller wrote a `: TYPE` annotation on the
                    // arg (populated by proc_args parsing into
                    // `ProcArg.type_annotation`) AND the value word
                    // is one whose type `value_type` can infer
                    // (`[proc-call]` return type or `$var` binding).
                    // Literals and mixed compounds silently skip
                    // (gradual typing).
                    //
                    // Identity via `types_match` — no alias walking
                    // (see `VarTypeTable` docs). `Quad0Ch1Props ≠
                    // Quad1Ch0Props ≠ Properties` even when both
                    // alias the same underlying; that's what
                    // catches the copy-paste-wrong-constructor bug
                    // this check exists for.
                    if let Some(declared) = &arg.type_annotation {
                        if let Some(actual) =
                            value_type(value, table, var_table)
                        {
                            if !types_match(declared, &actual) {
                                diags.push(Diagnostic {
                                    severity: Severity::Error,
                                    message: format!(
                                        "type mismatch for -{}: expected \
                                         `{}`, found `{}`",
                                        flag_name,
                                        render_type_inline(declared),
                                        render_type_inline(&actual),
                                    ),
                                    span: value.span,
                                });
                            }
                        } else if is_bool_type(declared) {
                            // `bool`-typed slot with a value the
                            // `value_type` pass can't infer. The
                            // ONLY textual values that produce a
                            // known `bool` are the bare `true` /
                            // `false` literals (see `value_type`);
                            // any other whole-word text literal at
                            // this slot is a mistyped bool
                            // (`-flag 1`, `-flag yes`, `-flag
                            // potato`). Reject with a message
                            // naming the offending literal so the
                            // caller can rewrite it.
                            //
                            // Skips vars/cmdsubst and compound
                            // words: those return None from
                            // value_type because we couldn't
                            // deduce the type, not because they're
                            // definitively wrong.
                            if let Some(lit) = value.as_text() {
                                diags.push(Diagnostic {
                                    severity: Severity::Error,
                                    message: format!(
                                        "type mismatch for -{}: expected \
                                         `bool`, found literal `{}` (use \
                                         `true` or `false`)",
                                        flag_name, lit,
                                    ),
                                    span: value.span,
                                });
                            }
                        }
                    }
                } else {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} is missing a value"
                        ),
                        span: word.span,
                    });
                }
            }
        }
        // Step past the flag and its value.
        idx += if value_word.is_some() { 2 } else { 1 };
    }

    // Build canonical `@one_of` groups. Each arg's `@one_of(...)`
    // declares an alternatives set: the arg itself plus the named
    // siblings. We collapse declarations from each direction (sib A
    // says `@one_of(B)` and sib B says `@one_of(A)`) into one
    // canonical group, then check that **exactly one** arg from each
    // group is supplied at the call site.
    //
    // Args participating in a group are treated as optional for the
    // missing-required check below — the group rule is the source of
    // truth for "must supply something."
    let one_of_groups = collect_one_of_groups(sig);
    let in_one_of: std::collections::HashSet<&str> = one_of_groups
        .iter()
        .flat_map(|g| g.iter().map(String::as_str))
        .collect();

    // Required-args check. An arg is required when it has no
    // `@default` to fall back to — the user must supply a value.
    // Args in an `@one_of` group are governed by the group rule
    // instead, so skip them here.
    for arg in &sig.args {
        if seen.contains_key(&arg.name) {
            continue;
        }
        if in_one_of.contains(arg.name.as_str()) {
            continue;
        }
        let is_required = arg.attribute("default").is_none();
        if is_required {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "missing required argument -{name}",
                    name = arg.name,
                ),
                span: cmd.span,
            });
        }
    }

    // `@one_of` groups: exactly one alternative must be present.
    for group in &one_of_groups {
        let present: Vec<&str> = group
            .iter()
            .filter(|n| seen.contains_key(n.as_str()))
            .map(String::as_str)
            .collect();
        if present.len() == 1 {
            continue;
        }
        let opts: Vec<String> = group.iter().map(|n| format!("-{n}")).collect();
        let message = if present.is_empty() {
            format!(
                "missing required argument — exactly one of {} must be \
                 supplied",
                opts.join(", ")
            )
        } else {
            let got: Vec<String> =
                present.iter().map(|n| format!("-{n}")).collect();
            format!(
                "exactly one of {} may be supplied, got {}",
                opts.join(", "),
                got.join(", ")
            )
        };
        diags.push(Diagnostic {
            severity: Severity::Error,
            message,
            span: cmd.span,
        });
    }

    // Inter-arg deps for present args.
    for (flag_name, flag_span) in &seen {
        let Some(arg) = sig.find(flag_name) else {
            continue;
        };
        if let Some(req) = arg.attribute("requires") {
            for value in &req.values {
                let referenced = match value {
                    AttributeValue::Ident { value, .. }
                    | AttributeValue::String { value, .. } => value.as_str(),
                    AttributeValue::Integer { .. }
                    | AttributeValue::Keyed { .. } => continue,
                };
                if !seen.contains_key(referenced) {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} requires -{referenced} \
                             to also be set"
                        ),
                        span: *flag_span,
                    });
                }
            }
        }
        if let Some(conflicts) = arg.attribute("conflicts") {
            for value in &conflicts.values {
                let referenced = match value {
                    AttributeValue::Ident { value, .. }
                    | AttributeValue::String { value, .. } => value.as_str(),
                    AttributeValue::Integer { .. }
                    | AttributeValue::Keyed { .. } => continue,
                };
                if seen.contains_key(referenced) {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} conflicts with \
                             -{referenced}"
                        ),
                        span: *flag_span,
                    });
                }
            }
        }
        if arg.attribute("deprecated").is_some() {
            let msg = arg
                .attribute("deprecated")
                .and_then(|a| a.values.first())
                .map(|v| v.as_str().to_string())
                .unwrap_or_default();
            let m = if msg.is_empty() {
                format!("argument -{flag_name} is deprecated")
            } else {
                format!("argument -{flag_name} is deprecated: {msg}")
            };
            diags.push(Diagnostic {
                severity: Severity::Warning,
                message: m,
                span: *flag_span,
            });
        }
    }
}

/// Collect canonical `@one_of` alternatives groups for a signature.
///
/// Each arg's `@one_of(sib1, sib2, ...)` declares that exactly one
/// of `{arg, sib1, sib2, ...}` must be supplied at the call site.
/// We treat the declaration as symmetric (both `dict @one_of(name)`
/// and `name @one_of(dict)` describe the same group), so a `BTreeSet`
/// of the participating names canonicalizes each group regardless of
/// which direction (or which redundant copies) the author wrote.
fn collect_one_of_groups(
    sig: &ProcSignature,
) -> Vec<std::collections::BTreeSet<String>> {
    use std::collections::BTreeSet;
    let mut seen: std::collections::HashSet<BTreeSet<String>> =
        std::collections::HashSet::new();
    let mut out: Vec<BTreeSet<String>> = Vec::new();
    for arg in &sig.args {
        let Some(attr) = arg.attribute("one_of") else {
            continue;
        };
        let mut group: BTreeSet<String> = BTreeSet::new();
        group.insert(arg.name.clone());
        for value in &attr.values {
            match value {
                AttributeValue::Ident { value, .. }
                | AttributeValue::String { value, .. } => {
                    group.insert(value.clone());
                }
                AttributeValue::Integer { .. }
                | AttributeValue::Keyed { .. } => continue,
            }
        }
        if group.len() >= 2 && seen.insert(group.clone()) {
            out.push(group);
        }
    }
    out
}

fn validate_value(
    call_name: &str,
    arg: &ProcArg,
    value_word: &Word,
    _source: &str,
    diags: &mut Vec<Diagnostic>,
) {
    // For Phase 2 we only validate literal-text values. Word forms
    // that include `$var` or `[cmd]` are runtime-dynamic; we let them
    // through silently. Future work can teach the validator about
    // values produced by known builtins.
    let Some(literal) = literal_value(value_word) else {
        return;
    };

    if let Some(enum_attr) = arg.attribute("enum") {
        check_enum(
            call_name, &arg.name, enum_attr, &literal, value_word, diags,
        );
    }
    if let Some(range_attr) = arg.attribute("range") {
        check_range(
            call_name, &arg.name, range_attr, &literal, value_word, diags,
        );
    }
}

fn literal_value(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::Escape { value, .. } => out.push(*value),
            WordPart::VarRef { .. } | WordPart::CmdSubst { .. } => {
                // Dynamic content — not a literal.
                return None;
            }
        }
    }
    Some(out)
}

fn check_enum(
    _call_name: &str,
    arg_name: &str,
    enum_attr: &Attribute,
    literal: &str,
    value_word: &Word,
    diags: &mut Vec<Diagnostic>,
) {
    let allowed: Vec<String> = enum_attr
        .values
        .iter()
        .map(|v| match v {
            AttributeValue::Integer { value, .. } => value.to_string(),
            AttributeValue::Ident { value, .. }
            | AttributeValue::String { value, .. } => value.clone(),
            // `key=value` in @enum(…) doesn't make semantic sense
            // (enum values are positional). Include the literal
            // `key=value` string so a runtime match against the
            // raw arg still works if someone actually did that.
            AttributeValue::Keyed { .. } => v.to_tcl_literal(),
        })
        .collect();
    if !allowed.iter().any(|a| a == literal) {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "value {literal} for -{arg_name} is not in @enum. Possible \
                 values are {}",
                allowed.join(", ")
            ),
            span: value_word.span,
        });
    }
}

fn check_range(
    _call_name: &str,
    arg_name: &str,
    range_attr: &Attribute,
    literal: &str,
    value_word: &Word,
    diags: &mut Vec<Diagnostic>,
) {
    let (Some(min), Some(max)) =
        (range_attr.values.first(), range_attr.values.get(1))
    else {
        diags.push(Diagnostic {
            severity: Severity::Warning,
            message: format!(
                "@range on -{arg_name} should have two numeric bounds"
            ),
            span: range_attr.span,
        });
        return;
    };
    let (
        AttributeValue::Integer { value: min, .. },
        AttributeValue::Integer { value: max, .. },
    ) = (min, max)
    else {
        diags.push(Diagnostic {
            severity: Severity::Warning,
            message: format!("@range on -{arg_name} has non-integer bounds"),
            span: range_attr.span,
        });
        return;
    };
    let Ok(n) = literal.parse::<i64>() else {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "argument -{arg_name} expects an integer, found {literal}"
            ),
            span: value_word.span,
        });
        return;
    };
    if n < *min || n > *max {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "value {n} for -{arg_name} is out of @range({min}, {max})"
            ),
            span: value_word.span,
        });
    }
}

/// Standard compiler-style "did you mean X?" suggestion: pick the
/// in-scope name with the smallest edit distance from `target`,
/// within a length-scaled threshold. Returns `None` when no
/// candidate is close enough (so unknown calls that aren't
/// near-misses don't get nonsense suggestions tacked on).
fn suggest_name<'a, I>(target: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a String>,
{
    // rustc-style threshold: scales with name length so single-char
    // typos count for short names, but a 12-char identifier
    // tolerates a few keystroke errors. Floor at 1, ceiling at 3 —
    // anything past 3 starts producing surprising suggestions.
    let threshold = (target.chars().count() / 3).clamp(1, 3);
    let target_len = target.chars().count();
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        // Length-difference lower-bound: levenshtein(a, b) ≥ ||a| - |b||.
        // Skip candidates whose length already exceeds the threshold —
        // no amount of substitution/insertion can bring the distance
        // in range. This alone cuts the ~5000-candidate scan on
        // gtwiz-versal (proc names 20-80 chars, target ~15 chars)
        // down to a small handful of viable comparisons.
        let cand_len = cand.chars().count();
        let len_diff = target_len.abs_diff(cand_len);
        if len_diff > threshold {
            continue;
        }
        let d = levenshtein_capped(target, cand, threshold);
        if d == 0 || d > threshold {
            continue;
        }
        if best.map(|(b, _)| d < b).unwrap_or(true) {
            best = Some((d, cand.as_str()));
        }
    }
    best.map(|(_, s)| s.to_string())
}

/// Length-capped Levenshtein — returns `max+1` (or larger) as soon
/// as the running row minimum exceeds `max`, avoiding the full
/// O(m*n) sweep when the caller only cares whether the distance
/// is ≤ `max`. Every call from `suggest_name` cares about a
/// threshold of at most 3, so this ends most comparisons after a
/// handful of cells — critical when the candidate table has
/// thousands of entries.
fn levenshtein_capped(a: &str, b: &str, max: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![0usize; n + 1];
    let sentinel = max + 1;
    for i in 1..=m {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=n {
            let sub = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + sub);
            if cur[j] < row_min {
                row_min = cur[j];
            }
        }
        if row_min > max {
            return sentinel;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

/// Standard Levenshtein edit distance — number of single-character
/// insertions, deletions, or substitutions to turn `a` into `b`.
/// Two-row rolling table; O(n*m) time, O(n) space.
#[allow(dead_code)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![0usize; n + 1];
    for i in 1..=m {
        cur[0] = i;
        for j in 1..=n {
            let sub = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let parsed = parse(src);
        // Parse errors shouldn't be present in these tests; assert
        // so that test failures point at the right layer.
        assert!(
            parsed.errors.is_empty(),
            "unexpected parse errors: {:?}",
            parsed.errors
        );
        // Filter out unused-variable warnings AND undefined-variable
        // errors. This module tests the arg / type / enum /
        // qualified-position validators; fixtures typically declare
        // test-only procs with unused args and reference free vars
        // to keep the snippets short. The unused / undefined passes
        // have their own tests in `unused::tests` / `undefined::tests`.
        validate(&parsed.document, src)
            .into_iter()
            .filter(|d| {
                let unused = d.severity == Severity::Warning
                    && (d.message.starts_with("unused proc arg ")
                        || d.message.starts_with("unused local "));
                let undefined = d.severity == Severity::Error
                    && d.message.starts_with("undefined variable ");
                !(unused || undefined)
            })
            .collect()
    }

    fn proc_decl(body: &str, call: &str) -> String {
        format!("proc axis_interface {{\n{body}\n}} {{ # body\n}}\n{call}\n")
    }

    #[test]
    fn happy_path_no_diagnostics() {
        let src = proc_decl(
            "  @default(0) has_tkeep\n  @default(8) tdata_num_bytes",
            "axis_interface -has_tkeep 1 -tdata_num_bytes 16",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn unknown_arg() {
        let src =
            proc_decl("  @default(0) has_tkeep", "axis_interface -has_typo 1");
        let d = diags(&src);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.contains("undefined argument -has_typo"),
            "{:?}",
            d
        );
        assert!(d[0].message.contains("Possible values are has_tkeep"));
    }

    #[test]
    fn missing_required() {
        let src = proc_decl("  @required width", "axis_interface");
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("missing required")));
    }

    #[test]
    fn enum_rejects_unlisted_value() {
        let src = proc_decl(
            "  @enum(1, 2, 4, 8) tdata_num_bytes",
            "axis_interface -tdata_num_bytes 3",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("@enum")));
    }

    #[test]
    fn enum_accepts_listed_value() {
        let src = proc_decl(
            "  @enum(1, 2, 4, 8) tdata_num_bytes",
            "axis_interface -tdata_num_bytes 4",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn range_check() {
        let src =
            proc_decl("  @range(1, 16) width", "axis_interface -width 32");
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("out of @range")));
    }

    #[test]
    fn requires_dependency() {
        let src = proc_decl(
            "  @default(0) has_tuser\n  @requires(has_tuser) tuser_width",
            "axis_interface -tuser_width 8",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("requires")), "{:?}", d);
    }

    #[test]
    fn conflicts_dependency() {
        let src = proc_decl(
            "  has_a\n  @conflicts(has_a) has_b",
            "axis_interface -has_a 1 -has_b 1",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("conflicts")));
    }

    #[test]
    fn one_of_requires_exactly_one_alternative() {
        // Two args in an @one_of group — neither supplied → error.
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface",
        );
        let d = diags(&src);
        assert!(
            d.iter().any(|m| m.message.contains("exactly one of -a, -b")
                && m.message.contains("must be supplied")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_satisfied_by_either_alternative() {
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface -a 1",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn one_of_rejects_both_alternatives() {
        // Both supplied — should be reported once (group rule).
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface -a 1 -b 2",
        );
        let d = diags(&src);
        assert!(
            d.iter().any(|m| m.message.contains("got -a, -b")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_arg_is_not_treated_as_required() {
        // An @one_of arg without @default should NOT trigger the
        // separate "missing required" error — the group rule
        // supersedes individual required-ness.
        let src =
            proc_decl("  @one_of(b) a\n  @one_of(a) b", "axis_interface -a 1");
        let d = diags(&src);
        assert!(
            d.iter()
                .all(|m| !m.message.contains("missing required argument -a")
                    && !m.message.contains("missing required argument -b")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_declarations_are_symmetric() {
        // Declaring `@one_of(b)` on `a` alone is enough; we don't need
        // the reverse on `b`.
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") b",
            "axis_interface",
        );
        let d = diags(&src);
        let group_errors: Vec<_> = d
            .iter()
            .filter(|m| {
                m.message.contains("exactly one of")
                    && m.message.contains("must be supplied")
            })
            .collect();
        assert_eq!(group_errors.len(), 1, "{:?}", d);
    }

    #[test]
    fn namespace_eval_proc_validates_at_qualified_name() {
        // A proc declared inside `namespace eval project { ... }`
        // should be reachable from the validator at its qualified
        // name (`project::set_target_language`), so `@enum`
        // constraints on its args still catch bad values at call
        // sites — exactly like a top-level proc declaration would.
        let src = "\
namespace eval project {
  proc set_target_language {
    proj
    @enum(VHDL, Verilog) language
  } { }
}
project::set_target_language -proj p -language Klingon
";
        let d = diags(src);
        assert!(
            d.iter().any(|m| m.message.contains("Klingon")
                && m.message.contains("@enum")),
            "{:?}",
            d
        );
    }

    #[test]
    fn namespaced_proc_satisfied_by_valid_args() {
        let src = "\
namespace eval project {
  proc set_target_language {
    proj
    @enum(VHDL, Verilog) language
  } { }
}
project::set_target_language -proj p -language VHDL
";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn nested_namespace_eval_qualifies_recursively() {
        let src = "\
namespace eval outer {
  namespace eval inner {
    proc foo { @enum(a, b) x } { }
  }
}
outer::inner::foo -x bogus
";
        let d = diags(src);
        assert!(d.iter().any(|m| m.message.contains("bogus")), "{:?}", d);
    }

    #[test]
    fn unknown_call_gets_did_you_mean_suggestion() {
        // The exact shape that caught the user's typo in metroid:
        // a single-char edit-distance miss against a known proc
        // should produce a `did you mean ...` suggestion.
        let src = "\
namespace eval port {
  proc plumb_if_pin {
    name
    pin
  } { }
}
port::plum_if_pin -name p -pin q
";
        let d = diags(src);
        let err = d.iter().find(|m| m.severity == Severity::Error).unwrap();
        assert!(
            err.message.contains("did you mean `port::plumb_if_pin`"),
            "{}",
            err.message
        );
    }

    /// Bare positional call to a name that's one character off
    /// from a defined proc — same shape as the metroid typo where
    /// `generate_dcmacc` silently escaped the validator. The old
    /// `uses_keyword || namespaced_match` gate short-circuited to
    /// false for zero-arg positional calls; the fuzzy-match gate
    /// now catches it.
    #[test]
    fn bare_positional_typo_of_known_proc_is_flagged() {
        let src = "\
namespace eval ip {
  proc generate_dcmac {} unit { }
  proc go {} unit {
    generate_dcmacc
  }
}
";
        let d = diags(src);
        let err = d
            .iter()
            .find(|m| {
                m.severity == Severity::Error
                    && m.message.contains("generate_dcmacc")
            })
            .unwrap_or_else(|| panic!("no diagnostic on typo: {d:?}"));
        assert!(
            err.message.contains("did you mean `ip::generate_dcmac`"),
            "{}",
            err.message
        );
    }

    #[test]
    fn unrelated_unknown_call_has_no_suggestion() {
        // A name with no near-miss should NOT get a fake suggestion
        // tacked on — that's just misleading noise.
        let src = "totally_made_up_thing -arg 1\n";
        let d = diags(src);
        let err = d.iter().find(|m| m.severity == Severity::Error).unwrap();
        assert!(!err.message.contains("did you mean"), "{}", err.message);
    }

    #[test]
    fn unknown_keyword_call_is_an_error() {
        // No proc declaration in scope, no `extern::` prefix — the
        // call uses `-flag` shape so the validator demands the user
        // be explicit about the dependency.
        let src = "create_project -in_memory 1 -name foo\n";
        let d = diags(src);
        assert!(
            d.iter().any(|m| m.severity == Severity::Error
                && m.message.contains("create_project")
                && m.message.contains("extern::")),
            "{:?}",
            d
        );
    }

    #[test]
    fn extern_prefixed_call_skips_unknown_check() {
        // `extern::` is the user's opt-out: they're calling a raw
        // Tcl proc deliberately. No diagnostic even though the
        // name isn't in the signature table.
        let src = "extern::create_project -name foo\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn positional_unknown_call_is_allowed() {
        // No `-flag` args → looks like a positional Tcl builtin
        // call (puts, set, etc.). Pass through silently.
        let src = "puts hello\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn positional_call_with_namespaced_match_is_flagged() {
        // `get_thing X` positional-only would normally pass (raw
        // Tcl assumption), but here a `foo::get_thing` is in
        // scope — the unqualified form is almost certainly a
        // missed namespace prefix, worth flagging with a
        // "did you mean" that names the exact match.
        let src = "\
namespace eval foo {
  proc get_thing {name} { return $name }
}
puts [get_thing X]
";
        let d = diags(src);
        let errs: Vec<_> = d
            .iter()
            .filter(|e| e.message.contains("undefined proc `get_thing`"))
            .collect();
        assert_eq!(
            errs.len(),
            1,
            "expected one undefined-proc diag, got {d:?}"
        );
        assert!(
            errs[0].message.contains("foo::get_thing"),
            "expected `foo::get_thing` suggestion, got: {}",
            errs[0].message,
        );
    }

    #[test]
    fn positional_call_with_no_namespaced_match_still_passes() {
        // No matching namespaced proc → keep the "raw Tcl
        // builtin assumption" semantics for positional calls.
        // A bare `some_native X` with nothing named `*::some_native`
        // in scope stays silent.
        let src = "puts [some_native X]\n";
        let d = diags(src);
        assert!(
            d.iter().all(|e| !e.message.contains("undefined proc")),
            "unexpected diag: {d:?}",
        );
    }

    #[test]
    fn known_tcl_builtin_with_keyword_args_is_allowed() {
        // `string match -nocase ...` is a legitimate Tcl-core
        // pattern; the allowlist keeps it from triggering the
        // unknown-call error.
        let src = "string match -nocase pat str\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn primitive_prelude_reprs_are_not_undefined() {
        // `list::repr` / `dict::repr` (and the other primitive
        // conversion procs) ship via `emit_primitive_prelude`, not
        // `src`, so their `-v` calls must NOT trip the unknown-call
        // check. Regression guard for the `list::`/`dict::` errors.
        let src = "\
puts [list::repr -v {a b c}]
puts [dict::repr -v {foo 1 bar 2}]
puts [string::repr -v hi]
puts [int::from -v 3]
";
        let d = diags(src);
        assert!(
            d.iter().all(|e| !e.message.contains("undefined proc")),
            "primitive prelude reprs should resolve, got: {d:?}",
        );
    }

    #[test]
    fn namespace_eval_extern_is_rejected() {
        let src = "namespace eval extern { proc foo {} { } }\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|m| m.message.contains("reserved namespace name")),
            "{:?}",
            d
        );
    }

    #[test]
    fn duplicate_arg_warns() {
        let src = proc_decl("  has_a", "axis_interface -has_a 1 -has_a 2");
        let d = diags(&src);
        assert!(
            d.iter().any(|d| d.message.contains("duplicate argument")),
            "{:?}",
            d
        );
    }

    #[test]
    fn dynamic_value_skips_enum_check() {
        let src =
            proc_decl("  @enum(1, 2, 4) width", "axis_interface -width $x");
        // $x is runtime; we don't statically know it's outside the
        // enum, so no enum diagnostic.
        let d = diags(&src);
        assert!(d.iter().all(|d| !d.message.contains("@enum")));
    }

    #[test]
    fn validates_call_inside_proc_body() {
        // A bad flag on a call nested in another proc's body should be
        // diagnosed, same as at the top level.
        let src = "\
proc if_tport {\n  type\n  name\n} { }\n\
proc axis_if {\n  kind\n} {\n  if_tport -type t -namze m\n}\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("undefined argument -namze")),
            "{:?}",
            d
        );
    }

    #[test]
    fn validates_call_inside_command_substitution() {
        // The user case: `set cell [create_cpm5 -foo bar]`. The
        // validator must descend into `[…]` so the bad flag is caught
        // the same way it is at the top level.
        let src = "\
proc create_cpm5 {\n  @default(0) name\n} { }\n\
set cell [create_cpm5 -foo bar]\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("undefined argument -foo")),
            "{:?}",
            d
        );
    }

    #[test]
    fn arg_with_no_default_is_implicitly_required() {
        // `name` has neither `@default` nor `@required` — calling the
        // proc without a value for it should still error.
        let src = "\
proc create_cpm5 {\n  name\n} { }\n\
create_cpm5\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("missing required argument -name")),
            "{:?}",
            d
        );
    }

    #[test]
    fn implicit_required_satisfied_when_supplied() {
        let src = "\
proc create_cpm5 {\n  name\n} { }\n\
create_cpm5 -name x\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn extra_signatures_resolve_unknown_calls_from_prior_batches() {
        // The REPL session case: a wrapper declared in a prior
        // batch is in `extra`; the new batch's bare call to it must
        // resolve (no `extern::` error) and its keyword args must
        // validate against the prior signature.
        let prior_src = "\
namespace eval vivado {
  proc create_project {
    @default(\"\") name
    @enum(0, 1) @default(0) in_memory
  } { }
}
";
        let prior_parsed = parse(prior_src);
        assert!(prior_parsed.errors.is_empty());
        let mut sink = Vec::new();
        let prior_table =
            build_signature_table(&prior_parsed.document, &mut sink);

        // New batch: bare `vivado::create_project -name foo`. No
        // declaration in scope here — only the prior batch's table
        // saves it from the unknown-keyword-call error.
        let new_src = "vivado::create_project -name foo\n";
        let new_parsed = parse(new_src);
        let diags = validate_with_signatures(
            &new_parsed.document,
            new_src,
            &prior_table,
        );
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "{:?}",
            diags
        );

        // And the keyword-args still get validated — a bad enum
        // value should still error even though the sig came in
        // through `extra`.
        let bad_src = "vivado::create_project -in_memory bogus\n";
        let bad_parsed = parse(bad_src);
        let bad_diags = validate_with_signatures(
            &bad_parsed.document,
            bad_src,
            &prior_table,
        );
        assert!(
            bad_diags
                .iter()
                .any(|d| d.message.contains("bogus")
                    && d.message.contains("@enum")),
            "{:?}",
            bad_diags
        );
    }

    #[test]
    fn doc_signatures_shadow_extra_without_warning() {
        // Re-declaring a proc in the new batch should NOT raise the
        // "duplicate definition" warning against the prior-batch
        // signature — that's a normal `src @lib` reload case in the
        // REPL and would be noisy. The new declaration takes
        // precedence.
        let prior_src = "proc foo { @default(0) x } { }\n";
        let prior_parsed = parse(prior_src);
        let mut sink = Vec::new();
        let prior_table =
            build_signature_table(&prior_parsed.document, &mut sink);

        let new_src = "proc foo { @default(1) y } { }\nfoo -y 2\n";
        let new_parsed = parse(new_src);
        let diags = validate_with_signatures(
            &new_parsed.document,
            new_src,
            &prior_table,
        );
        assert!(
            diags.iter().all(|d| !d.message.contains("duplicate")),
            "{:?}",
            diags
        );
        // And the new sig is the one that resolved: `-y` is
        // accepted, `-x` would have been the prior sig's arg.
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "{:?}",
            diags
        );
    }

    #[test]
    fn unknown_positional_call_is_not_validated() {
        // Bare positional call to an unknown name (could be a Tcl
        // builtin) is silently accepted. Unknown calls with
        // `-flag` args are the *only* unknown-call case that
        // errors — see `unknown_keyword_call_is_an_error`.
        let src = "axis_interface tkeep_yes 1\n";
        assert!(diags(src).is_empty());
    }

    // --- type-decl triplet enforcement (step 1b) ----------------

    /// Build a valid type+triplet block — bd_cell with all three
    /// procs present so the validator should accept it.
    fn full_triplet_src() -> &'static str {
        "type bd_cell = string\n\
         proc bd_cell::repr {v} { return $v }\n\
         proc bd_cell::from {v} { return $v }\n\
         proc bd_cell::to {v} { return $v }\n"
    }

    #[test]
    fn type_decl_with_full_triplet_passes() {
        let src = full_triplet_src();
        let d = diags(src);
        assert!(
            d.iter().all(|d| !d.message.contains("missing required")),
            "unexpected diagnostics: {:?}",
            d
        );
    }

    #[test]
    fn type_decl_missing_repr_emits_diagnostic() {
        let src = "type bd_cell = string\n\
                   proc bd_cell::from {v} { return $v }\n\
                   proc bd_cell::to {v} { return $v }\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| d.message.contains("missing required"))
            .expect("expected diagnostic");
        assert!(hit.message.contains("bd_cell::repr"), "{:?}", hit);
        assert_eq!(hit.severity, Severity::Error);
    }

    #[test]
    fn type_decl_missing_all_three_lists_each() {
        let src = "type widget = string\n";
        let d = diags(src);
        // Now each missing slot emits its own diagnostic, so we
        // assert each one shows up separately.
        let missing: Vec<&str> = d
            .iter()
            .filter(|d| d.message.contains("missing required proc"))
            .map(|d| d.message.as_str())
            .collect();
        assert!(missing.iter().any(|m| m.contains("widget::repr")));
        assert!(missing.iter().any(|m| m.contains("widget::from")));
        assert!(missing.iter().any(|m| m.contains("widget::to")));
    }

    #[test]
    fn type_decl_wrong_arg_type_emits_diagnostic() {
        // Annotate the v arg with the wrong type and expect a
        // shape-mismatch diagnostic.
        let src = "type widget = string\n\
                   proc widget::repr {v: int} string { return $v }\n\
                   proc widget::from {v: string} widget { return $v }\n\
                   proc widget::to {v: widget} string { return $v }\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| d.message.contains("widget::repr"))
            .expect("expected mismatch diagnostic");
        assert!(
            hit.message.contains("`int`") || hit.message.contains("int"),
            "{:?}",
            hit
        );
        assert!(hit.message.contains("widget"), "{:?}", hit);
    }

    #[test]
    fn type_decl_wrong_return_type_emits_diagnostic() {
        let src = "type widget = string\n\
                   proc widget::repr {v: widget} int { return 0 }\n\
                   proc widget::from {v: string} widget { return $v }\n\
                   proc widget::to {v: widget} string { return $v }\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| d.message.contains("returns"))
            .expect("expected return-type mismatch diagnostic");
        assert!(hit.message.contains("widget::repr"), "{:?}", hit);
        assert!(hit.message.contains("string"), "{:?}", hit);
    }

    #[test]
    fn type_decl_unannotated_triplet_still_passes() {
        // Existence-only check stays a fallback when the user
        // hasn't annotated the procs yet.
        let src = "type widget = string\n\
                   proc widget::repr {v} { return $v }\n\
                   proc widget::from {v} { return $v }\n\
                   proc widget::to {v} { return $v }\n";
        let d = diags(src);
        assert!(
            d.iter().all(|d| d.severity != Severity::Error),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn type_decl_in_namespace_qualifies() {
        let src = "namespace eval x {\n\
                     type widget = string\n\
                   }\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| d.message.contains("missing required"))
            .expect("expected diagnostic");
        // The namespace-qualified name should appear in the message.
        assert!(hit.message.contains("x::widget"), "{:?}", hit);
        assert!(hit.message.contains("x::widget::repr"));
    }

    #[test]
    fn prior_batch_procs_satisfy_current_batch_type_decl() {
        // Batch 1: just declares the procs.
        let prior_src = "proc bd_cell::repr {v} { return $v }\n\
                         proc bd_cell::from {v} { return $v }\n\
                         proc bd_cell::to {v} { return $v }\n";
        let prior = parse(prior_src);
        let mut prior_diags = Vec::new();
        let prior_sigs =
            build_signature_table(&prior.document, &mut prior_diags);
        // Batch 2: declares the type — should NOT complain because
        // the procs live in the prior batch's signature table.
        let new_src = "type bd_cell = string\n";
        let new_parsed = parse(new_src);
        let diags = validate_with_signatures(
            &new_parsed.document,
            new_src,
            &prior_sigs,
        );
        assert!(
            diags
                .iter()
                .all(|d| !d.message.contains("missing required")),
            "got: {:?}",
            diags
        );
    }

    #[test]
    fn prior_batch_type_decl_does_not_re_trigger_in_current_batch() {
        // Batch 1: declares the type, no procs yet (would error
        // in isolation).
        let prior_src = "type bd_cell = string\n";
        let prior = parse(prior_src);
        let mut prior_diags = Vec::new();
        let prior_types =
            build_type_decl_table(&prior.document, &mut prior_diags);
        // Batch 2: adds the procs. The type is in `extra_types`,
        // and the procs are in batch 2's signature table. Putting
        // them together via validate_with_extras should pass.
        let new_src = "proc bd_cell::repr {v} { return $v }\n\
                       proc bd_cell::from {v} { return $v }\n\
                       proc bd_cell::to {v} { return $v }\n";
        let new_parsed = parse(new_src);
        let empty_sigs: HashMap<String, &ProcSignature> = HashMap::new();
        let d = validate_with_extras(
            &new_parsed.document,
            new_src,
            &empty_sigs,
            &prior_types,
        );
        assert!(
            d.iter().all(|d| !d.message.contains("missing required")),
            "got: {:?}",
            d
        );
    }

    // --- enum + overload classifier (step 3) ----------------------

    #[test]
    fn enum_decl_with_unique_variants_passes() {
        let src = "enum Direction = {\n  North\n  South\n  East\n  West\n}\n";
        let d = diags(src);
        assert!(d.is_empty(), "got: {:?}", d);
    }

    #[test]
    fn enum_decl_with_duplicate_variants_errors() {
        let src = "enum Bad = {\n  A: int\n  B: string\n  A: bool\n}\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| {
                d.severity == Severity::Error
                    && d.message.contains("variant `A`")
            })
            .expect("expected duplicate-variant diagnostic");
        assert!(hit.message.contains("more than once"), "{:?}", hit);
    }

    #[test]
    fn overload_set_with_exhaustive_arms_classifies() {
        let src = "\
enum Property = {\n  Scalar: string\n  Nested: int\n}\n\
proc handle {v: Property::Scalar} { return $v }\n\
proc handle {v: Property::Nested} { return $v }\n";
        let parsed = parse(src);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let mut diags = Vec::new();
        let (sig_table, overloads) = build_signature_table_with_overloads(
            &parsed.document,
            &std::collections::HashSet::new(),
            &mut diags,
        );
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "got: {:?}",
            diags
        );
        let info = overloads.get("handle").expect("overload info for `handle`");
        assert_eq!(info.enum_name, "Property");
        assert_eq!(info.variants.len(), 2);
        let names: Vec<&str> = info
            .variants
            .iter()
            .map(|v| v.variant_name.as_str())
            .collect();
        assert!(names.contains(&"Scalar"));
        assert!(names.contains(&"Nested"));
        // Public-name entry exists in the sig table.
        assert!(sig_table.contains_key("handle"));
        // Specializations also register under mangled names so
        // analyzer drill-down works.
        assert!(sig_table.contains_key("__handle__Scalar"));
        assert!(sig_table.contains_key("__handle__Nested"));
    }

    #[test]
    fn ad_hoc_overload_emits_hard_error() {
        let src = "\
proc foo {v: int} { return $v }\n\
proc foo {v: string} { return $v }\n";
        let d = diags(src);
        let hit = d
            .iter()
            .find(|d| {
                d.severity == Severity::Error
                    && d.message.contains("ad-hoc overloading")
            })
            .expect("expected ad-hoc-overloading diagnostic");
        assert!(hit.message.contains("foo"), "{:?}", hit);
    }

    #[test]
    fn overload_with_mismatched_enums_errors() {
        let src = "\
enum A = {\n  X\n  Y\n}\n\
enum B = {\n  P\n  Q\n}\n\
proc foo {v: A::X} { }\n\
proc foo {v: B::P} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("mixes enums")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn overload_with_duplicate_variant_errors() {
        let src = "\
enum E = {\n  A: int\n  B: int\n}\n\
proc foo {v: E::A} { }\n\
proc foo {v: E::A} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("two")
                && d.message.contains("E::A")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn overload_with_extra_args_errors() {
        // v1 restricts overloaded procs to exactly one arg (the
        // dispatched variant). Extra tail args trip the arity
        // check.
        let src = "\
enum E = {\n  A: int\n  B: int\n}\n\
proc foo {\n  v: E::A\n  x\n} { }\n\
proc foo {\n  v: E::B\n  y\n} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("exactly ONE arg")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn overload_with_mismatched_return_type_errors() {
        let src = "\
enum E = {\n  A: int\n  B: int\n}\n\
proc foo {v: E::A} int { return 0 }\n\
proc foo {v: E::B} string { return \"\" }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("return type")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn reserved_prefix_user_proc_errors() {
        let src = "proc __foo {v} { return $v }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("reserved")
                && d.message.contains("__")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn qualified_type_in_return_position_errors() {
        let src = "\
enum E = {\n  A\n}\n\
proc bad {} E::A { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("qualified")
                && d.message.contains("only legal")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn qualified_type_in_tail_arg_errors() {
        let src = "\
enum E = {\n  A\n  B\n}\n\
proc bad {\n  v: E::A\n  x: E::B\n} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("qualified")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn qualified_type_inside_generic_errors() {
        let src = "\
enum E = {\n  A\n}\n\
proc bad {x: list<E::A>} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("qualified")),
            "got: {:?}",
            d
        );
    }

    /// A qualified name that resolves to a declared newtype
    /// (`dcmac::GtChProps`) is legal wherever a Named type is
    /// legal — including return-type slots on the newtype's own
    /// `from`/`empty` helpers.
    #[test]
    fn namespaced_newtype_return_type_allowed() {
        let src = "\
namespace eval dcmac {}\n\
namespace eval dcmac::T {}\n\
type dcmac::T = string\n\
proc dcmac::T::repr {v: dcmac::T} string { return $v }\n\
proc dcmac::T::from {v: string} dcmac::T { return $v }\n\
proc dcmac::T::to {v: dcmac::T} string { return $v }\n";
        let d = diags(src);
        let errs: Vec<_> =
            d.iter().filter(|d| d.severity == Severity::Error).collect();
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    /// A namespaced newtype used as a tail-arg type annotation
    /// (non-first-arg position, previously ruled out for
    /// Qualified) passes when the name resolves to a real newtype.
    #[test]
    fn namespaced_newtype_tail_arg_allowed() {
        // `use` returns via `dcmac::T::to` so the return-type
        // check sees a `string`-typed value matching the
        // declared `string` return. A raw `return $slot` (leaking
        // the newtype out as its underlying) is now correctly
        // flagged as a type mismatch — the analyzer wants
        // callers to cross the newtype boundary through the
        // explicit `T::to` conversion.
        let src = "\
namespace eval dcmac {}\n\
namespace eval dcmac::T {}\n\
type dcmac::T = string\n\
proc dcmac::T::repr {v: dcmac::T} string { return $v }\n\
proc dcmac::T::from {v: string} dcmac::T { return $v }\n\
proc dcmac::T::to {v: dcmac::T} string { return $v }\n\
proc use {name slot: dcmac::T} string { return [dcmac::T::to -v $slot] }\n";
        let d = diags(src);
        let errs: Vec<_> =
            d.iter().filter(|d| d.severity == Severity::Error).collect();
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    /// Unknown qualified names (no matching `type` decl) still
    /// get rejected — the disambiguator only clears names that
    /// resolve to a real newtype.
    #[test]
    fn unknown_qualified_still_rejected() {
        let src = "proc bad {name} Unknown::Thing { return $name }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|d| d.severity == Severity::Error
                && d.message.contains("qualified")
                && d.message.contains("only legal")),
            "got: {:?}",
            d
        );
    }

    #[test]
    fn recursive_enum_passes() {
        // Inner generic with whitespace needs brace-wrapping at
        // the word level — that's the existing type-decl rule.
        let src = "\
enum Property = {\n  Scalar: string\n  Nested: Properties\n}\n\
type Properties = {dict<string, Property>}\n\
proc Properties::repr {v} { return $v }\n\
proc Properties::from {v} { return $v }\n\
proc Properties::to {v} { return $v }\n";
        let d = diags(src);
        // No errors — Property/Properties cycle is fine (Tcl
        // resolves at call time) and the triplet exists for the
        // type-decl side.
        assert!(
            d.iter().all(|d| d.severity != Severity::Error),
            "got: {:?}",
            d
        );
    }

    // ------------------------------------------------------------------
    // Undefined `src @<name>` module check.
    // ------------------------------------------------------------------

    fn src_diags(src: &str, known_deps: &[&str]) -> Vec<Diagnostic> {
        let parsed = crate::parser::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "unexpected parse errors: {:?}",
            parsed.errors
        );
        let deps: std::collections::HashSet<String> =
            known_deps.iter().map(|s| s.to_string()).collect();
        validate_with_all_extras_and_vars(
            &parsed.document,
            src,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &deps,
        )
        .into_iter()
        .filter(|d| d.message.starts_with("unknown src module"))
        .collect()
    }

    #[test]
    fn src_at_named_unresolved_flagged() {
        let src = "src @gtwiz-versal\n";
        let d = src_diags(src, &["vivado-cmd", "cpm5"]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, Severity::Error);
        assert!(d[0].message.contains("gtwiz-versal"), "{}", d[0].message);
        assert!(
            d[0].message.contains("[dependencies.gtwiz-versal]"),
            "{}",
            d[0].message
        );
        // Span should cover the `@gtwiz-versal` text.
        let bytes =
            &src.as_bytes()[d[0].span.start as usize..d[0].span.end as usize];
        assert_eq!(std::str::from_utf8(bytes).unwrap(), "@gtwiz-versal");
    }

    #[test]
    fn src_at_named_resolved_clean() {
        let src = "src @vivado-cmd\n";
        let d = src_diags(src, &["vivado-cmd"]);
        assert!(d.is_empty(), "unexpected diags: {d:?}");
    }

    #[test]
    fn src_at_named_multi_flagged() {
        let src = "src @foo\nsrc @bar\nsrc @cpm5\n";
        let d = src_diags(src, &["cpm5"]);
        assert_eq!(d.len(), 2);
        // Distinct spans.
        assert_ne!(d[0].span.start, d[1].span.start);
    }

    #[test]
    fn src_bare_path_not_flagged() {
        // Relative path form — never triggers the `@<name>` check
        // even when known_deps is empty. Filesystem existence gets
        // validated downstream by the loader; the analyzer's
        // job here is only the dep-name lookup.
        let src = "src ./ports.htcl\n";
        let d = src_diags(src, &[]);
        assert!(d.is_empty(), "unexpected diags: {d:?}");
    }

    #[test]
    fn src_subpath_reported_with_hint() {
        // `@foo/sub` — the subpath appears in the diagnostic
        // message so the user sees the exact directive that
        // failed. Also verifies subpath doesn't confuse the
        // classifier.
        let src = "src @gtwiz-versal/module\n";
        let d = src_diags(src, &["cpm5"]);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.contains("@gtwiz-versal/module"),
            "{}",
            d[0].message
        );
    }

    #[test]
    fn empty_deps_no_check() {
        // The check must no-op when the caller doesn't know about
        // any deps — matches the behavior for unit tests / non-
        // workspace-aware callers who invoke `validate` directly.
        let src = "src @gtwiz-versal\nsrc @other\n";
        let d = src_diags(src, &[]);
        assert!(d.is_empty(), "unexpected diags: {d:?}");
    }

    // ------ call-site type check ---------------------------------
    //
    // Three fixtures exercise the arg-type check:
    //
    // 1. Matching newtypes → no diagnostic (happy path).
    // 2. Mismatched newtypes (the copy-paste-wrong-constructor
    //    bug in ip/gtm.htcl) → one diagnostic naming both types.
    // 3. Unknown-type value (literal string) → silent skip
    //    (gradual typing). Sanity-checks that the check doesn't
    //    over-fire on values we can't infer.

    /// Set up a minimal document with two newtypes and one taker
    /// proc that accepts a `-slot` of each. Callers append their
    /// own top-level call.
    fn typed_slots_fixture(tail: &str) -> String {
        format!(
            "type ns::TypeA = Properties\n\
             type ns::TypeB = Properties\n\
             proc make_a {{}} ns::TypeA {{ return {{}} }}\n\
             proc make_b {{}} ns::TypeB {{ return {{}} }}\n\
             proc take {{ @default(\"\") slot: ns::TypeA }} {{ }}\n\
             {tail}",
        )
    }

    #[test]
    fn type_check_matching_types_no_diagnostic() {
        // `-slot [make_a]` — expected ns::TypeA, actual ns::TypeA.
        let src = typed_slots_fixture("take -slot [make_a]\n");
        let d = diags(&src);
        assert!(
            d.iter().all(|x| !x.message.contains("type mismatch")),
            "unexpected type diags: {d:?}",
        );
    }

    #[test]
    fn type_check_mismatched_types_errors() {
        // `-slot [make_b]` — expected ns::TypeA, actual ns::TypeB.
        // This is the class of bug the check exists to catch.
        let src = typed_slots_fixture("take -slot [make_b]\n");
        let d = diags(&src);
        let type_errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("type mismatch"))
            .collect();
        assert_eq!(type_errs.len(), 1, "expected 1 type diag, got {d:?}");
        let msg = &type_errs[0].message;
        assert!(
            msg.contains("ns::TypeA") && msg.contains("ns::TypeB"),
            "message should name both types: {msg}",
        );
        assert!(msg.contains("-slot"), "message should name the arg: {msg}",);
    }

    #[test]
    fn type_check_unknown_value_is_silent() {
        // `-slot hello` — value is a plain literal, type unknown.
        // The check must NOT fire (gradual typing).
        let src = typed_slots_fixture("take -slot hello\n");
        let d = diags(&src);
        assert!(
            d.iter().all(|x| !x.message.contains("type mismatch")),
            "unexpected type diag on untyped literal: {d:?}",
        );
    }

    #[test]
    fn type_check_var_binding_flows_through_set() {
        // `set x [make_b]; take -slot $x` — actual type flows
        // through the `set` binding into the `$x` reference.
        let src = typed_slots_fixture("set x [make_b]\ntake -slot $x\n");
        let d = diags(&src);
        let type_errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("type mismatch"))
            .collect();
        assert_eq!(
            type_errs.len(),
            1,
            "expected 1 type diag through `set`, got {d:?}",
        );
    }

    // ------ bool literal check ----------------------------------
    //
    // Sanity check the four legs of the bool-typed slot machinery:
    // `true` and `false` land clean, `1` and arbitrary garbage
    // both error with a message that names the offending literal
    // and the arg.

    fn bool_slot_fixture(value: &str) -> String {
        format!(
            "proc take {{ @default(false) flag: bool }} {{ }}\n\
             take -flag {value}\n",
        )
    }

    #[test]
    fn bool_literal_true_no_diagnostic() {
        let src = bool_slot_fixture("true");
        let d = diags(&src);
        assert!(
            d.iter().all(|x| !x.message.contains("type mismatch")),
            "unexpected type diag on `true`: {d:?}",
        );
    }

    #[test]
    fn bool_literal_false_no_diagnostic() {
        let src = bool_slot_fixture("false");
        let d = diags(&src);
        assert!(
            d.iter().all(|x| !x.message.contains("type mismatch")),
            "unexpected type diag on `false`: {d:?}",
        );
    }

    #[test]
    fn bool_literal_integer_1_errors() {
        // The specific class of bug this pass exists to catch —
        // `-enable_reg_interface 1` accepted silently today.
        let src = bool_slot_fixture("1");
        let d = diags(&src);
        let type_errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("type mismatch"))
            .collect();
        assert_eq!(type_errs.len(), 1, "expected 1 diag, got {d:?}");
        let msg = &type_errs[0].message;
        assert!(msg.contains("bool"), "message names type: {msg}");
        assert!(msg.contains("1"), "message names literal: {msg}");
        assert!(msg.contains("-flag"), "message names arg: {msg}");
    }

    #[test]
    fn bool_literal_arbitrary_string_errors() {
        // Guards against `potato` / `yes` / `on` sliding through
        // as "Tcl also accepts this as truthy" — HTCL's bool
        // surface is exactly `true` / `false`.
        let src = bool_slot_fixture("potato");
        let d = diags(&src);
        let type_errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("type mismatch"))
            .collect();
        assert_eq!(type_errs.len(), 1, "expected 1 diag, got {d:?}");
        assert!(
            type_errs[0].message.contains("potato"),
            "message names offending literal: {}",
            type_errs[0].message,
        );
    }

    // ------ return-type check ------------------------------------
    //
    // Annotated procs must produce a value whose type matches the
    // declared return type on every `return X` in the body —
    // including returns buried inside `if`/`while`/etc bodies.

    #[test]
    fn return_type_matching_annotation_no_diag() {
        let src = "\
type ns::TypeA = Properties
proc make_a {} ns::TypeA { return {} }
proc use_a {} ns::TypeA {
  set x [make_a]
  return $x
}
";
        let d = diags(src);
        assert!(
            d.iter()
                .all(|x| !x.message.contains("return type mismatch")),
            "unexpected diags: {d:?}",
        );
    }

    #[test]
    fn return_type_mismatch_errors() {
        // Body returns `ns::TypeA` but annotation says `ns::TypeB`.
        // This is the shape the user hit with `configure_gtm`
        // wrongly annotated `cpm5::Config`.
        let src = "\
type ns::TypeA = Properties
type ns::TypeB = Properties
proc make_a {} ns::TypeA { return {} }
proc mismatched {} ns::TypeB {
  set x [make_a]
  return $x
}
";
        let d = diags(src);
        let errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("return type mismatch"))
            .collect();
        assert_eq!(errs.len(), 1, "expected 1 diag, got {d:?}");
        let msg = &errs[0].message;
        assert!(
            msg.contains("ns::TypeA") && msg.contains("ns::TypeB"),
            "message names both types: {msg}",
        );
    }

    #[test]
    fn return_type_check_descends_into_if_body() {
        // Wrong-typed return buried inside an `if` body — the
        // walker parses the braced body and finds the return.
        let src = "\
type ns::TypeA = Properties
type ns::TypeB = Properties
proc make_a {} ns::TypeA { return {} }
proc branchy {} ns::TypeB {
  if 1 {
    set x [make_a]
    return $x
  }
  return {}
}
";
        let d = diags(src);
        let errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("return type mismatch"))
            .collect();
        assert!(!errs.is_empty(), "expected mismatch inside if, got {d:?}");
    }

    #[test]
    fn bare_return_in_annotated_proc_errors() {
        let src = "\
type ns::TypeA = Properties
proc bad {} ns::TypeA {
  return
}
";
        let d = diags(src);
        let errs: Vec<_> = d
            .iter()
            .filter(|x| x.message.contains("bare `return`"))
            .collect();
        assert_eq!(errs.len(), 1, "expected bare-return diag, got {d:?}");
    }

    #[test]
    fn bare_return_in_unit_proc_is_ok() {
        // `unit` return type = "no meaningful value"; bare
        // `return` is idiomatic for side-effecting procs that
        // early-out on a condition.
        let src = "\
proc side_effect {x} unit {
  if $x { return }
  return
}
";
        let d = diags(src);
        assert!(
            d.iter().all(|x| !x.message.contains("bare `return`")),
            "unexpected bare-return diag: {d:?}",
        );
    }

    #[test]
    fn unannotated_proc_return_with_value_errors() {
        // No return annotation + `return X` → an error. The proc's
        // shape declares "side effects only," and a value-carrying
        // return contradicts that.
        let src = "\
proc anything {} { return 42 }
";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|x| x.message.contains("no declared return type")),
            "expected diag, got: {d:?}",
        );
    }

    #[test]
    fn unannotated_proc_bare_return_ok() {
        // Bare `return` is fine in a side-effect proc — common
        // early-exit pattern.
        let src = "\
proc anything {} { puts hi; return }
";
        let d = diags(src);
        assert!(
            d.iter()
                .all(|x| !x.message.contains("no declared return type")),
            "unexpected diag, got: {d:?}",
        );
    }

    #[test]
    fn unannotated_proc_return_value_inside_if_errors() {
        // A value-return buried in a control-flow branch still fires.
        let src = "\
proc anything { x: int } { if {$x > 0} { return 42 } }
";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|x| x.message.contains("no declared return type")),
            "expected diag, got: {d:?}",
        );
    }

    // ─── must-return / fallthrough analysis ─────────────────────

    fn has_fallthrough_diag(d: &[Diagnostic]) -> bool {
        d.iter().any(|x| x.message.contains("may fall through"))
    }

    #[test]
    fn empty_body_annotated_proc_errors() {
        // The user's `configure_txr1` case: annotated with a
        // real type but the body is empty. Must-return should
        // flag it.
        let src = "\
type MyType = string
proc configure_txr1 {} MyType { }
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn single_puts_body_annotated_proc_errors() {
        // Body has a side-effecting `puts` and no return; the
        // last statement's result isn't a MyType.
        let src = "\
type MyType = string
proc foo {} MyType {
  puts hello
}
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn if_no_else_annotated_proc_errors() {
        let src = "\
type MyType = string
proc bad {} MyType {
  if 1 {
    return {}
  }
}
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn while_body_return_annotated_proc_errors() {
        // Even with a `return` inside the loop body, the loop
        // may not execute — must-return still fires.
        let src = "\
type MyType = string
proc bad {} MyType {
  while 1 {
    return {}
  }
}
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn if_else_both_return_no_error() {
        let src = "\
type MyType = string
proc ok {} MyType {
  if 1 {
    return {}
  } else {
    return {}
  }
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn if_elseif_else_all_return_no_error() {
        let src = "\
type MyType = string
proc ok {} MyType {
  if 1 {
    return {}
  } elseif 2 {
    return {}
  } else {
    return {}
  }
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn implicit_last_expression_typed_proc_call_no_error() {
        // Trailing typed proc-call is an implicit return in Tcl.
        let src = "\
type MyType = string
proc make_it {} MyType { return {} }
proc user {} MyType {
  make_it
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn implicit_last_expression_extern_call_no_error() {
        // Trailing extern call is the user's opt-out for raw
        // Tcl. Trust it as a valid implicit return.
        let src = "\
type MyType = string
proc user {} MyType {
  extern::some_raw_tcl_proc -foo bar
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn switch_with_default_all_return_no_error() {
        let src = "\
type MyType = string
proc ok {} MyType {
  switch $x {
    a { return {} }
    default { return {} }
  }
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn switch_no_default_errors() {
        let src = "\
type MyType = string
proc bad {} MyType {
  switch $x {
    a { return {} }
    b { return {} }
  }
}
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn switch_default_falls_through_errors() {
        let src = "\
type MyType = string
proc bad {} MyType {
  switch $x {
    a { return {} }
    default { puts hi }
  }
}
";
        let d = diags(src);
        assert!(has_fallthrough_diag(&d), "diags: {d:?}");
    }

    #[test]
    fn try_body_and_handler_terminate_no_error() {
        // Matches the vw-ip generator's wrap pattern:
        // `try { return X } on error { error "…" }`.
        let src = "\
type MyType = string
proc gen {} MyType {
  try {
    return {}
  } on error {msg} {
    error \"foo.$msg\"
  }
}
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn unit_annotated_empty_body_no_error() {
        // `unit` return type doesn't require a value.
        let src = "\
proc side_effect {} unit { }
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn newtype_triplet_empty_body_no_error() {
        // T::from and friends are exempted before the must-return
        // check fires.
        let src = "\
type ns::T = string
proc ns::T::from {v: string} ns::T { return $v }
proc ns::T::empty {} ns::T { }
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    #[test]
    fn enum_overload_arm_empty_body_no_error() {
        // First-arg-Qualified shape (overload arm) is exempted
        // before the must-return check fires.
        let src = "\
enum E = {
  A: string
  B: string
}
proc handle {v: E::A} string { }
";
        let d = diags(src);
        assert!(!has_fallthrough_diag(&d), "unexpected diags: {d:?}");
    }

    // ─── @test attribute semantic checks ───────────────────────

    #[test]
    fn test_attribute_dedicated_eda_ok() {
        let src = "@test(dedicated-eda)\nproc t {} { }\n";
        let d = diags(src);
        assert!(
            d.iter().all(|x| !x.message.contains("dedicated-eda")),
            "unexpected diag: {d:?}",
        );
    }

    #[test]
    fn test_attribute_wrong_positional_value_warns() {
        let src = "@test(bogus)\nproc t {} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("dedicated-eda")),
            "expected `dedicated-eda`-related warning: {d:?}",
        );
    }

    #[test]
    fn test_attribute_on_proc_with_args_warns() {
        // Zero-arg only for MVP.
        let src = "@test\nproc t {x: int} { }\n";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("zero arguments")),
            "expected zero-arg warning: {d:?}",
        );
    }

    #[test]
    fn test_attribute_on_nested_proc_warns() {
        // Only top-level @test procs are runnable.
        let src = "\
proc outer {} {
  @test
  proc inner {} { puts hi }
}
";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("nested proc")),
            "expected nested-proc warning: {d:?}",
        );
    }

    // ─── @test(target=…) keyed-item checks ─────────────────────

    #[test]
    fn test_attribute_with_dedicated_eda_and_target_ok() {
        let src = "\
@test(dedicated-eda target=\"xcvm3358-vsvh1747-2M-e-S\")
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().all(|x| !x.message.contains("`@test")
                && !x.message.contains("target=")),
            "unexpected @test diag: {d:?}",
        );
    }

    #[test]
    fn test_target_without_dedicated_eda_warns() {
        // Shared bucket can't honor per-test parts.
        let src = "\
@test(target=\"xcvm3358-vsvh1747-2M-e-S\")
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("dedicated-eda")),
            "expected dedicated-eda requirement warning: {d:?}",
        );
    }

    #[test]
    fn test_attribute_unknown_key_warns() {
        let src = "\
@test(dedicated-eda family=versal)
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("unrecognized key")),
            "expected unrecognized-key warning: {d:?}",
        );
    }

    #[test]
    fn test_attribute_variant_with_dedicated_eda_ok() {
        let src = "\
@test(dedicated-eda variant=\"vpk120\")
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().all(|x| !x.message.contains("`@test")),
            "unexpected @test diag: {d:?}",
        );
    }

    #[test]
    fn test_variant_without_dedicated_eda_warns() {
        let src = "\
@test(variant=\"vpk120\")
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("dedicated-eda")),
            "expected dedicated-eda requirement warning: {d:?}",
        );
    }

    #[test]
    fn test_target_and_variant_together_warns() {
        // Mutually exclusive within `@test` — variants own their
        // parts, so specifying both is a config bug.
        let src = "\
@test(dedicated-eda target=\"xcv...\" variant=\"vpk120\")
proc t {} { }
";
        let d = diags(src);
        assert!(
            d.iter().any(|x| x.severity == Severity::Warning
                && x.message.contains("pick one of")),
            "expected pick-one-of warning: {d:?}",
        );
    }
}
