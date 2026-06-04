// Integration tests for `quote_htcl!`. A proc-macro crate can't use
// its own macro from within `src/`, so these tests live in `tests/`
// and pull `vw-quote` and `vw-htcl` as dev-deps.

use vw_quote::quote_htcl;

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
