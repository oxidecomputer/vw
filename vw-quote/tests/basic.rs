// Integration tests for `quote_htcl!` and `quote_tcl!`. A proc-macro
// crate can't use its own macros from within `src/`, so these tests
// live in `tests/` and pull `vw-quote` and `vw-htcl` as dev-deps.

use vw_quote::{quote_htcl, quote_tcl};

#[test]
fn literal_passthrough() {
    let s = quote_htcl!("puts hi\n");
    assert_eq!(s, "puts hi\n");
}

#[test]
fn simple_ident_interpolation() {
    let name = "greet";
    let s = quote_htcl!("proc #(name) {} { puts hi }\n");
    assert_eq!(s, "proc greet {} { puts hi }\n");
}

#[test]
fn expression_interpolation() {
    let width = 16u32;
    let s = quote_htcl!("set w #(width)\n");
    assert_eq!(s, "set w 16\n");
}

#[test]
fn values_needing_quoting_get_quoted() {
    let msg = "hello world";
    let s = quote_htcl!("puts #(msg)\n");
    assert_eq!(s, "puts \"hello world\"\n");
}

#[test]
fn dollar_in_value_is_escaped() {
    let s = "$x";
    let out = quote_htcl!("puts #(s)\n");
    // The value `$x` has special chars, so it gets quoted with
    // `\$` escaped — preserving it as the literal text "$x" at runtime.
    assert_eq!(out, "puts \"\\$x\"\n");
}

#[test]
fn braces_in_template_pass_through() {
    let name = "f";
    let s = quote_htcl!("proc #(name) {} {\n  puts hi\n}\n");
    assert_eq!(s, "proc f {} {\n  puts hi\n}\n");
}

#[test]
fn doc_comment_passes_through() {
    let s = quote_htcl!("## A doc comment.\nputs hi\n");
    assert_eq!(s, "## A doc comment.\nputs hi\n");
}

#[test]
fn multiple_interpolations() {
    let name = "greet";
    let arg = "world";
    let s = quote_htcl!("proc #(name) { #(arg) } { puts hi }\n");
    assert_eq!(s, "proc greet { world } { puts hi }\n");
}

#[test]
fn method_call_in_interpolation() {
    struct P {
        name: &'static str,
    }
    let p = P { name: "greet" };
    let s = quote_htcl!("proc #(p.name) {} { }\n");
    assert_eq!(s, "proc greet {} { }\n");
}

#[test]
fn output_parses_as_valid_htcl() {
    let name = "greet";
    let msg = "hi there";
    let s = quote_htcl!("proc #(name) {} {\n  puts #(msg)\n}\n");
    let parsed = vw_htcl::parse(&s);
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
}

// --- quote_tcl! -------------------------------------------------------------
//
// Mirror the quote_htcl! shape tests. The Tcl-dialect macro shares
// the same template parser, so the same template should produce the
// same output for the cases that overlap (which is most of them
// today — the Tcl/htcl split exists so they can DIVERGE later, not
// because their current behavior differs).

#[test]
fn tcl_literal_passthrough() {
    let s = quote_tcl!("puts hi\n");
    assert_eq!(s, "puts hi\n");
}

#[test]
fn tcl_simple_ident_interpolation() {
    let name = "greet";
    let s = quote_tcl!("proc #(name) {} { puts hi }\n");
    assert_eq!(s, "proc greet {} { puts hi }\n");
}

#[test]
fn tcl_expression_interpolation() {
    let width = 16u32;
    let s = quote_tcl!("set w #(width)\n");
    assert_eq!(s, "set w 16\n");
}

#[test]
fn tcl_values_needing_quoting_get_quoted() {
    let msg = "hello world";
    let s = quote_tcl!("puts #(msg)\n");
    assert_eq!(s, "puts \"hello world\"\n");
}

#[test]
fn tcl_braces_in_template_pass_through() {
    let name = "f";
    let s = quote_tcl!("proc #(name) {} {\n  puts hi\n}\n");
    assert_eq!(s, "proc f {} {\n  puts hi\n}\n");
}

#[test]
fn tcl_multiple_interpolations() {
    let name = "greet";
    let arg = "world";
    let s = quote_tcl!("proc #(name) { #(arg) } { puts hi }\n");
    assert_eq!(s, "proc greet { world } { puts hi }\n");
}

#[test]
fn tcl_emits_repr_proc_shape() {
    // A representative use case from step 2b: emit a per-type
    // repr proc body via quote_tcl!.
    let mangled = "string";
    let s = quote_tcl!("proc #(mangled)::repr {v} {\n  return $v\n}\n");
    assert_eq!(s, "proc string::repr {v} {\n  return $v\n}\n");
}
