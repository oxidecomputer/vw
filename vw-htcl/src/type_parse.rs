// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Mini-parser for htcl type expressions.
//!
//! Grammar:
//!
//! ```text
//! Type     ::= Ident ('::' Ident | '<' Type (',' Type)* '>')?
//! Ident    ::= [A-Za-z_] [A-Za-z0-9_]*
//! ```
//!
//! The `Ident '::' Ident` form yields a [`TypeExpr::Qualified`],
//! used for the `Enum::Variant` annotations on overloaded handler
//! procs. The two forms (qualified vs generic) are mutually
//! exclusive — `Enum::Variant<…>` is a parse error.
//!
//! Whitespace is permitted between tokens but not within identifiers.
//! That's why type expressions with whitespace (`dict<string, int>`)
//! must be brace-wrapped when used as a single htcl word — `dict<string,
//! int>` parses as four htcl words at the parent level, but
//! `{dict<string, int>}` parses as one. The caller of [`parse`] is
//! responsible for that unwrap before handing us the type text.
//!
//! Spans returned are absolute source spans: the caller passes a
//! `base_offset` corresponding to the byte position of the first
//! character of `text` in the original source.

use crate::ast::TypeExpr;
use crate::span::Span;

/// One parse-error from the type parser. The caller renders these as
/// regular htcl parse-error diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeParseError {
    pub message: String,
    pub span: Span,
}

/// Parse `text` as a type expression, with absolute source positions
/// rooted at `base_offset`. Returns the parsed expression on success,
/// or the first error encountered.
pub fn parse(text: &str, base_offset: u32) -> Result<TypeExpr, TypeParseError> {
    let mut p = Parser::new(text, base_offset);
    let ty = p.parse_type()?;
    p.skip_ws();
    if !p.eof() {
        return Err(TypeParseError {
            message: format!(
                "unexpected `{}` after type expression",
                p.rest().chars().next().unwrap_or('\0')
            ),
            span: p.here_span(),
        });
    }
    Ok(ty)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    base: u32,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str, base: u32) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
            base,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && self.bytes[self.pos].is_ascii_whitespace()
        {
            self.pos += 1;
        }
    }

    fn here(&self) -> u32 {
        self.base + self.pos as u32
    }

    /// Zero-width span at the current position — used for "unexpected
    /// token" diagnostics where there's no real token to underline.
    fn here_span(&self) -> Span {
        let h = self.here();
        Span::new(h, h)
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(self.base + start as u32, self.base + self.pos as u32)
    }

    /// Consume one bare identifier, returning its text and span.
    /// Identifiers start with `[A-Za-z_]` and contain `[A-Za-z0-9_]`.
    fn parse_ident(&mut self) -> Result<(String, Span), TypeParseError> {
        self.skip_ws();
        let start = self.pos;
        if self.eof() {
            return Err(TypeParseError {
                message: "expected type name, found end of input".into(),
                span: self.here_span(),
            });
        }
        let first = self.bytes[self.pos];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(TypeParseError {
                message: format!(
                    "expected type name, found `{}`",
                    first as char
                ),
                span: self.here_span(),
            });
        }
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name = self.text[start..self.pos].to_string();
        Ok((name, self.span_from(start)))
    }

    fn parse_type(&mut self) -> Result<TypeExpr, TypeParseError> {
        let start = self.pos;
        self.skip_ws();
        let ident_start = self.pos;
        let (name, name_span) = self.parse_ident()?;
        self.skip_ws();
        // Optional `::Variant` qualified-path suffix. Mutually
        // exclusive with the `<…>` generic form — `E::V<int>` is
        // rejected below.
        //
        // Deeper chains (`A::B::C::D`) collapse into a `Named` type
        // whose name is the whole colon-joined string. That's how
        // generated wrappers can reference nested-namespace
        // newtypes like `gtwiz_versal::intf0::gt_settings::Lr0Settings`
        // without teaching the whole validator about
        // multi-segment qualified paths (they never carry variant
        // semantics — they're just deep newtype references).
        if self.pos + 1 < self.bytes.len()
            && self.bytes[self.pos] == b':'
            && self.bytes[self.pos + 1] == b':'
        {
            self.pos += 2; // ::
            let (variant, variant_span) = self.parse_ident()?;
            self.skip_ws();
            // Third `::segment`? Keep going and produce a flat
            // Named type with the whole joined path as its name.
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b':'
                && self.bytes[self.pos + 1] == b':'
            {
                let mut joined = format!("{name}::{variant}");
                while self.pos + 1 < self.bytes.len()
                    && self.bytes[self.pos] == b':'
                    && self.bytes[self.pos + 1] == b':'
                {
                    self.pos += 2;
                    let (seg, _) = self.parse_ident()?;
                    joined.push_str("::");
                    joined.push_str(&seg);
                    self.skip_ws();
                }
                if !self.eof() && self.bytes[self.pos] == b'<' {
                    return Err(TypeParseError {
                        message: format!(
                            "nested-namespace type `{joined}` cannot take \
                             generic arguments"
                        ),
                        span: self.here_span(),
                    });
                }
                let span = self.span_from(start);
                return Ok(TypeExpr::Named { name: joined, span });
            }
            // Exactly two segments — the classic `Enum::Variant`
            // shape used for overload dispatch. Preserve the
            // Qualified form so the validator's variant-reference
            // rules kick in.
            //
            // Reject `E::V<…>` — qualified names don't take generic
            // args (their purpose is to name one variant of a
            // declared enum, which has no type parameters in v1).
            if !self.eof() && self.bytes[self.pos] == b'<' {
                return Err(TypeParseError {
                    message: format!(
                        "qualified type `{name}::{variant}` cannot take \
                         generic arguments"
                    ),
                    span: self.here_span(),
                });
            }
            return Ok(TypeExpr::Qualified {
                namespace: name,
                variant,
                namespace_span: name_span,
                variant_span,
                span: self.span_from(start),
            });
        }
        // Optional `<...>` generic argument list.
        if !self.eof() && self.bytes[self.pos] == b'<' {
            self.pos += 1; // <
            let mut args = Vec::new();
            // Allow empty? No — `list<>` is meaningless. Require
            // at least one arg.
            args.push(self.parse_type()?);
            self.skip_ws();
            while !self.eof() && self.bytes[self.pos] == b',' {
                self.pos += 1; // ,
                args.push(self.parse_type()?);
                self.skip_ws();
            }
            if self.eof() {
                return Err(TypeParseError {
                    message: format!(
                        "unterminated generic type `{name}<…>`: \
                         expected `>` or `,`",
                    ),
                    span: self.span_from(ident_start),
                });
            }
            if self.bytes[self.pos] != b'>' {
                return Err(TypeParseError {
                    message: format!(
                        "expected `>` or `,` in generic type `{name}<…>`, \
                         found `{}`",
                        self.bytes[self.pos] as char
                    ),
                    span: self.here_span(),
                });
            }
            self.pos += 1; // >
            return Ok(TypeExpr::Generic {
                name,
                name_span,
                args,
                span: self.span_from(start),
            });
        }
        Ok(TypeExpr::Named {
            name,
            span: name_span,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> TypeExpr {
        parse(s, 0).unwrap_or_else(|e| panic!("parse failed: {e:?}"))
    }

    #[test]
    fn named_simple() {
        let ty = p("string");
        match ty {
            TypeExpr::Named { name, span } => {
                assert_eq!(name, "string");
                assert_eq!(span, Span::new(0, 6));
            }
            _ => panic!("expected Named"),
        }
    }

    #[test]
    fn generic_single_arg() {
        let ty = p("list<bd_cell>");
        let TypeExpr::Generic {
            name, args, span, ..
        } = ty
        else {
            panic!("expected Generic");
        };
        assert_eq!(name, "list");
        assert_eq!(span, Span::new(0, 13));
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].name(), "bd_cell");
    }

    #[test]
    fn generic_two_args() {
        let ty = p("dict<string,int>");
        let TypeExpr::Generic { name, args, .. } = ty else {
            panic!();
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name(), "string");
        assert_eq!(args[1].name(), "int");
    }

    #[test]
    fn nested_generic() {
        let ty = p("dict<string,list<int>>");
        let TypeExpr::Generic { args, .. } = ty else {
            panic!()
        };
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name(), "string");
        let TypeExpr::Generic {
            name, args: inner, ..
        } = &args[1]
        else {
            panic!("expected inner generic");
        };
        assert_eq!(name, "list");
        assert_eq!(inner[0].name(), "int");
    }

    #[test]
    fn deeply_nested() {
        let ty = p("list<dict<string,bd_cell>>");
        let TypeExpr::Generic { name, args, .. } = ty else {
            panic!()
        };
        assert_eq!(name, "list");
        let TypeExpr::Generic {
            name: inner_name,
            args: inner_args,
            ..
        } = &args[0]
        else {
            panic!();
        };
        assert_eq!(inner_name, "dict");
        assert_eq!(inner_args.len(), 2);
        assert_eq!(inner_args[0].name(), "string");
        assert_eq!(inner_args[1].name(), "bd_cell");
    }

    #[test]
    fn whitespace_between_tokens_is_fine() {
        let ty = p(" dict < string , int > ");
        let TypeExpr::Generic { name, args, .. } = ty else {
            panic!()
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn span_uses_base_offset() {
        let ty = parse("bd_cell", 100).unwrap();
        let TypeExpr::Named { span, .. } = ty else {
            panic!()
        };
        assert_eq!(span, Span::new(100, 107));
    }

    #[test]
    fn err_empty_input() {
        let e = parse("", 0).unwrap_err();
        assert!(e.message.contains("expected type name"));
    }

    #[test]
    fn err_invalid_ident_start() {
        let e = parse("<bad", 0).unwrap_err();
        assert!(e.message.contains("expected type name"));
    }

    #[test]
    fn err_unterminated_generic() {
        let e = parse("list<int", 0).unwrap_err();
        assert!(e.message.contains("unterminated"));
    }

    #[test]
    fn err_missing_close_after_comma() {
        let e = parse("dict<string,int", 0).unwrap_err();
        assert!(e.message.contains("unterminated"));
    }

    #[test]
    fn err_trailing_garbage() {
        let e = parse("string extra", 0).unwrap_err();
        assert!(e.message.contains("unexpected"));
    }

    // --- qualified types (E::V) -------------------------------------

    #[test]
    fn qualified_simple() {
        let ty = p("Property::Scalar");
        let TypeExpr::Qualified {
            namespace,
            variant,
            span,
            ..
        } = ty
        else {
            panic!("expected Qualified, got {ty:?}");
        };
        assert_eq!(namespace, "Property");
        assert_eq!(variant, "Scalar");
        assert_eq!(span, Span::new(0, 16));
    }

    #[test]
    fn qualified_with_underscore_idents() {
        let ty = p("bd_obj::With_Underscore");
        let TypeExpr::Qualified {
            namespace, variant, ..
        } = ty
        else {
            panic!();
        };
        assert_eq!(namespace, "bd_obj");
        assert_eq!(variant, "With_Underscore");
    }

    #[test]
    fn err_qualified_with_generic_args() {
        let e = parse("Result::Ok<int>", 0).unwrap_err();
        assert!(
            e.message.contains("cannot take generic arguments"),
            "{}",
            e.message
        );
    }

    #[test]
    fn err_qualified_missing_variant() {
        let e = parse("Property::", 0).unwrap_err();
        assert!(e.message.contains("expected type name"), "{}", e.message);
    }

    #[test]
    fn err_single_colon_not_qualified() {
        // `Property:Scalar` (one colon) — not a qualified form. The
        // first ident parses, then the trailing `:Scalar` is junk.
        let e = parse("Property:Scalar", 0).unwrap_err();
        assert!(e.message.contains("unexpected"));
    }
}
