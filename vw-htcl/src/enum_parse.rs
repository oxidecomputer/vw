// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Mini-parser for the variants block of an `enum NAME = { ... }`
//! declaration.
//!
//! Grammar (operates on the text INSIDE the body braces, not
//! including the braces themselves):
//!
//! ```text
//! Variants ::= Sep* (Variant Sep+ Variant)* Sep*
//! Variant  ::= Ident (':' Type)?
//! Sep      ::= '\n' | comment | doc_comment | whitespace
//! ```
//!
//! Variants are newline-separated (mirroring `proc {a; b}` arg-list
//! style); blank lines and `##` doc comments are ignored. The payload
//! type, when present, is parsed via [`crate::type_parse`] verbatim
//! — so anything that grammar accepts (primitives, newtypes,
//! generics, qualified) is legal here too. A future tightening could
//! reject `Qualified` payloads as nonsensical; v1 keeps it permissive
//! and lets the validator decide.

use crate::ast::EnumVariant;
use crate::span::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumParseError {
    pub message: String,
    pub span: Span,
}

/// Parse the body of an enum declaration. `text` is the contents
/// INSIDE the body braces (not including the braces). `base_offset`
/// is the absolute byte position of `text[0]` in the original
/// source so returned spans land in the right place.
pub fn parse(
    text: &str,
    base_offset: u32,
) -> Result<Vec<EnumVariant>, EnumParseError> {
    let mut p = Parser::new(text, base_offset);
    let mut variants = Vec::new();
    loop {
        p.skip_separators();
        if p.eof() {
            break;
        }
        variants.push(p.parse_variant()?);
    }
    Ok(variants)
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

    fn here(&self) -> u32 {
        self.base + self.pos as u32
    }

    fn here_span(&self) -> Span {
        let h = self.here();
        Span::new(h, h)
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(self.base + start as u32, self.base + self.pos as u32)
    }

    fn skip_horizontal_ws(&mut self) {
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c == b' ' || c == b'\t' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Skip newlines, whitespace, regular `#` comments, and `##`
    /// doc comments. Variants are separated by at least one newline
    /// (or are at the start of the body).
    fn skip_separators(&mut self) {
        loop {
            // Any whitespace, including newlines.
            while self.pos < self.bytes.len()
                && self.bytes[self.pos].is_ascii_whitespace()
            {
                self.pos += 1;
            }
            if self.eof() {
                break;
            }
            // Comment line — consume to next newline. `##` doc
            // comments are dropped here; if a future revision needs
            // to attach docs to variants, this is the spot.
            if self.bytes[self.pos] == b'#' {
                while self.pos < self.bytes.len()
                    && self.bytes[self.pos] != b'\n'
                {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    /// Variant := IDENT (':' TYPE)?
    fn parse_variant(&mut self) -> Result<EnumVariant, EnumParseError> {
        let start = self.pos;
        let (name, name_span) = self.parse_ident()?;
        self.skip_horizontal_ws();
        let payload_pos = self.pos;
        let (payload, payload_span) = if self.pos < self.bytes.len()
            && self.bytes[self.pos] == b':'
        {
            self.pos += 1; // ':'
            self.skip_horizontal_ws();
            let type_start = self.pos;
            // Consume up to end-of-line or end-of-input. The
            // type-text-extraction window stops at newline so a
            // bad payload doesn't bleed into the next variant.
            while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                self.pos += 1;
            }
            // Trim trailing horizontal whitespace from the
            // payload text so spans are tight.
            let mut end = self.pos;
            while end > type_start
                && matches!(self.bytes[end - 1], b' ' | b'\t' | b'\r')
            {
                end -= 1;
            }
            let payload_text = &self.text[type_start..end];
            let span = Span::new(
                self.base + type_start as u32,
                self.base + end as u32,
            );
            let ty = crate::type_parse::parse(
                payload_text,
                self.base + type_start as u32,
            )
            .map_err(|e| EnumParseError {
                message: e.message,
                span: e.span,
            })?;
            (Some(ty), span)
        } else {
            // Empty-payload variant. Span is a zero-width point
            // right after the name.
            let here = Span::new(
                self.base + payload_pos as u32,
                self.base + payload_pos as u32,
            );
            (None, here)
        };
        Ok(EnumVariant {
            name,
            name_span,
            payload,
            payload_span,
            span: self.span_from(start),
        })
    }

    fn parse_ident(&mut self) -> Result<(String, Span), EnumParseError> {
        self.skip_horizontal_ws();
        let start = self.pos;
        if self.eof() {
            return Err(EnumParseError {
                message: "expected variant name, found end of body".into(),
                span: self.here_span(),
            });
        }
        let first = self.bytes[self.pos];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(EnumParseError {
                message: format!(
                    "expected variant name, found `{}`",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::TypeExpr;

    fn p(s: &str) -> Vec<EnumVariant> {
        parse(s, 0).unwrap_or_else(|e| panic!("parse failed: {e:?}"))
    }

    #[test]
    fn empty_body() {
        let v = p("");
        assert!(v.is_empty());
        let v = p("   \n\n  ");
        assert!(v.is_empty());
    }

    #[test]
    fn single_variant_with_payload() {
        let v = p("Scalar: string");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Scalar");
        let ty = v[0].payload.as_ref().unwrap();
        assert_eq!(ty.name(), "string");
    }

    #[test]
    fn single_empty_payload_variant() {
        let v = p("North");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "North");
        assert!(v[0].payload.is_none());
    }

    #[test]
    fn mixed_payload_and_empty() {
        let v = p("\n  North\n  South: int\n  East\n  West\n");
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].name, "North");
        assert!(v[0].payload.is_none());
        assert_eq!(v[1].name, "South");
        assert_eq!(v[1].payload.as_ref().unwrap().name(), "int");
        assert_eq!(v[2].name, "East");
        assert!(v[2].payload.is_none());
        assert_eq!(v[3].name, "West");
        assert!(v[3].payload.is_none());
    }

    #[test]
    fn generic_payload() {
        let v = p("\n  Scalar: string\n  Nested: dict<string,Property>\n");
        assert_eq!(v.len(), 2);
        let TypeExpr::Generic { name, args, .. } =
            v[1].payload.as_ref().unwrap()
        else {
            panic!()
        };
        assert_eq!(name, "dict");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name(), "string");
        assert_eq!(args[1].name(), "Property");
    }

    #[test]
    fn comments_skipped_between_variants() {
        let v = p(
            "\n# leading comment\n## doc comment\nScalar: string\n# trailing\nNested: int\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "Scalar");
        assert_eq!(v[1].name, "Nested");
    }

    #[test]
    fn err_invalid_first_char() {
        let e = parse("123Foo: int", 0).unwrap_err();
        assert!(e.message.contains("variant name"));
    }

    #[test]
    fn err_bad_payload_type() {
        let e = parse("Scalar: <bad", 0).unwrap_err();
        assert!(
            e.message.contains("type name") || e.message.contains("unexpected"),
            "{}",
            e.message
        );
    }

    #[test]
    fn spans_use_base_offset() {
        let v = parse("Scalar: string", 100).unwrap();
        let var = &v[0];
        assert_eq!(var.name_span, Span::new(100, 106));
        assert_eq!(var.payload.as_ref().unwrap().name(), "string");
        // Payload type-span is the bare 'string' word starting at
        // 100 + 8 (after "Scalar: ").
        assert_eq!(var.payload.as_ref().unwrap().span(), Span::new(108, 114));
    }
}
