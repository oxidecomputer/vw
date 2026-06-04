// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Parser for the structured proc-arg grammar (Phase 2).
//!
//! Operates on the inner contents of a `proc`'s args braces — the
//! span passed in must point at the text between the braces, not
//! including the braces themselves. All spans on the returned AST
//! nodes are absolute file offsets, so they slot directly into the
//! main document's diagnostics without rebasing.
//!
//! Grammar:
//!
//! ```text
//! args      := arg_item*
//! arg_item  := doc_comment* attribute* IDENT
//! attribute := '@' IDENT ( '(' value ( ',' value )* ')' )?
//! value     := integer | string | ident
//! ```
//!
//! Whitespace, blank lines, and non-doc comments are skippable
//! between items.

use crate::ast::{Attribute, AttributeValue, ProcArg, ProcSignature};
use crate::parser::ParseError;
use crate::span::Span;

pub fn parse_proc_args(
    full_source: &str,
    args_span: Span,
) -> (ProcSignature, Vec<ParseError>) {
    let inner = args_span.slice(full_source);
    let mut state = State {
        inner,
        base: args_span.start,
        pos: 0,
        errors: Vec::new(),
    };
    let mut args = Vec::new();
    state.parse_args(&mut args);
    let State { errors, .. } = state;
    (
        ProcSignature {
            args,
            span: args_span,
        },
        errors,
    )
}

struct State<'a> {
    inner: &'a str,
    /// Absolute file offset where `inner` starts.
    base: u32,
    /// Byte offset into `inner`.
    pos: usize,
    errors: Vec<ParseError>,
}

impl<'a> State<'a> {
    fn at_eof(&self) -> bool {
        self.pos >= self.inner.len()
    }

    fn current(&self) -> char {
        self.inner[self.pos..].chars().next().unwrap_or('\0')
    }

    fn peek_at(&self, offset: usize) -> char {
        let target = self.pos + offset;
        if target >= self.inner.len() {
            '\0'
        } else {
            self.inner[target..].chars().next().unwrap_or('\0')
        }
    }

    fn abs(&self) -> u32 {
        self.base + self.pos as u32
    }

    fn bump(&mut self) {
        if let Some(c) = self.inner[self.pos..].chars().next() {
            self.pos += c.len_utf8();
        }
    }

    fn skip_horizontal_ws(&mut self) {
        while !self.at_eof() {
            let c = self.current();
            if c == ' ' || c == '\t' || c == '\r' {
                self.bump();
            } else {
                break;
            }
        }
    }

    /// Consume blank lines, comments, and whitespace; doc comments
    /// (`##`) are collected and returned so they can attach to the
    /// next arg item.
    fn skip_separators(&mut self) -> Vec<String> {
        let mut docs = Vec::new();
        loop {
            // Whitespace including newlines
            while !self.at_eof() {
                let c = self.current();
                if c.is_whitespace() {
                    self.bump();
                } else {
                    break;
                }
            }
            if self.at_eof() {
                break;
            }
            if self.current() == '#' {
                let is_doc = self.peek_at(1) == '#';
                self.bump();
                if is_doc {
                    self.bump();
                }
                if !self.at_eof() && self.current() == ' ' {
                    self.bump();
                }
                let text_start = self.pos;
                while !self.at_eof() && self.current() != '\n' {
                    self.bump();
                }
                let text = self.inner[text_start..self.pos].to_string();
                if is_doc {
                    docs.push(text);
                }
                continue;
            }
            break;
        }
        docs
    }

    fn parse_args(&mut self, out: &mut Vec<ProcArg>) {
        loop {
            let docs = self.skip_separators();
            if self.at_eof() {
                if !docs.is_empty() {
                    // Doc comments with nothing to attach to. Warn so
                    // the user knows they're unused.
                    self.errors.push(ParseError {
                        message: "doc comment with no following argument"
                            .into(),
                        span: Span::new(self.abs(), self.abs()),
                    });
                }
                break;
            }
            let item_start = self.abs();
            let mut attributes = Vec::new();
            // Attributes can be interleaved with whitespace and
            // doc comments themselves can't appear between attrs,
            // only at the head — that's the convention from the
            // project plan ("doc comments first, then attributes in
            // any order, then the argument name").
            while !self.at_eof() && self.current() == '@' {
                if let Some(attr) = self.parse_attribute() {
                    attributes.push(attr);
                }
                self.skip_horizontal_ws();
                // Allow newlines between attributes.
                while !self.at_eof() && self.current() == '\n' {
                    self.bump();
                    self.skip_horizontal_ws();
                }
            }
            // Identifier.
            self.skip_horizontal_ws();
            if self.at_eof() {
                self.errors.push(ParseError {
                    message: "expected argument name".into(),
                    span: Span::new(item_start, self.abs()),
                });
                break;
            }
            let name_start = self.abs();
            let name = self.consume_ident();
            if name.is_empty() {
                let c = self.current();
                self.errors.push(ParseError {
                    message: format!("expected argument name, found {c}"),
                    span: Span::new(self.abs(), self.abs() + 1),
                });
                // Resync: drop whatever non-whitespace junk is here
                // so we can try the next item.
                while !self.at_eof() && !self.current().is_whitespace() {
                    self.bump();
                }
                continue;
            }
            let name_span = Span::new(name_start, self.abs());
            let span = Span::new(item_start, self.abs());
            out.push(ProcArg {
                name,
                name_span,
                doc_comments: docs,
                attributes,
                span,
            });
        }
    }

    fn parse_attribute(&mut self) -> Option<Attribute> {
        let start = self.abs();
        self.bump(); // '@'
        let name_start = self.abs();
        let name = self.consume_ident();
        if name.is_empty() {
            self.errors.push(ParseError {
                message: "expected attribute name after @".into(),
                span: Span::new(start, self.abs()),
            });
            return None;
        }
        let name_span = Span::new(name_start, self.abs());
        let mut values = Vec::new();
        if !self.at_eof() && self.current() == '(' {
            self.bump();
            loop {
                self.skip_horizontal_ws();
                // Allow newlines inside the value list
                while !self.at_eof() && self.current() == '\n' {
                    self.bump();
                    self.skip_horizontal_ws();
                }
                if self.at_eof() {
                    self.errors.push(ParseError {
                        message: "unterminated attribute argument list".into(),
                        span: Span::new(start, self.abs()),
                    });
                    break;
                }
                if self.current() == ')' {
                    self.bump();
                    break;
                }
                match self.parse_value() {
                    Some(v) => values.push(v),
                    None => {
                        // Drop characters up to `,` or `)` to resync.
                        while !self.at_eof() {
                            let c = self.current();
                            if c == ',' || c == ')' || c == '\n' {
                                break;
                            }
                            self.bump();
                        }
                    }
                }
                self.skip_horizontal_ws();
                while !self.at_eof() && self.current() == '\n' {
                    self.bump();
                    self.skip_horizontal_ws();
                }
                if self.at_eof() {
                    continue;
                }
                if self.current() == ',' {
                    self.bump();
                }
            }
        }
        Some(Attribute {
            name,
            name_span,
            values,
            span: Span::new(start, self.abs()),
        })
    }

    fn parse_value(&mut self) -> Option<AttributeValue> {
        let start = self.abs();
        let c = self.current();
        if c == '"' {
            self.bump();
            let text_start = self.pos;
            let mut buf = String::new();
            while !self.at_eof() && self.current() != '"' {
                if self.current() == '\\' {
                    self.bump();
                    if !self.at_eof() {
                        buf.push(self.current());
                        self.bump();
                    }
                } else {
                    buf.push(self.current());
                    self.bump();
                }
            }
            let _ = text_start;
            if self.at_eof() {
                self.errors.push(ParseError {
                    message: "unterminated string".into(),
                    span: Span::new(start, self.abs()),
                });
            } else {
                self.bump(); // closing "
            }
            Some(AttributeValue::String {
                value: buf,
                span: Span::new(start, self.abs()),
            })
        } else if c == '-' || c.is_ascii_digit() {
            let mut buf = String::new();
            if c == '-' {
                buf.push('-');
                self.bump();
            }
            while !self.at_eof() && self.current().is_ascii_digit() {
                buf.push(self.current());
                self.bump();
            }
            match buf.parse::<i64>() {
                Ok(value) => Some(AttributeValue::Integer {
                    value,
                    span: Span::new(start, self.abs()),
                }),
                Err(_) => {
                    self.errors.push(ParseError {
                        message: format!("invalid integer: {buf}"),
                        span: Span::new(start, self.abs()),
                    });
                    None
                }
            }
        } else if is_ident_start(c) {
            let value = self.consume_ident();
            Some(AttributeValue::Ident {
                value,
                span: Span::new(start, self.abs()),
            })
        } else {
            self.errors.push(ParseError {
                message: format!("expected attribute value, found {c}"),
                span: Span::new(start, self.abs() + 1),
            });
            None
        }
    }

    fn consume_ident(&mut self) -> String {
        let mut out = String::new();
        let mut first = true;
        while !self.at_eof() {
            let c = self.current();
            let ok = if first {
                is_ident_start(c)
            } else {
                is_ident_continue(c)
            };
            if !ok {
                break;
            }
            out.push(c);
            self.bump();
            first = false;
        }
        out
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> (ProcSignature, Vec<ParseError>) {
        // Pretend the inner args text starts at offset 0 in a virtual
        // source identical to `input`.
        let span = Span::new(0, input.len() as u32);
        parse_proc_args(input, span)
    }

    #[test]
    fn empty_signature() {
        let (sig, errs) = parse("");
        assert!(errs.is_empty());
        assert!(sig.args.is_empty());
    }

    #[test]
    fn plain_arg_names() {
        let (sig, errs) = parse("a b c");
        assert!(errs.is_empty(), "{:?}", errs);
        let names: Vec<&str> =
            sig.args.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn doc_comments_and_attributes() {
        let src = "
  ## first arg doc
  @default(0)
  has_tkeep

  ## tdata width
  @default(8)
  @enum(1, 2, 4, 8, 16)
  tdata_num_bytes
";
        let (sig, errs) = parse(src);
        assert!(errs.is_empty(), "{:?}", errs);
        assert_eq!(sig.args.len(), 2);

        let a = &sig.args[0];
        assert_eq!(a.name, "has_tkeep");
        assert_eq!(a.doc_comments, vec!["first arg doc".to_string()]);
        assert_eq!(a.attributes.len(), 1);
        assert_eq!(a.attributes[0].name, "default");
        assert!(matches!(
            a.attributes[0].values[0],
            AttributeValue::Integer { value: 0, .. }
        ));

        let b = &sig.args[1];
        assert_eq!(b.name, "tdata_num_bytes");
        assert_eq!(b.attributes.len(), 2);
        assert_eq!(b.attributes[0].name, "default");
        assert_eq!(b.attributes[1].name, "enum");
        assert_eq!(b.attributes[1].values.len(), 5);
    }

    #[test]
    fn required_attribute_no_args() {
        let (sig, errs) = parse("@required name");
        assert!(errs.is_empty(), "{:?}", errs);
        assert_eq!(sig.args.len(), 1);
        assert_eq!(sig.args[0].attributes[0].name, "required");
        assert!(sig.args[0].attributes[0].values.is_empty());
    }

    #[test]
    fn ident_attribute_values() {
        let (sig, errs) = parse("@requires(has_tuser) tuser_width");
        assert!(errs.is_empty(), "{:?}", errs);
        let attr = &sig.args[0].attributes[0];
        assert_eq!(attr.name, "requires");
        assert!(matches!(
            attr.values[0],
            AttributeValue::Ident { ref value, .. } if value == "has_tuser"
        ));
    }

    #[test]
    fn string_attribute_values() {
        let (sig, errs) = parse("@deprecated(\"use foo instead\") legacy_flag");
        assert!(errs.is_empty(), "{:?}", errs);
        let attr = &sig.args[0].attributes[0];
        assert_eq!(attr.name, "deprecated");
        assert!(matches!(
            attr.values[0],
            AttributeValue::String { ref value, .. }
                if value == "use foo instead"
        ));
    }

    #[test]
    fn error_on_missing_arg_name() {
        let (_, errs) = parse("@default(0)\n@required");
        assert!(!errs.is_empty());
    }

    #[test]
    fn error_on_garbage_attribute_value() {
        let (_, errs) = parse("@enum(1, &, 3) x");
        assert!(!errs.is_empty(), "expected an error for `&`");
    }
}
