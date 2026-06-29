// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Codegen for the enum-overload dispatcher.
//!
//! When the validator classifies a set of procs sharing a name as
//! a valid enum-overload (see
//! [`crate::validate::build_signature_table_with_overloads`]), the
//! lowerer rewrites each specialization under its mangled name
//! (`__<public>__<Variant>`) and emits a single public dispatcher
//! proc that switches on the tagged value's variant tag and calls
//! the right specialization with the unwrapped payload.
//!
//! Runtime model:
//!
//! ```tcl
//! proc handle_prop {v args} {
//!   switch -- [lindex $v 0] {
//!     Scalar { return [__handle_prop__Scalar [lindex $v 1] {*}$args] }
//!     Nested { return [__handle_prop__Nested [lindex $v 1] {*}$args] }
//!     default { error "handle_prop: unknown variant '[lindex $v 0]'" }
//!   }
//! }
//! ```
//!
//! The payload is unwrapped before the specialization runs — the
//! body of `proc handle_prop {v: Property::Scalar} ...` sees `$v`
//! as the bare payload (a `string`), matching Haskell `case`
//! semantics.
//!
//! Empty-payload variants still get `[lindex $v 1]` passed through
//! (it's the empty string for a single-element list) — for those
//! the specialization's body shouldn't reference `$v` and the
//! lowering should ideally drop the arg, but for v1 we pass
//! uniformly for simplicity.

use crate::ast::OverloadInfo;

/// Generate the public-name dispatcher proc for an overload set.
///
/// `tail_arg_names` is the list of tail arg names (after the
/// dispatched first arg). They thread through via `{*}$args` so
/// the public signature stays `{v args}` regardless of arity —
/// keeping the dispatcher uniform across overload sets with
/// different tail shapes. Specializations always receive the
/// payload as their first positional arg, then the tail by
/// position.
pub fn emit_dispatcher(info: &OverloadInfo) -> String {
    let mut out = String::new();
    // The dispatcher takes the same kwargs envelope every other
    // proc takes (so calls can pass `-<arg> <enum-value>`). It
    // extracts the dispatched-arg value from the kwargs args list
    // and switches on its tag. Specializations receive the
    // payload via the same kwargs protocol — `-<arg> <payload>`
    // — so their bodies bind `$<arg>` to the unwrapped payload
    // naturally.
    let pub_name = &info.public_name;
    let arg = &info.dispatch_arg_name;
    out.push_str(&format!("proc {pub_name} {{args}} {{\n"));
    // Grab the dispatched arg's value from the kwargs args list.
    // We look for `-<arg> <val>` pairwise; if the user passes
    // positional, well, that's not a supported form for overloaded
    // procs in v1 (kwargs-only).
    out.push_str(&format!(
        "  set __vw_disp \"\"\n  \
         foreach {{__vw_k __vw_v}} $args {{\n    \
           if {{$__vw_k eq \"-{arg}\"}} {{ set __vw_disp $__vw_v; break }}\n  \
         }}\n"
    ));
    out.push_str("  switch -- [lindex $__vw_disp 0] {\n");
    for v in &info.variants {
        // Build a new args list with the dispatched-arg's value
        // replaced by the unwrapped payload, then forward the full
        // args list (including any tail args we pass through for
        // future multi-arg overload support) to the specialization.
        out.push_str(&format!(
            "    {variant} {{\n      \
                set __vw_new [list]\n      \
                foreach {{__vw_k __vw_v}} $args {{\n        \
                  if {{$__vw_k eq \"-{arg}\"}} {{\n          \
                    lappend __vw_new $__vw_k [lindex $__vw_v 1]\n        \
                  }} else {{\n          \
                    lappend __vw_new $__vw_k $__vw_v\n        \
                  }}\n      \
                }}\n      \
                return [{mangled} {{*}}$__vw_new]\n    \
              }}\n",
            variant = v.variant_name,
            mangled = v.mangled_proc_name,
        ));
    }
    out.push_str(&format!(
        "    default {{ error \"{pub_name}: unknown variant '[lindex $__vw_disp 0]'\" }}\n"
    ));
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{OverloadInfo, OverloadVariant};
    use crate::span::Span;
    use crate::validate::mangle_specialization;

    fn info(public: &str, variants: &[&str]) -> OverloadInfo {
        OverloadInfo {
            public_name: public.into(),
            enum_name: "E".into(),
            dispatch_arg_name: "v".into(),
            variants: variants
                .iter()
                .map(|v| OverloadVariant {
                    variant_name: (*v).into(),
                    mangled_proc_name: mangle_specialization(public, v),
                    dispatch_arg_span: Span::new(0, 0),
                })
                .collect(),
            anchor_span: Span::new(0, 0),
        }
    }

    #[test]
    fn dispatcher_two_arms() {
        let i = info("handle_prop", &["Scalar", "Nested"]);
        let d = emit_dispatcher(&i);
        // Dispatcher takes the standard `{args}` kwargs envelope.
        assert!(d.contains("proc handle_prop {args}"));
        // Walks kwargs to find the `-v <enum value>` pair.
        assert!(d.contains("if {$__vw_k eq \"-v\"}"));
        assert!(d.contains("switch -- [lindex $__vw_disp 0]"));
        // Each arm forwards to its mangled specialization with the
        // unwrapped payload spliced back into the kwargs list.
        assert!(d.contains("Scalar {"));
        assert!(d.contains("__handle_prop__Scalar"));
        assert!(d.contains("Nested {"));
        assert!(d.contains("__handle_prop__Nested"));
        assert!(d.contains("default {"));
        assert!(d.contains("unknown variant"));
    }

    #[test]
    fn dispatcher_single_arm() {
        let i = info("only_one", &["Solo"]);
        let d = emit_dispatcher(&i);
        assert!(d.contains("proc only_one {args}"));
        assert!(d.contains("__only_one__Solo"));
    }

    #[test]
    fn dispatcher_includes_default_arm() {
        // Future-proof against runtime corruption / unanticipated
        // tag values. The validator's exhaustiveness check guards
        // the source side; the default arm guards the runtime
        // side.
        let i = info("foo", &["A", "B"]);
        let d = emit_dispatcher(&i);
        assert!(d.contains("default { error"));
    }
}
