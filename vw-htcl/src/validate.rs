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
    let mut diags = Vec::new();
    let (mut table, _overloads) =
        build_signature_table_with_overloads(document, &mut diags);
    // Prior-batch signatures fill in the gaps. The doc's own entries
    // win because `entry().or_insert(...)` is a no-op on present keys.
    for (name, sig) in extra_sigs {
        table.entry(name.clone()).or_insert(*sig);
    }
    let mut type_table = build_type_decl_table(document, &mut diags);
    for (name, td) in extra_types {
        type_table.entry(name.clone()).or_insert(*td);
    }
    let mut enum_table = build_enum_decl_table(document, &mut diags);
    for (name, ed) in extra_enums {
        enum_table.entry(name.clone()).or_insert(*ed);
    }
    validate_type_decl_triplets(&type_table, &table, &mut diags);
    validate_enum_decls(&enum_table, &type_table, &mut diags);
    validate_qualified_positions(document, &mut diags);
    validate_stmts(&document.stmts, source, &table, &mut diags);
    diags
}

/// Validate every command in `stmts`, descending into proc bodies so
/// that calls nested inside a proc are checked just like top-level
/// ones. The signature table is document-wide, so a call resolves to
/// its (top-level) proc at any depth.
fn validate_stmts(
    stmts: &[Stmt],
    source: &str,
    table: &HashMap<String, &ProcSignature>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        validate_command(cmd, source, table, diags);
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                validate_stmts(&proc.body, source, table, diags);
            }
            CommandKind::NamespaceEval(ns) => {
                // Calls inside the namespace body are validated the
                // same way; the signature-table is document-wide so
                // a call to `project::set_target_language` from
                // anywhere resolves to the same entry. (Bare,
                // sibling-relative calls inside a namespace body
                // aren't auto-qualified yet — write the qualified
                // name explicitly.)
                validate_stmts(&ns.body, source, table, diags);
            }
            _ => {}
        }
        // Also descend into any `[ … ]` command substitutions on this
        // command's words so calls written inline get validated the
        // same as top-level ones.
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    validate_stmts(body, source, table, diags);
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
    let (table, _overloads) =
        build_signature_table_with_overloads(document, diags);
    table
}

/// Same as [`build_signature_table`] but also returns the
/// [`OverloadTable`] side-map. Callers that need to know whether a
/// given proc name resolves through enum-overload dispatch (codegen,
/// hover, signature help) consult this.
pub fn build_signature_table_with_overloads<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> (HashMap<String, &'doc ProcSignature>, OverloadTable) {
    // First pass: collect every proc decl per qualified name,
    // preserving order so a "first wins" / "last wins" choice is
    // unambiguous when we have to make one. Multi-decl entries are
    // candidate overload sets; single-decl entries are normal
    // procs.
    let mut multi: HashMap<String, Vec<(&'doc Proc, &'doc ProcSignature)>> =
        HashMap::new();
    collect_signatures_multi(&document.stmts, "", &mut multi, diags);

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
                // Detect this here so the user gets a clear error
                // instead of a confused runtime behavior.
                let is_qualified_first = sig
                    .args
                    .first()
                    .and_then(|a| a.type_annotation.as_ref())
                    .map(|t| matches!(t, TypeExpr::Qualified { .. }))
                    .unwrap_or(false);
                if is_qualified_first && !prefix.is_empty() {
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
                collect_signatures_multi(&ns.body, &nested, multi, diags);
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
/// indicators; they're nonsense as return types, generic args,
/// nested type positions, or any non-first-arg slot.
fn validate_qualified_positions(
    document: &Document,
    diags: &mut Vec<Diagnostic>,
) {
    fn walk_stmts(stmts: &[Stmt], diags: &mut Vec<Diagnostic>) {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            match &cmd.kind {
                CommandKind::Proc(proc) => {
                    if let Some(sig) = proc.signature.as_ref() {
                        for (i, arg) in sig.args.iter().enumerate() {
                            if let Some(ty) = arg.type_annotation.as_ref() {
                                // The first arg may be Qualified;
                                // tail args may NOT.
                                let allow_qualified = i == 0;
                                reject_nested_qualified(
                                    ty,
                                    allow_qualified,
                                    diags,
                                );
                            }
                        }
                        if let Some(ret) = sig.return_type.as_ref() {
                            reject_nested_qualified(ret, false, diags);
                        }
                    }
                    walk_stmts(&proc.body, diags);
                }
                CommandKind::NamespaceEval(ns) => {
                    walk_stmts(&ns.body, diags);
                }
                CommandKind::TypeDecl(td) => {
                    if let Some(ty) = td.underlying.as_ref() {
                        reject_nested_qualified(ty, false, diags);
                    }
                }
                CommandKind::EnumDecl(ed) => {
                    for v in &ed.variants {
                        if let Some(ty) = v.payload.as_ref() {
                            reject_nested_qualified(ty, false, diags);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    walk_stmts(&document.stmts, diags);
}

fn reject_nested_qualified(
    ty: &TypeExpr,
    allow_top_qualified: bool,
    diags: &mut Vec<Diagnostic>,
) {
    match ty {
        TypeExpr::Named { .. } => {}
        TypeExpr::Generic { args, .. } => {
            // Inside a generic, nested Qualified is never allowed.
            for a in args {
                reject_nested_qualified(a, false, diags);
            }
        }
        TypeExpr::Qualified {
            namespace,
            variant,
            span,
            ..
        } => {
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
                let actual_name = match actual {
                    TypeExpr::Named { name, .. } => name.as_str(),
                    TypeExpr::Generic { name, .. } => name.as_str(),
                    // A qualified type like `E::V` shouldn't appear
                    // as a newtype's return type — that's caught by
                    // the dedicated Qualified-position validator
                    // step. If it slips through, render the
                    // namespace name so the user sees something
                    // meaningful in the diagnostic.
                    TypeExpr::Qualified { namespace, .. } => namespace.as_str(),
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
        // Unknown call. If it uses `-flag` keyword arguments, the
        // user probably meant an htcl wrapper that isn't loaded —
        // shipping it to the EDA backend would either error
        // cryptically or do something nonsensical with the
        // arguments. Force the user to be explicit: either `src` a
        // wrapper module, or use `extern::<name>` for the raw
        // Tcl/EDA proc.
        let uses_keyword = cmd.words.iter().skip(1).any(|w| {
            w.as_text()
                .is_some_and(|t| t.starts_with('-') && t.len() > 1)
        });
        if uses_keyword && !is_known_tcl_builtin(call_name) {
            let hint = match suggest_name(call_name, table.keys()) {
                Some(s) => format!(" — did you mean `{s}`?"),
                None => String::new(),
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
                    AttributeValue::Integer { .. } => continue,
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
                    AttributeValue::Integer { .. } => continue,
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
                AttributeValue::Integer { .. } => continue,
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
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = levenshtein(target, cand);
        if d == 0 || d > threshold {
            continue;
        }
        if best.map(|(b, _)| d < b).unwrap_or(true) {
            best = Some((d, cand.as_str()));
        }
    }
    best.map(|(_, s)| s.to_string())
}

/// Standard Levenshtein edit distance — number of single-character
/// insertions, deletions, or substitutions to turn `a` into `b`.
/// Two-row rolling table; O(n*m) time, O(n) space.
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
        validate(&parsed.document, src)
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
    fn known_tcl_builtin_with_keyword_args_is_allowed() {
        // `string match -nocase ...` is a legitimate Tcl-core
        // pattern; the allowlist keeps it from triggering the
        // unknown-call error.
        let src = "string match -nocase pat str\n";
        assert!(diags(src).is_empty());
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
        let (sig_table, overloads) =
            build_signature_table_with_overloads(&parsed.document, &mut diags);
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
}
