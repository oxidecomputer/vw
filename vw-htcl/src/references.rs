// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Symbol-references core.
//!
//! The vw-htcl side of `textDocument/references` and
//! `textDocument/rename`. Splits the problem into two orthogonal
//! pieces:
//!
//! 1. **[`identify_at`]** — given a cursor `offset`, figure out what
//!    the user is pointing at. Returns a [`ReferenceTarget`] that
//!    names the symbol in a way independent of the source file it
//!    was found in.
//! 2. **[`find_references_in`]** — given a target, walk a document
//!    and return every source span that is a use or a decl of that
//!    target.
//!
//! Both procs and types cross file boundaries in real workspaces,
//! so the LSP layer typically calls (1) on the file the cursor is
//! in and (2) on every `.htcl` file under the workspace root. The
//! two functions never talk to each other — the target flows in as
//! a plain value.
//!
//! Locals and proc args have file-local scope by construction, so
//! `find_references_in` returns the empty set for them when passed
//! a document that doesn't contain the declaration. The LSP layer
//! uses this to skip the cross-file scan for local kinds.

use crate::ast::{
    AttributeValue, Command, CommandKind, Document, Proc, ProcSignature, Stmt,
    TypeExpr, Word, WordForm, WordPart,
};
use crate::hover::is_body_host;
use crate::scope::{resolve_var_def, scan_var_ref, VarDef};
use crate::span::Span;

/// A symbol whose references we want to find. Kinds carry the
/// identifying data needed to match uses across files (procs and
/// types by qualified name) or to bound the scope to a single
/// declaration (locals and proc args).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceTarget {
    /// A proc named exactly `name` — either bare (`configure_gtm`) or
    /// namespaced (`vivado_cmd::create_bd_cell`). Matching is
    /// literal on the head-word text of every command, plus the
    /// name-word of every `proc` decl. Cross-file.
    Proc { name: String },
    /// A `type NAME = …` declaration and every `: NAME` /
    /// `Generic<NAME>` reference. `name` is the declared (possibly
    /// qualified) form. Cross-file.
    Type { name: String },
    /// A single variant of an enum. Referred to as
    /// `<enum>::<variant>` in qualified type annotations and as
    /// `<enum>::<variant>` at construction sites. Cross-file.
    EnumVariant { enum_name: String, variant: String },
    /// A `set VAR …` / `variable VAR` / `foreach VAR …` / `upvar
    /// … LOCAL` inside a specific scope. File-local by
    /// construction. `decl_scope_span` is the span that bounds the
    /// scope (a proc body's span, or the whole document for
    /// top-level locals) so that scans in OTHER files return the
    /// empty set instead of accidentally matching a same-name
    /// local elsewhere.
    Local { name: String, decl_scope_span: Span },
    /// A proc-arg parameter. Emitted when the cursor is on the arg
    /// itself, on a body-level `$name` reference, or on an
    /// attribute-ident value that names the arg. File-local by
    /// the same reasoning as [`Local`].
    ProcArg {
        proc_name: Option<String>,
        arg_name: String,
        /// Span of the enclosing proc body, used to bound the ref
        /// scan the same way `Local` does.
        decl_scope_span: Span,
    },
}

/// If `offset` lands on something we can identify as a reference
/// target, return it. Order of tries is narrowest-first so the
/// less-specific fallbacks don't misclassify.
///
/// Returns `None` when the cursor isn't on an identifier we know
/// how to track (whitespace, comment interior, keyword, arbitrary
/// argument text, etc.).
pub fn identify_at(
    document: &Document,
    source: &str,
    offset: u32,
) -> Option<ReferenceTarget> {
    // 1. Proc-arg identification. Narrowest by construction —
    //    only fires when the cursor is on an arg-name-shaped word
    //    inside a proc signature, an attr ident referring to a
    //    sibling arg, or a `$name` in a body resolving to an arg.
    if let Some((proc, arg_name)) =
        find_proc_arg_at(&document.stmts, source, offset)
    {
        return Some(ReferenceTarget::ProcArg {
            proc_name: proc.name.clone(),
            arg_name,
            decl_scope_span: proc.body_span,
        });
    }
    // 2. Enum-variant reference (`Enum::Variant` in a type
    //    annotation, or `Enum::Variant` in a construction call
    //    like `Enum::Variant -payload $x`). Narrower than plain
    //    Proc because a Qualified type annotation is only
    //    parsed that way when the AST tagged it as such.
    if let Some(t) = identify_enum_variant_at(document, offset) {
        return Some(t);
    }
    // 3. Type reference or type decl. Fires on `type NAME = …`
    //    name spans and on any Named/Generic type-expression
    //    name-span in a decl or annotation.
    if let Some(t) = identify_type_at(document, offset) {
        return Some(t);
    }
    // 4. Proc reference or proc decl. Fires on the name span of
    //    a `proc` decl and on the head word of any Command.
    if let Some(t) = identify_proc_at(document, offset) {
        return Some(t);
    }
    // 5. Local variable — `set`/`variable`/`foreach`/`upvar` decl
    //    spans, and `$name` references in the enclosing scope.
    if let Some(t) = identify_local_at(document, source, offset) {
        return Some(t);
    }
    None
}

/// Walk `document` collecting every span that references
/// `target`. Includes both use sites and (for cross-file kinds)
/// the declaration span itself so the rename pipeline gets a
/// single unified list.
///
/// For file-local kinds (`Local`, `ProcArg`), returns the empty
/// set when the document doesn't contain the declaration —
/// callers use this to skip the cross-file scan.
pub fn find_references_in(
    document: &Document,
    source: &str,
    target: &ReferenceTarget,
) -> Vec<Span> {
    let mut out = Vec::new();
    match target {
        ReferenceTarget::Proc { name } => {
            find_proc_refs(&document.stmts, name, &mut out);
        }
        ReferenceTarget::Type { name } => {
            find_type_refs(&document.stmts, name, &mut out);
        }
        ReferenceTarget::EnumVariant { enum_name, variant } => {
            find_enum_variant_refs(
                &document.stmts,
                enum_name,
                variant,
                &mut out,
            );
        }
        ReferenceTarget::Local {
            name,
            decl_scope_span,
        } => {
            // Only scan if the scope is present in THIS document.
            // `decl_scope_span` is an absolute-source offset from
            // whatever document defined the local; a different
            // file would have different byte offsets and this
            // check would fail — which is the intent (file-local
            // kinds don't leak across files).
            if let Some(scope_stmts) =
                scope_stmts_by_span(document, *decl_scope_span)
            {
                collect_local_decl_spans(scope_stmts, name, &mut out);
                collect_var_ref_spans(scope_stmts, source, name, &mut out);
            }
        }
        ReferenceTarget::ProcArg {
            arg_name,
            decl_scope_span,
            ..
        } => {
            if let Some(proc) =
                find_proc_by_body_span(&document.stmts, *decl_scope_span)
            {
                if let Some(sig) = &proc.signature {
                    if let Some(arg) =
                        sig.args.iter().find(|a| &a.name == arg_name)
                    {
                        out.push(arg.name_span);
                    }
                    for attr_span in attribute_ident_ref_spans(sig, arg_name) {
                        out.push(attr_span);
                    }
                }
                collect_var_ref_spans(&proc.body, source, arg_name, &mut out);
            }
        }
    }
    out.sort_by_key(|s| (s.start, s.end));
    out.dedup();
    out
}

// ─── identify_at helpers ────────────────────────────────────────────

fn identify_proc_at(
    document: &Document,
    offset: u32,
) -> Option<ReferenceTarget> {
    // Cursor on a `proc NAME { … }` decl?
    if let Some(name) = proc_decl_name_at(&document.stmts, offset) {
        return Some(ReferenceTarget::Proc { name });
    }
    // Cursor on a command's head word (any call)?
    if let Some(name) = command_head_name_at(&document.stmts, offset) {
        return Some(ReferenceTarget::Proc { name });
    }
    None
}

fn proc_decl_name_at(stmts: &[Stmt], offset: u32) -> Option<String> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(name) = &proc.name {
                    if proc.name_span.contains(offset) {
                        return Some(name.clone());
                    }
                }
                if let Some(name) = proc_decl_name_at(&proc.body, offset) {
                    return Some(name);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(name) = proc_decl_name_at(&ns.body, offset) {
                    return Some(name);
                }
            }
            _ => {}
        }
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    if let Some(name) = proc_decl_name_at(body, offset) {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

fn command_head_name_at(stmts: &[Stmt], offset: u32) -> Option<String> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if let CommandKind::Generic = cmd.kind {
            if let Some(head) = cmd.words.first() {
                if head.span.contains(offset) {
                    if let Some(t) = head.as_text() {
                        return Some(t.to_string());
                    }
                }
            }
        }
        // Recurse for nested calls / bodies.
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(n) = command_head_name_at(&proc.body, offset) {
                    return Some(n);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(n) = command_head_name_at(&ns.body, offset) {
                    return Some(n);
                }
            }
            _ => {}
        }
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    if let Some(n) = command_head_name_at(body, offset) {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

fn identify_type_at(
    document: &Document,
    offset: u32,
) -> Option<ReferenceTarget> {
    // Cursor on a `type NAME = …` decl name?
    if let Some(name) = type_decl_name_at(&document.stmts, offset) {
        return Some(ReferenceTarget::Type { name });
    }
    // Cursor on a type-expression name (annotation or nested)?
    if let Some(name) = type_expr_name_at(&document.stmts, offset) {
        return Some(ReferenceTarget::Type { name });
    }
    None
}

fn type_decl_name_at(stmts: &[Stmt], offset: u32) -> Option<String> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::TypeDecl(td) => {
                if let Some(name) = &td.name {
                    if td.name_span.contains(offset) {
                        return Some(name.clone());
                    }
                }
            }
            CommandKind::Proc(proc) => {
                if let Some(n) = type_decl_name_at(&proc.body, offset) {
                    return Some(n);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(n) = type_decl_name_at(&ns.body, offset) {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    None
}

fn type_expr_name_at(stmts: &[Stmt], offset: u32) -> Option<String> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(sig) = &proc.signature {
                    for arg in &sig.args {
                        if let Some(ty) = &arg.type_annotation {
                            if let Some(n) = type_expr_name_span_hit(ty, offset)
                            {
                                return Some(n);
                            }
                        }
                    }
                }
                if let Some(ty) = &proc.return_type {
                    if let Some(n) = type_expr_name_span_hit(ty, offset) {
                        return Some(n);
                    }
                }
                if let Some(n) = type_expr_name_at(&proc.body, offset) {
                    return Some(n);
                }
            }
            CommandKind::TypeDecl(td) => {
                if let Some(ty) = &td.underlying {
                    if let Some(n) = type_expr_name_span_hit(ty, offset) {
                        return Some(n);
                    }
                }
            }
            CommandKind::EnumDecl(ed) => {
                for variant in &ed.variants {
                    if let Some(ty) = &variant.payload {
                        if let Some(n) = type_expr_name_span_hit(ty, offset) {
                            return Some(n);
                        }
                    }
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(n) = type_expr_name_at(&ns.body, offset) {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    None
}

/// If `offset` lands on the name portion of `ty`, return that
/// name. Recurses through generic args. For `Qualified` we
/// deliberately return `None` — those are handled by the
/// enum-variant identifier, which is more specific.
fn type_expr_name_span_hit(ty: &TypeExpr, offset: u32) -> Option<String> {
    match ty {
        TypeExpr::Named { name, span } => {
            if span.contains(offset) {
                Some(name.clone())
            } else {
                None
            }
        }
        TypeExpr::Generic {
            name,
            name_span,
            args,
            ..
        } => {
            if name_span.contains(offset) {
                return Some(name.clone());
            }
            for a in args {
                if let Some(n) = type_expr_name_span_hit(a, offset) {
                    return Some(n);
                }
            }
            None
        }
        TypeExpr::Qualified { .. } => None,
    }
}

fn identify_enum_variant_at(
    document: &Document,
    offset: u32,
) -> Option<ReferenceTarget> {
    // Cursor on an enum-decl variant name?
    if let Some(t) = enum_decl_variant_at(&document.stmts, offset) {
        return Some(t);
    }
    // Cursor on a `Qualified{ns, variant}` type annotation in a
    // proc arg (overload-arm shape)?
    if let Some(t) = qualified_type_variant_at(&document.stmts, offset) {
        return Some(t);
    }
    // Cursor on a construction call `Enum::Variant -payload …`
    // — matched purely by name shape (`NAME::NAME`) in a command
    // head. Only recognized when there's a matching enum decl
    // anywhere in the document.
    if let Some(t) = enum_construct_head_at(document, offset) {
        return Some(t);
    }
    None
}

fn enum_decl_variant_at(
    stmts: &[Stmt],
    offset: u32,
) -> Option<ReferenceTarget> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::EnumDecl(ed) => {
                let Some(enum_name) = &ed.name else { continue };
                for variant in &ed.variants {
                    if variant.name_span.contains(offset) {
                        return Some(ReferenceTarget::EnumVariant {
                            enum_name: enum_name.clone(),
                            variant: variant.name.clone(),
                        });
                    }
                }
            }
            CommandKind::Proc(proc) => {
                if let Some(t) = enum_decl_variant_at(&proc.body, offset) {
                    return Some(t);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(t) = enum_decl_variant_at(&ns.body, offset) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

fn qualified_type_variant_at(
    stmts: &[Stmt],
    offset: u32,
) -> Option<ReferenceTarget> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(sig) = &proc.signature {
                    for arg in &sig.args {
                        if let Some(TypeExpr::Qualified {
                            namespace,
                            variant,
                            span,
                            ..
                        }) = &arg.type_annotation
                        {
                            if span.contains(offset) {
                                return Some(ReferenceTarget::EnumVariant {
                                    enum_name: namespace.clone(),
                                    variant: variant.clone(),
                                });
                            }
                        }
                    }
                }
                if let Some(t) = qualified_type_variant_at(&proc.body, offset) {
                    return Some(t);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(t) = qualified_type_variant_at(&ns.body, offset) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

fn enum_construct_head_at(
    document: &Document,
    offset: u32,
) -> Option<ReferenceTarget> {
    let (name, span) = command_head_qualified_at(&document.stmts, offset)?;
    // Parse as `Enum::Variant` — first-level namespace only.
    let (ns, variant) = split_first_scope(&name)?;
    // Only accept if there's a matching enum decl in the doc.
    if !enum_decl_exists(&document.stmts, ns) {
        return None;
    }
    // `span` was the whole head-word; the cursor lands on the
    // string covered by it, so ownership by variant vs namespace
    // is captured by the "any part of the compound" rule the
    // caller wanted. Return the variant identity either way.
    let _ = span;
    Some(ReferenceTarget::EnumVariant {
        enum_name: ns.to_string(),
        variant: variant.to_string(),
    })
}

fn command_head_qualified_at(
    stmts: &[Stmt],
    offset: u32,
) -> Option<(String, Span)> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if let CommandKind::Generic = cmd.kind {
            if let Some(head) = cmd.words.first() {
                if head.span.contains(offset) {
                    if let Some(t) = head.as_text() {
                        if t.contains("::") {
                            return Some((t.to_string(), head.span));
                        }
                    }
                }
            }
        }
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(x) = command_head_qualified_at(&proc.body, offset) {
                    return Some(x);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(x) = command_head_qualified_at(&ns.body, offset) {
                    return Some(x);
                }
            }
            _ => {}
        }
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    if let Some(x) = command_head_qualified_at(body, offset) {
                        return Some(x);
                    }
                }
            }
        }
    }
    None
}

fn split_first_scope(name: &str) -> Option<(&str, &str)> {
    let (ns, rest) = name.split_once("::")?;
    // Only match single-segment variants: `Enum::Variant`, not
    // `Enum::Nested::Something`.
    if rest.contains("::") {
        return None;
    }
    Some((ns, rest))
}

fn enum_decl_exists(stmts: &[Stmt], name: &str) -> bool {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::EnumDecl(ed) if ed.name.as_deref() == Some(name) => {
                return true;
            }
            CommandKind::Proc(proc) if enum_decl_exists(&proc.body, name) => {
                return true;
            }
            CommandKind::NamespaceEval(ns)
                if enum_decl_exists(&ns.body, name) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn identify_local_at(
    document: &Document,
    source: &str,
    offset: u32,
) -> Option<ReferenceTarget> {
    let (scope_stmts, scope_span, enclosing) =
        innermost_scope(document, offset)?;
    // On a decl target?
    if let Some(name) = local_decl_name_at(scope_stmts, offset) {
        return Some(ReferenceTarget::Local {
            name: name.to_string(),
            decl_scope_span: scope_span,
        });
    }
    // On a `$var` that resolves to a local?
    let name = var_ref_name_in_scope(scope_stmts, source, offset)?;
    match resolve_var_def(&name, scope_stmts, enclosing, offset)? {
        VarDef::Local(_) => Some(ReferenceTarget::Local {
            name,
            decl_scope_span: scope_span,
        }),
        VarDef::Param(_) => None,
    }
}

// ─── find_references_in helpers ─────────────────────────────────────

fn find_proc_refs(stmts: &[Stmt], name: &str, out: &mut Vec<Span>) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if proc.name.as_deref() == Some(name) {
                    out.push(proc.name_span);
                }
                find_proc_refs(&proc.body, name, out);
            }
            CommandKind::NamespaceEval(ns) => {
                find_proc_refs(&ns.body, name, out);
            }
            CommandKind::Generic => {
                if let Some(head) = cmd.words.first() {
                    if head.as_text() == Some(name) {
                        out.push(head.span);
                    }
                }
            }
            _ => {}
        }
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    find_proc_refs(body, name, out);
                }
            }
        }
    }
}

fn find_type_refs(stmts: &[Stmt], name: &str, out: &mut Vec<Span>) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::TypeDecl(td) => {
                if td.name.as_deref() == Some(name) {
                    out.push(td.name_span);
                }
                if let Some(ty) = &td.underlying {
                    collect_type_expr_name_spans(ty, name, out);
                }
            }
            CommandKind::EnumDecl(ed) => {
                for variant in &ed.variants {
                    if let Some(ty) = &variant.payload {
                        collect_type_expr_name_spans(ty, name, out);
                    }
                }
            }
            CommandKind::Proc(proc) => {
                if let Some(sig) = &proc.signature {
                    for arg in &sig.args {
                        if let Some(ty) = &arg.type_annotation {
                            collect_type_expr_name_spans(ty, name, out);
                        }
                    }
                }
                if let Some(ty) = &proc.return_type {
                    collect_type_expr_name_spans(ty, name, out);
                }
                find_type_refs(&proc.body, name, out);
            }
            CommandKind::NamespaceEval(ns) => {
                find_type_refs(&ns.body, name, out);
            }
            _ => {}
        }
    }
}

fn collect_type_expr_name_spans(
    ty: &TypeExpr,
    name: &str,
    out: &mut Vec<Span>,
) {
    match ty {
        TypeExpr::Named { name: n, span } => {
            if n == name {
                out.push(*span);
            }
        }
        TypeExpr::Generic {
            name: n,
            name_span,
            args,
            ..
        } => {
            if n == name {
                out.push(*name_span);
            }
            for a in args {
                collect_type_expr_name_spans(a, name, out);
            }
        }
        TypeExpr::Qualified { .. } => {}
    }
}

fn find_enum_variant_refs(
    stmts: &[Stmt],
    enum_name: &str,
    variant: &str,
    out: &mut Vec<Span>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::EnumDecl(ed)
                if ed.name.as_deref() == Some(enum_name) =>
            {
                for v in &ed.variants {
                    if v.name == variant {
                        out.push(v.name_span);
                    }
                }
            }
            CommandKind::Proc(proc) => {
                if let Some(sig) = &proc.signature {
                    for arg in &sig.args {
                        if let Some(TypeExpr::Qualified {
                            namespace,
                            variant: v2,
                            span,
                            ..
                        }) = &arg.type_annotation
                        {
                            if namespace == enum_name && v2 == variant {
                                out.push(*span);
                            }
                        }
                    }
                }
                find_enum_variant_refs(&proc.body, enum_name, variant, out);
            }
            CommandKind::NamespaceEval(ns) => {
                find_enum_variant_refs(&ns.body, enum_name, variant, out);
            }
            CommandKind::Generic => {
                if let Some(head) = cmd.words.first() {
                    let expected = format!("{enum_name}::{variant}");
                    if head.as_text() == Some(&expected) {
                        out.push(head.span);
                    }
                }
            }
            _ => {}
        }
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    find_enum_variant_refs(body, enum_name, variant, out);
                }
            }
        }
    }
}

// ─── local + proc-arg helpers, adapted from rename.rs ──────────────

/// Walk the AST looking for a scope whose enclosing span
/// contains `offset`. Returns the statements list, the scope's
/// bounding span, and the enclosing proc when nested inside one
/// (for arg-name resolution).
fn innermost_scope(
    document: &Document,
    offset: u32,
) -> Option<(&[Stmt], Span, Option<&Proc>)> {
    // Try each proc body's stmts, deepest-first. Fall back to
    // the document top level.
    fn inner<'a>(
        stmts: &'a [Stmt],
        _top_span: Span,
        offset: u32,
        enclosing: Option<&'a Proc>,
    ) -> Option<(&'a [Stmt], Span, Option<&'a Proc>)> {
        for stmt in stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            match &cmd.kind {
                CommandKind::Proc(proc) if proc.body_span.contains(offset) => {
                    return inner(
                        &proc.body,
                        proc.body_span,
                        offset,
                        Some(proc),
                    )
                    .or(Some((
                        &proc.body,
                        proc.body_span,
                        Some(proc),
                    )));
                }
                CommandKind::NamespaceEval(ns)
                    if ns.body_span.contains(offset) =>
                {
                    return inner(&ns.body, ns.body_span, offset, enclosing)
                        .or(Some((&ns.body, ns.body_span, enclosing)));
                }
                _ => {}
            }
        }
        None
    }
    let doc_span = Span::new(0, u32::MAX);
    inner(&document.stmts, doc_span, offset, None).or(Some((
        &document.stmts,
        doc_span,
        None,
    )))
}

/// Return the stmts of the scope whose bounding span equals
/// `scope_span`. `Span::new(0, u32::MAX)` means "the document
/// top level."
fn scope_stmts_by_span(
    document: &Document,
    scope_span: Span,
) -> Option<&[Stmt]> {
    if scope_span.start == 0 && scope_span.end == u32::MAX {
        return Some(&document.stmts);
    }
    scope_stmts_by_span_in(&document.stmts, scope_span)
}

fn scope_stmts_by_span_in(stmts: &[Stmt], scope_span: Span) -> Option<&[Stmt]> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if proc.body_span == scope_span {
                    return Some(&proc.body);
                }
                if let Some(s) = scope_stmts_by_span_in(&proc.body, scope_span)
                {
                    return Some(s);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if ns.body_span == scope_span {
                    return Some(&ns.body);
                }
                if let Some(s) = scope_stmts_by_span_in(&ns.body, scope_span) {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_proc_by_body_span(stmts: &[Stmt], body_span: Span) -> Option<&Proc> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if proc.body_span == body_span {
                    return Some(proc);
                }
                if let Some(p) = find_proc_by_body_span(&proc.body, body_span) {
                    return Some(p);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(p) = find_proc_by_body_span(&ns.body, body_span) {
                    return Some(p);
                }
            }
            _ => {}
        }
    }
    None
}

/// Reuse of [`crate::rename`]'s proc-arg identification without
/// borrowing its private helpers.
fn find_proc_arg_at<'a>(
    stmts: &'a [Stmt],
    source: &str,
    offset: u32,
) -> Option<(&'a Proc, String)> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(sig) = &proc.signature {
                    if proc.args_span.contains(offset) {
                        for arg in &sig.args {
                            if arg.name_span.contains(offset) {
                                return Some((proc, arg.name.clone()));
                            }
                            for a in &arg.attributes {
                                for v in &a.values {
                                    if let AttributeValue::Ident {
                                        value,
                                        span,
                                    } = v
                                    {
                                        if span.contains(offset)
                                            && sig
                                                .args
                                                .iter()
                                                .any(|x| &x.name == value)
                                        {
                                            return Some((proc, value.clone()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if proc.body_span.contains(offset) {
                        if let Some(name) =
                            var_ref_name_in(&proc.body, source, offset)
                        {
                            if sig.args.iter().any(|a| a.name == name) {
                                return Some((proc, name));
                            }
                        }
                    }
                }
                if let Some(x) = find_proc_arg_at(&proc.body, source, offset) {
                    return Some(x);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                if let Some(x) = find_proc_arg_at(&ns.body, source, offset) {
                    return Some(x);
                }
            }
            _ => {}
        }
    }
    None
}

fn var_ref_name_in(
    _stmts: &[Stmt],
    source: &str,
    offset: u32,
) -> Option<String> {
    scan_var_ref(source, offset).map(|(n, _)| n)
}

fn var_ref_name_in_scope(
    _stmts: &[Stmt],
    source: &str,
    offset: u32,
) -> Option<String> {
    scan_var_ref(source, offset).map(|(n, _)| n)
}

fn local_decl_name_at(stmts: &[Stmt], offset: u32) -> Option<&str> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let words = &cmd.words;
        let Some(head) = words.first().and_then(|w| w.as_text()) else {
            continue;
        };
        // set / variable / foreach / upvar — same rules as rename.rs.
        match head {
            "set" | "variable" | "foreach" => {
                if let Some(w) = words.get(1) {
                    if w.form == WordForm::Bare && w.span.contains(offset) {
                        if let Some(t) = w.as_text() {
                            return Some(t);
                        }
                    }
                }
            }
            "upvar" => {
                // `upvar [LEVEL] remote local [remote local]…`
                // Skip an optional leading level (`#N` or digits),
                // then walk pairs — cursor on the LOCAL of any
                // pair identifies that local as the target.
                let mut idx = 1;
                if let Some(w) = words.get(idx) {
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
                while idx + 1 < words.len() {
                    let local_word = &words[idx + 1];
                    if local_word.span.contains(offset) {
                        if let Some(t) = local_word.as_text() {
                            return Some(t);
                        }
                    }
                    idx += 2;
                }
            }
            _ => {}
        }
        if let CommandKind::Proc(proc) = &cmd.kind {
            if let Some(n) = local_decl_name_at(&proc.body, offset) {
                return Some(n);
            }
        }
        if is_body_host(head) {
            for word in words.iter().skip(1) {
                if let WordForm::Braced = word.form {
                    if word.span.contains(offset) {
                        // We don't re-parse braced bodies here —
                        // rename.rs's local pass already covers
                        // that via its own reparse. Skip.
                    }
                }
            }
        }
    }
    None
}

fn collect_local_decl_spans(stmts: &[Stmt], name: &str, out: &mut Vec<Span>) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // Skip nested scopes — locals don't cross.
        if matches!(
            &cmd.kind,
            CommandKind::Proc(_) | CommandKind::NamespaceEval(_)
        ) {
            continue;
        }
        let Some(head) = cmd.words.first().and_then(|w| w.as_text()) else {
            continue;
        };
        match head {
            "set" | "variable" | "foreach" => {
                if let Some(w) = cmd.words.get(1) {
                    if w.form == WordForm::Bare && w.as_text() == Some(name) {
                        out.push(w.span);
                    }
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
                    if local_word.as_text() == Some(name) {
                        out.push(local_word.span);
                    }
                    idx += 2;
                }
            }
            _ => {}
        }
        // Body-host commands (if/while/foreach/…) — descend into
        // their braced bodies. They run in the SAME frame, so a
        // `set foo` inside an `if` body is the enclosing scope's
        // local, not a separate scope.
        if is_body_host(head) {
            for word in cmd.words.iter().skip(1) {
                if let Some(inner_stmts) =
                    crate::unused::reparse_braced_body(word, "")
                {
                    collect_local_decl_spans(&inner_stmts, name, out);
                }
            }
        }
    }
}

fn collect_var_ref_spans(
    stmts: &[Stmt],
    source: &str,
    name: &str,
    out: &mut Vec<Span>,
) {
    walk_var_ref_spans(stmts, source, name, out);
}

fn walk_var_ref_spans(
    stmts: &[Stmt],
    source: &str,
    name: &str,
    out: &mut Vec<Span>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        // Skip nested scopes.
        if matches!(
            &cmd.kind,
            CommandKind::Proc(_) | CommandKind::NamespaceEval(_)
        ) {
            continue;
        }
        for word in &cmd.words {
            walk_var_ref_spans_in_word(word, source, name, out);
        }
        // Descend into body-host braced bodies (if/while/foreach/…).
        if let Some(head) = cmd.words.first().and_then(|w| w.as_text()) {
            if is_body_host(head) {
                for word in cmd.words.iter().skip(1) {
                    if let Some(inner_stmts) =
                        crate::unused::reparse_braced_body(word, source)
                    {
                        walk_var_ref_spans(&inner_stmts, source, name, out);
                    }
                }
            }
        }
    }
}

fn walk_var_ref_spans_in_word(
    word: &Word,
    source: &str,
    name: &str,
    out: &mut Vec<Span>,
) {
    for part in &word.parts {
        match part {
            WordPart::VarRef { name: n, span } => {
                if n == name {
                    // Span covers `$name`; the target is the
                    // identifier portion (skip the leading `$`).
                    out.push(Span::new(span.start + 1, span.end));
                }
            }
            WordPart::CmdSubst { body, .. } => {
                walk_var_ref_spans(body, source, name, out);
            }
            WordPart::Text { .. } | WordPart::Escape { .. } => {}
        }
    }
}

fn attribute_ident_ref_spans(sig: &ProcSignature, arg_name: &str) -> Vec<Span> {
    let mut out = Vec::new();
    for arg in &sig.args {
        for a in &arg.attributes {
            for v in &a.values {
                if let AttributeValue::Ident { value, span } = v {
                    if value == arg_name {
                        out.push(*span);
                    }
                }
            }
        }
    }
    out
}

// Sanity: the AST re-exports we need for the pattern matches above.
#[allow(dead_code)]
fn _unused_shape_check(w: &Word, _s: &str, _c: &Command) {
    let _ = w.form;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn parsed(src: &str) -> crate::ast::Document {
        parse(src).document
    }

    fn find_offset(src: &str, needle: &str) -> u32 {
        src.find(needle).unwrap() as u32
    }

    #[test]
    fn identify_proc_on_decl_name() {
        let src = "proc configure_gtm {} { }\n";
        let d = parsed(src);
        let offset = find_offset(src, "configure_gtm");
        let target = identify_at(&d, src, offset).expect("identified");
        assert_eq!(
            target,
            ReferenceTarget::Proc {
                name: "configure_gtm".into()
            }
        );
    }

    #[test]
    fn identify_proc_on_call_site() {
        let src = "proc configure_gtm {} { }\nconfigure_gtm\n";
        let d = parsed(src);
        // Cursor on the CALL of configure_gtm (after the decl).
        let offset = src.rfind("configure_gtm").unwrap() as u32;
        let target = identify_at(&d, src, offset).expect("identified");
        assert_eq!(
            target,
            ReferenceTarget::Proc {
                name: "configure_gtm".into()
            }
        );
    }

    #[test]
    fn find_proc_refs_covers_decl_and_calls() {
        let src = "\
proc configure_gtm {} { }
configure_gtm
proc other {} { configure_gtm }
";
        let d = parsed(src);
        let target = ReferenceTarget::Proc {
            name: "configure_gtm".into(),
        };
        let refs = find_references_in(&d, src, &target);
        // 3 hits: decl name + top-level call + nested call in `other`.
        assert_eq!(refs.len(), 3, "spans: {refs:?}");
    }

    #[test]
    fn identify_type_on_decl_and_annotation() {
        let src = "\
type MyThing = string
proc use_it {v: MyThing} { }
";
        let d = parsed(src);
        // Cursor on the decl name.
        let offset_decl = find_offset(src, "MyThing");
        assert_eq!(
            identify_at(&d, src, offset_decl),
            Some(ReferenceTarget::Type {
                name: "MyThing".into()
            })
        );
        // Cursor on the annotation.
        let offset_ann = src.rfind("MyThing").unwrap() as u32;
        assert_eq!(
            identify_at(&d, src, offset_ann),
            Some(ReferenceTarget::Type {
                name: "MyThing".into()
            })
        );
    }

    #[test]
    fn find_type_refs_covers_decl_and_annotations() {
        let src = "\
type MyThing = string
proc a {v: MyThing} MyThing { }
proc b {v: MyThing} { }
";
        let d = parsed(src);
        let target = ReferenceTarget::Type {
            name: "MyThing".into(),
        };
        let refs = find_references_in(&d, src, &target);
        // decl + a's arg-type + a's return-type + b's arg-type = 4.
        assert_eq!(refs.len(), 4, "spans: {refs:?}");
    }

    #[test]
    fn identify_enum_variant_on_decl_variant() {
        let src = "\
enum Color = {
  Red
  Green
}
";
        let d = parsed(src);
        let offset = find_offset(src, "Red");
        assert_eq!(
            identify_at(&d, src, offset),
            Some(ReferenceTarget::EnumVariant {
                enum_name: "Color".into(),
                variant: "Red".into(),
            })
        );
    }
}
