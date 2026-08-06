// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `quote_htcl!` — generate htcl source code with interpolation.
//!
//! Analogous to the `quote` crate, but for htcl. Takes a string
//! literal of htcl source containing `#(expr)` interpolation markers
//! and produces a `String` of well-formed htcl at runtime. Each
//! interpolated value is passed through [`vw_htcl::emit::ToHtcl`] to
//! choose the right word form (bare vs. quoted vs. …) so generated
//! code is always parseable.
//!
//! ## Why a string literal?
//!
//! Rust's `TokenStream` doesn't preserve newlines, and newlines
//! terminate htcl commands — so a token-walking macro that reads
//! `quote_htcl! { proc x { … } { … } }` directly can't tell where one
//! statement ends and the next begins. Taking the input as a string
//! literal keeps the source text exact at zero cost in ergonomics.
//!
//! ## Syntax
//!
//! - `#(expr)` — interpolation slot. `expr` is parsed as a Rust
//!   expression at macro time and emitted via
//!   `vw_htcl::emit::ToHtcl::to_htcl(&expr)`.
//! - Anything else is literal htcl, copied verbatim except that `{`
//!   and `}` in the template don't need any escaping (the macro
//!   handles `format!` quoting for you).
//!
//! ## Example
//!
//! ```ignore
//! use vw_quote::quote_htcl;
//! let name = "greet";
//! let who = "world";
//! let s = quote_htcl!("\
//!     proc #(name) {
//!         #(who)
//!     } { puts hi }
//! ");
//! // s == "proc greet {\n    world\n} { puts hi }\n"
//! // (the interpolated values get `ToHtcl`-formatted; here both are
//! //  bare identifiers, so they emit as-is.)
//! ```

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{parse_macro_input, Expr, LitStr};

#[proc_macro]
pub fn quote_htcl(input: TokenStream) -> TokenStream {
    expand(input, Dialect::Htcl)
}

/// Same template grammar as [`quote_htcl!`], but routes interpolated
/// values through [`vw_htcl::emit::ToTcl`] and produces pure Tcl
/// (no htcl-specific attribute handling). Use for compiler-emitted
/// runtime helpers — `repr` procs, `kwargs` shim glue, anything that
/// lives in the Tcl interpreter and should never look like htcl.
///
/// The split exists so future Tcl-only behavior (typed `Tcl_Obj`
/// handle quoting, etc.) can land on `ToTcl` without changing
/// `quote_htcl!`'s contract.
#[proc_macro]
pub fn quote_tcl(input: TokenStream) -> TokenStream {
    expand(input, Dialect::Tcl)
}

/// Which interpolation trait the macro routes through. The template
/// parsing is shared verbatim — the only thing that differs is the
/// trait + method name used in the generated `format!` arguments.
#[derive(Clone, Copy)]
enum Dialect {
    Htcl,
    Tcl,
}

fn expand(input: TokenStream, dialect: Dialect) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    let template_text = lit.value();
    let lit_span = lit.span();

    let (template, exprs) = match split_template(&template_text, lit_span) {
        Ok(parts) => parts,
        Err(e) => return e.to_compile_error().into(),
    };

    // Escape literal `{`/`}` so the format string parser leaves them
    // alone; replace each `#(…)` site with a positional placeholder.
    let format_string = render_format_string(&template, exprs.len());
    let format_lit = LitStr::new(&format_string, Span::call_site());

    let exprs: Vec<TokenStream2> =
        exprs.into_iter().map(|e| e.to_token_stream()).collect();

    let out = match dialect {
        Dialect::Htcl => quote! {{
            // Bring the trait into scope so `(&expr).to_htcl()` resolves
            // without the caller needing to import it.
            #[allow(unused_imports)]
            use ::vw_htcl::emit::ToHtcl as _;
            ::std::format!(
                #format_lit,
                #( (&{ #exprs }).to_htcl() ),*
            )
        }},
        Dialect::Tcl => quote! {{
            #[allow(unused_imports)]
            use ::vw_htcl::emit::ToTcl as _;
            ::std::format!(
                #format_lit,
                #( (&{ #exprs }).to_tcl() ),*
            )
        }},
    };
    out.into()
}

// --- template parsing ------------------------------------------------------

/// One piece of the parsed template.
enum Piece {
    /// Verbatim text from the template.
    Text(String),
    /// An interpolation site; index into the parallel `exprs` Vec.
    Interp,
}

/// Split the template string into alternating text / interpolation
/// pieces, parsing each `#(…)` body as a Rust expression.
fn split_template(
    text: &str,
    lit_span: Span,
) -> syn::Result<(Vec<Piece>, Vec<Expr>)> {
    let mut pieces = Vec::new();
    let mut exprs = Vec::new();
    let mut buf = String::new();

    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Allow `##` doc comments and `#` plain comments to pass
        // through unmolested: interpolation is `#(...)`, so we only
        // engage when `#` is immediately followed by `(`.
        if c == b'#' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            if !buf.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut buf)));
            }
            let body_end = match find_matching_paren(bytes, i + 1) {
                Some(end) => end,
                None => {
                    return Err(syn::Error::new(
                        lit_span,
                        "unterminated `#(...)` in quote_htcl! template",
                    ));
                }
            };
            // bytes[i+2 .. body_end] is the inside of the parens.
            let expr_src = &text[i + 2..body_end];
            let expr: Expr = syn::parse_str(expr_src).map_err(|e| {
                syn::Error::new(
                    lit_span,
                    format!(
                        "could not parse interpolation `{expr_src}` as a \
                         Rust expression: {e}"
                    ),
                )
            })?;
            exprs.push(expr);
            pieces.push(Piece::Interp);
            i = body_end + 1;
            continue;
        }
        // Push this char (handle UTF-8 boundary by stepping a full
        // char rather than a byte).
        let ch_start = i;
        // Safe because `i` is always at a UTF-8 boundary (we only
        // advance by full chars or past ASCII chars we recognized).
        let ch = text[ch_start..].chars().next().unwrap();
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        pieces.push(Piece::Text(buf));
    }
    Ok((pieces, exprs))
}

/// Find the byte index of the `)` that matches an opening `(` at
/// `bytes[open]`. Tracks nested parens. Returns `None` on unterminated.
fn find_matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Build the format string passed to `std::format!`, with literal
/// `{`/`}` doubled and `{0}` / `{1}` / … placeholders inserted at each
/// interpolation site.
fn render_format_string(pieces: &[Piece], n_interps: usize) -> String {
    let mut out = String::new();
    let mut next_interp = 0usize;
    let _ = n_interps;
    for piece in pieces {
        match piece {
            Piece::Text(s) => {
                for c in s.chars() {
                    match c {
                        '{' => out.push_str("{{"),
                        '}' => out.push_str("}}"),
                        other => out.push(other),
                    }
                }
            }
            Piece::Interp => {
                use std::fmt::Write;
                write!(out, "{{{}}}", next_interp).unwrap();
                next_interp += 1;
            }
        }
    }
    out
}

trait ToTokenStream {
    fn to_token_stream(&self) -> TokenStream2;
}
impl ToTokenStream for Expr {
    fn to_token_stream(&self) -> TokenStream2 {
        quote! { #self }
    }
}
