// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Compiler-emitted `repr` procs for typed runtime values.
//!
//! The REPL (and other consumers) ask "give me Tcl source that, when
//! applied to a value of type T, produces a display string." That's
//! `dispatch_name(T)`, which always resolves to `<mangle(T)>::repr` in
//! the running Tcl interpreter:
//!
//! - For **primitives** (`string`, `int`, `bool`, `unit`), `<T>::repr`
//!   is shipped once at session start via [`emit_primitive_prelude`].
//! - For **user-declared newtypes** (`bd_cell`, `widget`, …), `<T>::repr`
//!   is the user's own proc — the validator enforces it exists (see
//!   [`crate::validate::build_type_decl_table`]).
//! - For **generics** (`list<T>`, `dict<K, V>`, nested combinations),
//!   [`emit_repr`] monomorphizes a per-instantiation `<mangle>::repr`
//!   that delegates to its element / key / value reprs. Each unique
//!   nested instantiation gets its own proc.
//!
//! All emission goes through [`vw_quote::quote_tcl!`] so word
//! quoting is handled automatically rather than via `format!` string
//! concatenation.
//!
//! Mangling: dot-free, separator `_`. `dict<string,int>` →
//! `dict_string_int`; `list<dict<string,bd_cell>>` →
//! `list_dict_string_bd_cell`. The mangled string is used as the
//! namespace of the emitted proc — `dict_string_int::repr`. This
//! corner-collides only when a user declares `type X` whose name
//! happens to equal a mangled compiler-generated namespace (e.g.
//! `type dict_string_int`); pathological in practice.

use std::collections::{HashMap, HashSet};

use vw_quote::quote_tcl;

use crate::ast::{EnumDecl, EnumVariant, TypeDecl, TypeExpr};

/// Output of [`emit_repr`]: the per-type Tcl procs to ship (in
/// dependency order) and the dispatch name to invoke after they're
/// in scope.
#[derive(Clone, Debug)]
pub struct ReprEmission {
    /// Tcl proc declarations to ship to the worker before any
    /// expression that needs them. Each entry is a complete
    /// `proc <namespace>::repr { v } { … }` source.
    pub procs: Vec<String>,
    /// Fully-qualified Tcl proc to invoke: `<mangle(ty)>::repr`.
    pub dispatch: String,
}

/// Mangled namespace name for `ty`. The compiler-emitted repr proc
/// for this type lives at `<mangle(ty)>::repr`.
pub fn mangle(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named { name, .. } => name.clone(),
        TypeExpr::Generic { name, args, .. } => {
            let mut out = String::with_capacity(name.len() + args.len() * 8);
            out.push_str(name);
            for arg in args {
                out.push('_');
                out.push_str(&mangle(arg));
            }
            out
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => {
            // Qualified types (`Enum::Variant`) are only legal as
            // the dispatch-arg annotation on an overloaded handler
            // — the validator rejects them anywhere else, so
            // codegen should never see one at a value position. If
            // we hit this it's a validator bug.
            panic!(
                "internal error: TypeExpr::Qualified `{namespace}::{variant}` \
                 reached codegen at a value position — validator should have \
                 rejected this"
            );
        }
    }
}

/// Fully-qualified Tcl name of `ty`'s repr proc — what a caller
/// invokes on a value to format it.
pub fn dispatch_name(ty: &TypeExpr) -> String {
    format!("{}::repr", mangle(ty))
}

/// Whether `name` is a primitive type the compiler ships repr for.
/// Anything else is either a user-declared newtype (whose triplet is
/// validated separately) or a generic instantiation (whose repr is
/// emitted by [`emit_repr`]).
pub fn is_primitive(name: &str) -> bool {
    matches!(name, "string" | "int" | "bool" | "unit")
}

/// Emit the primitive prelude — Tcl source for the
/// `string` / `int` / `bool` / `unit` triplets (`repr` + `from` +
/// `to`). Shipped once at session start so every typed expression
/// downstream can rely on the primitives being defined.
///
/// Each type's procs are wrapped in an explicit `namespace eval`
/// block. `string` is a Tcl built-in command, so the otherwise-
/// implicit `proc string::repr` namespace-creation hits a
/// "unknown namespace" error from the interpreter; wrapping in
/// `namespace eval string {...}` sidesteps that (we're operating
/// on the namespace as a Tcl namespace, not as a command class).
/// The same wrapping is applied uniformly to `int` / `bool` /
/// `unit` for consistency and so a future Tcl that promotes
/// `bool` or `int` to a built-in doesn't silently break us.
///
/// `from` / `to` for primitives are identity (or coerce to the
/// canonical representation, e.g. `expr {int(...)}` for `int`).
pub fn emit_primitive_prelude() -> Vec<String> {
    // Compiler-emitted reprs share the same kwargs envelope as
    // user-written newtype reprs (`proc <T>::repr {v: T} string
    // { … }` lowers to `proc repr {args} { ::vw::kwargs $args
    // {v ""}; … }`). The dispatch site (see
    // `vw-repl::lower::wrap_with_repr`) always calls them with
    // `-v <val>` so the kwargs envelope binds `$v` uniformly.
    // Without this uniformity, user-written reprs (which can't
    // avoid the kwargs wrap) would error on positional calls.
    vec![
        // string: identity at every slot.
        quote_tcl!(
            "namespace eval string {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return $v }\n\
            }\n"
        ),
        // int: format / coerce.
        quote_tcl!(
            "namespace eval int {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return [format %d $v] }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n\
            }\n"
        ),
        // bool: textual form; 0/1 round-trip for from/to.
        quote_tcl!(
            "namespace eval bool {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? \"true\" : \"false\"}] }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n\
            }\n"
        ),
        // unit: empty value. The App suppresses on the *type*, not
        // on the value — these procs exist so generics over `unit`
        // still type-check, even though they're unusual.
        quote_tcl!(
            "namespace eval unit {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n\
            }\n"
        ),
    ]
}

/// Walk `ty` depth-first, emitting one `<mangle>::repr` proc per
/// unique generic instantiation along the way. Plain [`Named`] types
/// (primitives or user newtypes) don't get codegen here — their
/// reprs come from [`emit_primitive_prelude`] or from the user's
/// own `<name>::repr` proc (validator-enforced).
///
/// The returned dispatch name is `<mangle(ty)>::repr` — the caller
/// invokes it on a value of type `ty` to get the display string.
pub fn emit_repr(ty: &TypeExpr) -> ReprEmission {
    emit_repr_with_types(ty, &HashMap::new())
}

/// Same as [`emit_repr`] but also walks user-declared newtypes
/// (`type T = U`) — when the dispatch type is a newtype whose
/// underlying is a generic, the generic's repr needs to be in
/// scope so the user's `proc T::repr` body can call it.
///
/// Without this recursion, `Properties::repr` (which delegates to
/// `dict_string_Property::repr`) errors at runtime with
/// `invalid command name "dict_string_Property::repr"` because
/// the monomorphized generic was never emitted.
pub fn emit_repr_with_types(
    ty: &TypeExpr,
    types: &HashMap<String, &TypeDecl>,
) -> ReprEmission {
    let mut procs = Vec::new();
    let mut seen = HashSet::new();
    emit_recursive(ty, &mut procs, &mut seen, types);
    ReprEmission {
        procs,
        dispatch: dispatch_name(ty),
    }
}

/// Emit the auto-generated `namespace eval <Enum> { … }` prelude
/// for an enum declaration. Contains:
///
/// - **Constructors** — one per variant. Payload variants take a
///   `v` arg and return `[list <Variant> $v]`; empty-payload
///   variants take no args and return `[list <Variant>]`.
/// - **`tag` / `payload`** — explicit unwrap accessors wrappers
///   use to bridge enum values into bare-Tcl `extern::` calls.
/// - **`repr`** — switches on `[lindex $v 0]`, calls each variant
///   payload type's `repr` and wraps as `<Variant>(<inner>)` for
///   payload variants, bare `<Variant>` for empty ones.
/// - **`from` / `to`** — identity (enum values are already in their
///   canonical tagged-tuple form; the triplet exists so generics
///   over enums type-check uniformly with newtypes).
///
/// The block is wrapped in `namespace eval` — Tcl auto-creates
/// the namespace on `proc <ns>::<name>` ONLY when nothing else
/// claims the name. For defensiveness (and so users can pick
/// enum names that happen to match a Tcl built-in's namespace
/// later without a confusing failure mode), we use the explicit
/// form, mirroring the primitive prelude.
pub fn emit_enum_prelude(enum_decl: &EnumDecl) -> String {
    let Some(name) = enum_decl.name.as_deref() else {
        // Anonymous enum — shouldn't happen post-parser, but
        // bail rather than emit junk.
        return String::new();
    };
    let mut body = String::new();
    body.push_str(&format!("namespace eval {name} {{\n"));
    // Constructors — plain positional Tcl, called by user code as
    // `Property::Scalar foo` (positional). NOT through the kwargs
    // envelope.
    for v in &enum_decl.variants {
        emit_constructor(&mut body, &v.name, v.payload.is_some());
    }
    // tag / payload — also positional; called by wrappers in
    // `extern::` bridging code as `Property::payload $v`.
    body.push_str("  proc tag {v} { return [lindex $v 0] }\n");
    body.push_str("  proc payload {v} { return [lindex $v 1] }\n");
    // repr / from / to — kwargs envelope so they're callable
    // uniformly with all other reprs (the dispatch site emits
    // `-v <val>` form universally; see
    // `vw-repl::lower::wrap_with_repr`).
    body.push_str("  proc repr {args} {\n");
    body.push_str("    ::vw::kwargs $args {v \"\"}\n");
    body.push_str("    switch -- [lindex $v 0] {\n");
    for v in &enum_decl.variants {
        emit_repr_arm(&mut body, v);
    }
    body.push_str("      default { return \"<unknown variant>\" }\n");
    body.push_str("    }\n");
    body.push_str("  }\n");
    // from / to are identity for enums (the constructors are the
    // user-facing lift).
    body.push_str(
        "  proc from {args} { ::vw::kwargs $args {v \"\"}; return $v }\n",
    );
    body.push_str(
        "  proc to {args} { ::vw::kwargs $args {v \"\"}; return $v }\n",
    );
    body.push_str("}\n");
    body
}

fn emit_constructor(out: &mut String, variant: &str, has_payload: bool) {
    if has_payload {
        out.push_str(&format!(
            "  proc {variant} {{v}} {{ return [list {variant} $v] }}\n"
        ));
    } else {
        out.push_str(&format!(
            "  proc {variant} {{}} {{ return [list {variant}] }}\n"
        ));
    }
}

fn emit_repr_arm(out: &mut String, v: &EnumVariant) {
    let variant = &v.name;
    match &v.payload {
        None => {
            // Empty-payload: just the bare variant name.
            out.push_str(&format!(
                "      {variant} {{ return \"{variant}\" }}\n"
            ));
        }
        Some(payload_ty) => {
            // Payload variant: `<Variant>(<inner>)`. When the
            // inner repr is multi-line (nested dicts / lists /
            // enums), indent each continuation line by two
            // spaces so the structure visually nests under the
            // variant name. Single-line inners stay compact.
            //
            // We use an intermediate `set __vw_inner ...` rather
            // than inlining the `string map` inside a quoted
            // string — embedding a Tcl `[list "\n" "\n  "]`
            // inside `"..."` requires escaping the inner `"`s
            // and reasoning about whether the outer quote
            // context bleeds into the command substitution.
            // Splitting into two statements sidesteps that
            // entirely.
            //
            // `\n` (bare word) and `"\n  "` (quoted) both
            // backslash-substitute to a newline character; the
            // bare form keeps the source readable.
            let dispatch = dispatch_name(payload_ty);
            out.push_str(&format!(
                "      {variant} {{\n        \
                    set __vw_inner [string map [list \\n \"\\n  \"] [{dispatch} -v [lindex $v 1]]]\n        \
                    return \"{variant}($__vw_inner)\"\n      \
                }}\n"
            ));
        }
    }
}

fn emit_recursive(
    ty: &TypeExpr,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
    types: &HashMap<String, &TypeDecl>,
) {
    match ty {
        TypeExpr::Named { name, .. } => {
            // No codegen for plain names directly — `<name>::repr`
            // is either a primitive (shipped via
            // `emit_primitive_prelude`) or a user newtype
            // (validator-enforced to exist). BUT if `name`
            // resolves to a user newtype whose underlying is a
            // generic, we have to recurse so the underlying's
            // monomorphized repr is shipped — the user's
            // `proc <name>::repr` body typically delegates to it.
            if let Some(decl) = types.get(name.as_str()) {
                if let Some(underlying) = decl.underlying.as_ref() {
                    emit_recursive(underlying, out, seen, types);
                }
            }
        }
        TypeExpr::Generic { name, args, .. } => {
            // Depth-first: emit each arg's repr first so this
            // proc's body can call them.
            for a in args {
                emit_recursive(a, out, seen, types);
            }
            let m = mangle(ty);
            if !seen.insert(m.clone()) {
                return; // Already emitted this instantiation.
            }
            let body = match name.as_str() {
                "dict" if args.len() == 2 => {
                    emit_dict_repr(&m, &args[0], &args[1])
                }
                "list" if args.len() == 1 => emit_list_repr(&m, &args[0]),
                _ => emit_unknown_generic_repr(&m),
            };
            out.push(body);
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => {
            // Mirror of `mangle`'s guard — Qualified types must
            // not reach codegen at a value position.
            panic!(
                "internal error: TypeExpr::Qualified `{namespace}::{variant}` \
                 reached emit_recursive — validator should have rejected this"
            );
        }
    }
}

/// `dict<K, V>::repr` — iterate pairs, format each as
/// `<K::repr key> <V::repr val>` joined with newlines.
///
/// The body uses braced `expr {…}` and avoids interpolating the
/// dispatch names raw via `quote_tcl!` because Tcl's word quoting
/// would brace the `::` separators (those are bare-safe but the
/// macro's `Word::lit` doesn't know that). The proc names go in via
/// raw substitution at template time instead — they're already
/// valid Tcl, and the macro template's literal regions pass through
/// untouched.
fn emit_dict_repr(mangled: &str, k: &TypeExpr, v: &TypeExpr) -> String {
    let key_repr = dispatch_name(k);
    let val_repr = dispatch_name(v);
    // Uses the same kwargs envelope as `emit_primitive_prelude`
    // so the dispatch site can uniformly call all reprs with
    // `-v <val>`. Sub-element reprs are invoked through the
    // same `-v` convention.
    format!(
        "namespace eval {ns} {{\n  \
            proc repr {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out \"\"\n    \
                set first 1\n    \
                foreach {{k val}} $v {{\n      \
                    if {{!$first}} {{ append out \"\\n\" }}\n      \
                    set first 0\n      \
                    set __vw_kr [string map [list \\n \"\\n  \"] [{kr} -v $k]]\n      \
                    set __vw_vr [string map [list \\n \"\\n  \"] [{vr} -v $val]]\n      \
                    append out $__vw_kr \" \" $__vw_vr\n    \
                }}\n    \
                return $out\n  \
            }}\n\
        }}\n",
        ns = mangled,
        kr = key_repr,
        vr = val_repr,
    )
}

/// `list<T>::repr` — iterate elements, format each via `T::repr`,
/// join with newlines.
fn emit_list_repr(mangled: &str, elem: &TypeExpr) -> String {
    let elem_repr = dispatch_name(elem);
    format!(
        "namespace eval {ns} {{\n  \
            proc repr {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out \"\"\n    \
                set first 1\n    \
                foreach item $v {{\n      \
                    if {{!$first}} {{ append out \"\\n\" }}\n      \
                    set first 0\n      \
                    set __vw_er [string map [list \\n \"\\n  \"] [{er} -v $item]]\n      \
                    append out $__vw_er\n    \
                }}\n    \
                return $out\n  \
            }}\n\
        }}\n",
        ns = mangled,
        er = elem_repr,
    )
}

/// Fallback for generic shapes we don't have a specialized shell
/// for (e.g. a hypothetical `tuple<…>` we haven't designed yet).
/// Renders the raw Tcl value — at least the user sees *something*
/// instead of an "unknown generic" error.
fn emit_unknown_generic_repr(mangled: &str) -> String {
    format!(
        "namespace eval {mangled} {{ \
            proc repr {{args}} {{ ::vw::kwargs $args {{v \"\"}}; return $v }} \
        }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    fn named(name: &str) -> TypeExpr {
        TypeExpr::Named {
            name: name.into(),
            span: Span::new(0, 0),
        }
    }

    fn generic(name: &str, args: Vec<TypeExpr>) -> TypeExpr {
        TypeExpr::Generic {
            name: name.into(),
            name_span: Span::new(0, 0),
            args,
            span: Span::new(0, 0),
        }
    }

    #[test]
    fn mangle_primitives() {
        assert_eq!(mangle(&named("string")), "string");
        assert_eq!(mangle(&named("bd_cell")), "bd_cell");
        assert_eq!(mangle(&named("unit")), "unit");
    }

    #[test]
    fn mangle_dict_two_args() {
        let ty = generic("dict", vec![named("string"), named("int")]);
        assert_eq!(mangle(&ty), "dict_string_int");
    }

    #[test]
    fn mangle_list_one_arg() {
        let ty = generic("list", vec![named("bd_cell")]);
        assert_eq!(mangle(&ty), "list_bd_cell");
    }

    #[test]
    fn mangle_nested() {
        let inner = generic("dict", vec![named("string"), named("bd_cell")]);
        let outer = generic("list", vec![inner]);
        assert_eq!(mangle(&outer), "list_dict_string_bd_cell");
    }

    #[test]
    fn dispatch_for_primitive_uses_name() {
        assert_eq!(dispatch_name(&named("string")), "string::repr");
        assert_eq!(dispatch_name(&named("bd_cell")), "bd_cell::repr");
    }

    #[test]
    fn dispatch_for_generic_uses_mangled() {
        let ty = generic("dict", vec![named("string"), named("string")]);
        assert_eq!(dispatch_name(&ty), "dict_string_string::repr");
    }

    #[test]
    fn primitive_prelude_emits_one_namespace_block_per_type() {
        let procs = emit_primitive_prelude();
        // 4 types, each emitted as a single `namespace eval`
        // block that internally defines repr/from/to.
        assert_eq!(procs.len(), 4);
        assert!(procs.iter().any(|p| p.contains("namespace eval string")));
        assert!(procs.iter().any(|p| p.contains("namespace eval int")));
        assert!(procs.iter().any(|p| p.contains("namespace eval bool")));
        assert!(procs.iter().any(|p| p.contains("namespace eval unit")));
        // Each block contains the full triplet (repr + from + to)
        // and uses `return` in every body.
        for p in &procs {
            assert!(p.contains("proc repr"), "missing repr in: {p}");
            assert!(p.contains("proc from"), "missing from in: {p}");
            assert!(p.contains("proc to"), "missing to in: {p}");
        }
    }

    #[test]
    fn emit_repr_named_emits_no_procs() {
        // Primitives and user newtypes don't need codegen — repr
        // lives in the primitive prelude or the user's own proc.
        let e = emit_repr(&named("string"));
        assert!(e.procs.is_empty());
        assert_eq!(e.dispatch, "string::repr");

        let e = emit_repr(&named("bd_cell"));
        assert!(e.procs.is_empty());
        assert_eq!(e.dispatch, "bd_cell::repr");
    }

    #[test]
    fn emit_repr_dict_string_string() {
        let ty = generic("dict", vec![named("string"), named("string")]);
        let e = emit_repr(&ty);
        assert_eq!(e.dispatch, "dict_string_string::repr");
        assert_eq!(e.procs.len(), 1);
        let body = &e.procs[0];
        // The proc is defined inside its `namespace eval`, so the
        // textual proc name is just `repr` — the namespace is in
        // the surrounding `namespace eval dict_string_string`.
        assert!(body.contains("namespace eval dict_string_string"));
        assert!(body.contains("proc repr {args}"));
        assert!(body.contains("::vw::kwargs $args"));
        assert!(body.contains("foreach {k val} $v"));
        // Element reprs called by their fully-qualified name via
        // the universal `-v <val>` kwargs form.
        assert!(body.contains("[string::repr -v $k]"));
        assert!(body.contains("[string::repr -v $val]"));
    }

    #[test]
    fn emit_repr_list_bd_cell() {
        let ty = generic("list", vec![named("bd_cell")]);
        let e = emit_repr(&ty);
        assert_eq!(e.dispatch, "list_bd_cell::repr");
        assert_eq!(e.procs.len(), 1);
        let body = &e.procs[0];
        assert!(body.contains("namespace eval list_bd_cell"));
        assert!(body.contains("proc repr {args}"));
        assert!(body.contains("[bd_cell::repr -v $item]"));
    }

    #[test]
    fn emit_repr_nested_topologically_orders_sub_procs() {
        // dict<string, list<int>>: emits list<int>::repr first,
        // then dict_string_list_int::repr.
        let inner = generic("list", vec![named("int")]);
        let outer = generic("dict", vec![named("string"), inner]);
        let e = emit_repr(&outer);
        assert_eq!(e.dispatch, "dict_string_list_int::repr");
        assert_eq!(e.procs.len(), 2);
        // First proc emitted is the inner list, second is the
        // outer dict.
        assert!(e.procs[0].contains("namespace eval list_int"));
        assert!(e.procs[1].contains("namespace eval dict_string_list_int"));
        // Outer body calls the inner by its fully-qualified name.
        assert!(e.procs[1].contains("[list_int::repr"));
    }

    #[test]
    fn emit_repr_dedups_repeated_subtypes() {
        // dict<bd_cell, bd_cell> — bd_cell is a leaf (Named), so no
        // codegen for it, but if we had dict<list<int>, list<int>>
        // we'd want list_int::repr emitted only ONCE.
        let inner = generic("list", vec![named("int")]);
        let outer = generic("dict", vec![inner.clone(), inner]);
        let e = emit_repr(&outer);
        // list_int's namespace block appears once even though it's
        // referenced twice in the outer dict.
        let list_int_count = e
            .procs
            .iter()
            .filter(|p| p.contains("namespace eval list_int "))
            .count();
        assert_eq!(list_int_count, 1);
    }

    #[test]
    fn emit_repr_unknown_generic_falls_back_to_identity() {
        let ty = generic("tuple", vec![named("string"), named("int")]);
        let e = emit_repr(&ty);
        assert_eq!(e.procs.len(), 1);
        assert!(
            e.procs[0].contains("return $v"),
            "expected identity body, got {:?}",
            e.procs[0]
        );
    }

    #[test]
    fn is_primitive_table() {
        assert!(is_primitive("string"));
        assert!(is_primitive("int"));
        assert!(is_primitive("bool"));
        assert!(is_primitive("unit"));
        assert!(!is_primitive("bd_cell"));
        assert!(!is_primitive("widget"));
        assert!(!is_primitive("dict"));
    }

    // --- enum prelude emission --------------------------------------

    fn ed_with_variants(
        name: &str,
        vs: Vec<(&str, Option<TypeExpr>)>,
    ) -> EnumDecl {
        EnumDecl {
            name: Some(name.into()),
            name_span: Span::new(0, 0),
            variants: vs
                .into_iter()
                .map(|(n, p)| EnumVariant {
                    name: n.into(),
                    name_span: Span::new(0, 0),
                    payload: p,
                    payload_span: Span::new(0, 0),
                    span: Span::new(0, 0),
                })
                .collect(),
            body_span: Span::new(0, 0),
        }
    }

    #[test]
    fn enum_prelude_with_payload_variants() {
        let ed = ed_with_variants(
            "Property",
            vec![
                ("Scalar", Some(named("string"))),
                (
                    "Nested",
                    Some(generic(
                        "dict",
                        vec![named("string"), named("string")],
                    )),
                ),
            ],
        );
        let p = emit_enum_prelude(&ed);
        // Wrapped in namespace eval.
        assert!(p.contains("namespace eval Property"));
        // Constructors with payload.
        assert!(p.contains("proc Scalar {v} { return [list Scalar $v] }"));
        assert!(p.contains("proc Nested {v} { return [list Nested $v] }"));
        // Accessors.
        assert!(p.contains("proc tag {v}"));
        assert!(p.contains("proc payload {v}"));
        // Repr switch — kwargs envelope around the body.
        assert!(p.contains("proc repr {args}"));
        assert!(p.contains("::vw::kwargs $args"));
        assert!(p.contains("switch -- [lindex $v 0]"));
        // Each variant's body now uses an intermediate
        // `__vw_inner` after applying the continuation-indent
        // `string map` transform.
        assert!(p.contains("Scalar($__vw_inner)"));
        assert!(p.contains("Nested($__vw_inner)"));
        // Payload reprs dispatched via mangled names with `-v`.
        assert!(p.contains("string::repr -v"));
        assert!(p.contains("dict_string_string::repr -v"));
        // Identity from/to — also kwargs envelope.
        assert!(p.contains("proc from {args}"));
        assert!(p.contains("proc to {args}"));
    }

    #[test]
    fn enum_prelude_with_empty_payload_variants() {
        let ed = ed_with_variants(
            "Direction",
            vec![
                ("North", None),
                ("South", None),
                ("East", None),
                ("West", None),
            ],
        );
        let p = emit_enum_prelude(&ed);
        // Empty-payload constructors take no args.
        assert!(p.contains("proc North {} { return [list North] }"));
        assert!(p.contains("proc West {} { return [list West] }"));
        // Repr arms render bare variant name (no parens).
        assert!(p.contains("North { return \"North\" }"));
        assert!(p.contains("West { return \"West\" }"));
        // No `(` after variant names in the repr arms.
        let arm = "North { return \"North(";
        assert!(!p.contains(arm), "shouldn't have parens for empty variants");
    }

    #[test]
    fn enum_prelude_mixed_payload_and_empty() {
        let ed = ed_with_variants(
            "Maybe",
            vec![("Some", Some(named("int"))), ("None", None)],
        );
        let p = emit_enum_prelude(&ed);
        assert!(p.contains("proc Some {v} { return [list Some $v] }"));
        assert!(p.contains("proc None {} { return [list None] }"));
        // Payload arm uses `__vw_inner` after the
        // continuation-indent `string map` transform.
        assert!(p.contains("int::repr -v"));
        assert!(p.contains("Some($__vw_inner)"));
        assert!(p.contains("None { return \"None\" }"));
    }
}
