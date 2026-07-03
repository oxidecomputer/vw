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
            // Qualified names that reached codegen at a value
            // position resolve to a namespaced newtype — the
            // validator has already verified the name refers to a
            // declared `type ns::T = …`. Enum-variant Qualifieds
            // never flow here (they're only legal as the dispatch
            // first-arg annotation, which mangling doesn't touch).
            // Mangle by joining with `::` so the resulting Tcl
            // proc name matches the newtype's declared namespace.
            format!("{namespace}::{variant}")
        }
    }
}

/// Fully-qualified Tcl name of `ty`'s repr proc — what a caller
/// invokes on a value to format it.
pub fn dispatch_name(ty: &TypeExpr) -> String {
    format!("{}::repr", mangle(ty))
}

/// Fully-qualified Tcl name of `ty`'s `to_raw` proc — the
/// boundary-lowering helper that flattens a typed htcl value
/// down to the bare-Tcl form Vivado consumes through `extern::`.
/// Used by [`emit_to_raw_arm`] and by wrappers that explicitly
/// invoke a type's lowering on a typed arg before forwarding to
/// `extern::`.
pub fn to_raw_dispatch_name(ty: &TypeExpr) -> String {
    format!("{}::to_raw", mangle(ty))
}

/// Fully-qualified Tcl name of `ty`'s `from_raw` proc — the
/// boundary-lifting helper that wraps a raw extern-returned
/// value into the typed form htcl downstream consumes.
pub fn from_raw_dispatch_name(ty: &TypeExpr) -> String {
    format!("{}::from_raw", mangle(ty))
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
        // string: identity at every slot — including to_raw / from_raw,
        // since the Tcl runtime representation of a string IS the raw
        // value the extern boundary expects.
        quote_tcl!(
            "namespace eval string {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc to_raw {args} { ::vw::kwargs $args {v \"\"}; return $v }\n  \
                proc from_raw {args} { ::vw::kwargs $args {v \"\"}; return $v }\n\
            }\n"
        ),
        // int: format / coerce. to_raw / from_raw mirror to / from
        // since Vivado consumes integer-shaped strings.
        quote_tcl!(
            "namespace eval int {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return [format %d $v] }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n  \
                proc to_raw {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n  \
                proc from_raw {args} { ::vw::kwargs $args {v \"\"}; return [expr {int($v)}] }\n\
            }\n"
        ),
        // bool: textual form; 0/1 round-trip for from/to. to_raw /
        // from_raw use the same 0/1 form Vivado expects.
        quote_tcl!(
            "namespace eval bool {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? \"true\" : \"false\"}] }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n  \
                proc to_raw {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n  \
                proc from_raw {args} { ::vw::kwargs $args {v \"\"}; return [expr {$v ? 1 : 0}] }\n\
            }\n"
        ),
        // unit: empty value. The App suppresses on the *type*, not
        // on the value — these procs exist so generics over `unit`
        // still type-check, even though they're unusual.
        quote_tcl!(
            "namespace eval unit {\n  \
                proc repr {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc from {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc to {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc to_raw {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n  \
                proc from_raw {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n\
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
    // to_raw: lower the tagged enum value to its raw extern-side
    // representation. Switch on variant tag; for payload variants
    // recurse via the payload type's to_raw; for empty variants
    // emit the variant name (the convention extern Vivado calls
    // recognize for tag-style values). See [docs/htcl-extern-boundary.md]
    // for the rationale on this being mechanical / compiler-emitted.
    body.push_str("  proc to_raw {args} {\n");
    body.push_str("    ::vw::kwargs $args {v \"\"}\n");
    body.push_str("    switch -- [lindex $v 0] {\n");
    for v in &enum_decl.variants {
        emit_to_raw_arm(&mut body, v);
    }
    body.push_str(
        "      default { error \"unknown variant: [lindex $v 0]\" }\n",
    );
    body.push_str("    }\n");
    body.push_str("  }\n");
    // from_raw: default lift wraps the raw value as the FIRST
    // variant. For sum types where the right variant depends on
    // the value's shape (e.g. Property — Scalar vs Nested
    // chosen by structural inference), users override via
    // `proc <Enum>::from_raw` AFTER the compiler-emitted prelude;
    // Tcl's last-`proc`-wins lets the user override take
    // precedence.
    if let Some(first) = enum_decl.variants.first() {
        emit_from_raw_default(&mut body, first);
    } else {
        body.push_str(
            "  proc from_raw {args} { ::vw::kwargs $args {v \"\"}; return \"\" }\n",
        );
    }
    body.push_str("}\n");
    body
}

/// Emit one arm of `<Enum>::to_raw`'s `switch -- [lindex $v 0]`
/// body: for payload variants, recurse via the payload type's
/// `to_raw`; for empty variants, emit the variant name as the
/// raw value (matches how extern Vivado callers receive bare
/// enum-style tags).
fn emit_to_raw_arm(out: &mut String, v: &EnumVariant) {
    let variant = &v.name;
    match &v.payload {
        None => {
            out.push_str(&format!(
                "      {variant} {{ return \"{variant}\" }}\n"
            ));
        }
        Some(payload_ty) => {
            let dispatch = to_raw_dispatch_name(payload_ty);
            out.push_str(&format!(
                "      {variant} {{ return [{dispatch} -v [lindex $v 1]] }}\n"
            ));
        }
    }
}

/// Default `<Enum>::from_raw` body — wrap input as the first
/// variant. For payload variants, the input flows through the
/// payload type's `from_raw` first. For empty variants, the
/// input is ignored and we return the bare-variant constructor.
fn emit_from_raw_default(out: &mut String, first: &EnumVariant) {
    let variant = &first.name;
    match &first.payload {
        None => {
            out.push_str(&format!(
                "  proc from_raw {{args}} {{\n    \
                    ::vw::kwargs $args {{v \"\"}}\n    \
                    return [list {variant}]\n  \
                }}\n",
            ));
        }
        Some(payload_ty) => {
            let dispatch = from_raw_dispatch_name(payload_ty);
            out.push_str(&format!(
                "  proc from_raw {{args}} {{\n    \
                    ::vw::kwargs $args {{v \"\"}}\n    \
                    return [list {variant} [{dispatch} -v $v]]\n  \
                }}\n",
            ));
        }
    }
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
            // Payload variant: `<Variant>(<inner>)`. Formatting
            // depends on whether the inner repr fits on one line:
            //
            //   single-line:  `Variant(inner)`
            //   multi-line:   `Variant(\n  line1\n  line2\n)`
            //
            // The multi-line shape (opening paren followed by
            // newline + 2-space indent for the first child,
            // closing paren on its own line, every inner line
            // indented one extra level) keeps deeply-nested
            // values readable instead of arrowing off the right
            // margin.
            //
            // 2-space indent applies to ALL inner lines
            // (including their pre-existing continuation indents),
            // so each nesting level adds exactly 2 spaces of
            // indent uniformly.
            let dispatch = dispatch_name(payload_ty);
            out.push_str(&format!(
                "      {variant} {{\n        \
                    set __vw_inner [{dispatch} -v [lindex $v 1]]\n        \
                    if {{[string first \"\\n\" $__vw_inner] >= 0}} {{\n          \
                        set __vw_indented [string map [list \\n \"\\n  \"] $__vw_inner]\n          \
                        return \"{variant}(\\n  $__vw_indented\\n)\"\n        \
                    }} else {{\n          \
                        return \"{variant}($__vw_inner)\"\n        \
                    }}\n      \
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
            // Namespaced newtype reference (`dcmac::GtChProps` and
            // friends). Look up by the joined qualified name — the
            // types table is keyed exactly that way (see
            // `validate::build_type_decl_table`). If found and its
            // underlying is a generic, recurse so the generic's
            // monomorphized repr ships alongside.
            let qualified = format!("{namespace}::{variant}");
            if let Some(decl) = types.get(qualified.as_str()) {
                if let Some(underlying) = decl.underlying.as_ref() {
                    emit_recursive(underlying, out, seen, types);
                }
            }
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
    let key_to_raw = to_raw_dispatch_name(k);
    let val_to_raw = to_raw_dispatch_name(v);
    let key_from_raw = from_raw_dispatch_name(k);
    let val_from_raw = from_raw_dispatch_name(v);
    // Uses the same kwargs envelope as `emit_primitive_prelude`
    // so the dispatch site can uniformly call all reprs with
    // `-v <val>`. Sub-element reprs are invoked through the
    // same `-v` convention.
    //
    // to_raw / from_raw are emitted in the SAME namespace so
    // callers can dispatch via `<mangle>::{repr,to_raw,from_raw}`
    // uniformly. to_raw walks the dict, applying K::to_raw and
    // V::to_raw element-wise and rebuilding as a flat paired
    // list (the shape Vivado consumes). from_raw is the inverse
    // — walks a paired list, applies K::from_raw / V::from_raw
    // element-wise, builds a typed dict.
    format!(
        "namespace eval {ns} {{\n  \
            proc repr {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out \"\"\n    \
                set first 1\n    \
                foreach {{k val}} $v {{\n      \
                    if {{!$first}} {{ append out \"\\n\" }}\n      \
                    set first 0\n      \
                    append out [{kr} -v $k] \" \" [{vr} -v $val]\n    \
                }}\n    \
                return $out\n  \
            }}\n  \
            proc to_raw {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out [list]\n    \
                foreach {{k val}} $v {{\n      \
                    lappend out [{ktr} -v $k] [{vtr} -v $val]\n    \
                }}\n    \
                return $out\n  \
            }}\n  \
            proc from_raw {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out [dict create]\n    \
                foreach {{k val}} $v {{\n      \
                    dict set out [{kfr} -v $k] [{vfr} -v $val]\n    \
                }}\n    \
                return $out\n  \
            }}\n\
        }}\n",
        ns = mangled,
        kr = key_repr,
        vr = val_repr,
        ktr = key_to_raw,
        vtr = val_to_raw,
        kfr = key_from_raw,
        vfr = val_from_raw,
    )
}

/// `list<T>::repr` — iterate elements, format each via `T::repr`,
/// join with newlines. Also emits `to_raw` / `from_raw` element-
/// wise dispatching through `T::to_raw` / `T::from_raw`.
fn emit_list_repr(mangled: &str, elem: &TypeExpr) -> String {
    let elem_repr = dispatch_name(elem);
    let elem_to_raw = to_raw_dispatch_name(elem);
    let elem_from_raw = from_raw_dispatch_name(elem);
    format!(
        "namespace eval {ns} {{\n  \
            proc repr {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out \"\"\n    \
                set first 1\n    \
                foreach item $v {{\n      \
                    if {{!$first}} {{ append out \"\\n\" }}\n      \
                    set first 0\n      \
                    append out [{er} -v $item]\n    \
                }}\n    \
                return $out\n  \
            }}\n  \
            proc to_raw {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out [list]\n    \
                foreach item $v {{\n      \
                    lappend out [{etr} -v $item]\n    \
                }}\n    \
                return $out\n  \
            }}\n  \
            proc from_raw {{args}} {{\n    \
                ::vw::kwargs $args {{v \"\"}}\n    \
                set out [list]\n    \
                foreach item $v {{\n      \
                    lappend out [{efr} -v $item]\n    \
                }}\n    \
                return $out\n  \
            }}\n\
        }}\n",
        ns = mangled,
        er = elem_repr,
        etr = elem_to_raw,
        efr = elem_from_raw,
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
            proc to_raw {{args}} {{ ::vw::kwargs $args {{v \"\"}}; return $v }} \
            proc from_raw {{args}} {{ ::vw::kwargs $args {{v \"\"}}; return $v }} \
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
