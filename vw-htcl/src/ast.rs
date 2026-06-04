// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Concrete syntax tree for htcl.
//!
//! Every node carries a [`Span`] so the same tree drives diagnostics,
//! hover, navigation, and source-faithful lowering back to TCL. The
//! tree is concrete in the sense that it retains enough information to
//! recover the original source (comments, blank lines, word forms);
//! later passes derive a stripped AST for analysis.

use crate::span::Span;

#[derive(Clone, Debug)]
pub struct Document {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Command(Command),
    Comment(Comment),
    Error(ParseFailure),
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Command(c) => c.span,
            Stmt::Comment(c) => c.span,
            Stmt::Error(e) => e.span,
        }
    }
}

/// A single TCL command — a whitespace-separated sequence of words,
/// terminated by newline, semicolon, or EOF.
#[derive(Clone, Debug)]
pub struct Command {
    pub words: Vec<Word>,
    pub span: Span,
    pub kind: CommandKind,
    /// Doc comments (`##`) immediately preceding the command, in
    /// source order with the `##` prefix stripped.
    pub doc_comments: Vec<String>,
}

/// Recognized command shapes. Generic covers any unrecognized command;
/// specific variants exist so downstream passes (symbol tables, the
/// LSP, the structured-proc work in Phase 2) can act on them without
/// re-parsing.
#[derive(Clone, Debug)]
pub enum CommandKind {
    Generic,
    Set,
    Proc(Proc),
    Src(SrcImport),
}

/// A `src <path>` import — load and evaluate another htcl module.
///
/// The path's *form* is classified at load time, not here: leading
/// `@name/` resolves through the workspace's `vw.toml` dependencies,
/// a leading `/` is filesystem-absolute, anything else is relative to
/// the importing file's directory. `path` is `None` only when the
/// path word couldn't be reduced to literal text (e.g. it contains
/// `$var` / `[cmd]` substitutions); those imports are diagnosed
/// downstream rather than parsed structurally.
#[derive(Clone, Debug)]
pub struct SrcImport {
    pub path: Option<String>,
    pub path_span: Span,
}

/// A `proc` declaration.
///
/// The outer shape (name, args span, body span) comes from the Phase 0
/// parser. The structured args grammar (Phase 2) is reparsed from
/// `args_span` and stored in [`signature`](Self::signature). When
/// `signature` is `None` the args body couldn't be parsed at all
/// (e.g. mid-edit syntax error); diagnostics for that live in the
/// document's parse-error list.
#[derive(Clone, Debug)]
pub struct Proc {
    /// Bare-text proc name when it could be extracted; `None` for
    /// programmatically-named procs (e.g. names built from
    /// substitution).
    pub name: Option<String>,
    pub name_span: Span,
    pub args_span: Span,
    pub body_span: Span,
    pub signature: Option<ProcSignature>,
    /// The body parsed into statements, with spans in absolute
    /// (whole-source) coordinates. Populated by a post-pass after the
    /// outer parse; empty until then and for bodies that are pure
    /// braced text with no commands. Lowering still ships the body
    /// verbatim from [`body_span`](Self::body_span) — this field
    /// exists so navigation, hover, and analysis can see *into* a
    /// proc body. Nested procs declared here have their own `body`
    /// populated recursively.
    pub body: Vec<Stmt>,
}

/// Structured proc-argument signature.
///
/// One entry per declared argument, in source order. The order is the
/// canonical positional order used when lowering keyword-arg call
/// sites to Tcl-positional calls for the EDA backend.
#[derive(Clone, Debug)]
pub struct ProcSignature {
    pub args: Vec<ProcArg>,
    pub span: Span,
}

impl ProcSignature {
    pub fn find(&self, name: &str) -> Option<&ProcArg> {
        self.args.iter().find(|a| a.name == name)
    }
}

#[derive(Clone, Debug)]
pub struct ProcArg {
    pub name: String,
    pub name_span: Span,
    pub doc_comments: Vec<String>,
    pub attributes: Vec<Attribute>,
    pub span: Span,
}

impl ProcArg {
    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.name == name)
    }
}

/// Raw attribute as parsed: name plus zero or more comma-separated
/// values. Semantic interpretation (default, required, enum, range,
/// requires, conflicts, deprecated) lives in the validators, not
/// here — keeping the AST shape unopinionated lets new attribute
/// names land without a parser change.
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: String,
    pub name_span: Span,
    pub values: Vec<AttributeValue>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum AttributeValue {
    Integer { value: i64, span: Span },
    String { value: String, span: Span },
    Ident { value: String, span: Span },
}

impl AttributeValue {
    pub fn span(&self) -> Span {
        match self {
            AttributeValue::Integer { span, .. }
            | AttributeValue::String { span, .. }
            | AttributeValue::Ident { span, .. } => *span,
        }
    }

    /// Render the value back to a Tcl-style literal, suitable for
    /// comparison against a runtime arg or for emitting in lowered
    /// Tcl. Integers and idents stringify as-is; strings get
    /// double-quoted with naive escaping.
    pub fn to_tcl_literal(&self) -> String {
        match self {
            AttributeValue::Integer { value, .. } => value.to_string(),
            AttributeValue::Ident { value, .. } => value.clone(),
            AttributeValue::String { value, .. } => {
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            }
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            AttributeValue::Ident { value, .. }
            | AttributeValue::String { value, .. } => value,
            AttributeValue::Integer { .. } => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Comment {
    /// Comment text with the leading `#` removed; for `##` doc
    /// comments, both `#`s are removed.
    pub text: String,
    pub span: Span,
    pub is_doc: bool,
}

#[derive(Clone, Debug)]
pub struct ParseFailure {
    pub message: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Word {
    pub form: WordForm,
    pub parts: Vec<WordPart>,
    pub span: Span,
}

impl Word {
    /// If this word is a single literal text part (no interpolation),
    /// return its value. Useful for matching command names, fixed
    /// keywords, and option flags without rebuilding the string.
    pub fn as_text(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Text { value, .. }] => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordForm {
    Bare,
    Quoted,
    Braced,
}

#[derive(Clone, Debug)]
pub enum WordPart {
    Text {
        value: String,
        span: Span,
    },
    VarRef {
        name: String,
        span: Span,
    },
    /// `[ cmd ... ]` command substitution. `source` is the raw interior
    /// text (between the brackets) and `span` covers the whole
    /// `[...]`. `body` is populated by a post-pass that recursively
    /// parses the interior into statements with absolute spans, so
    /// hover / goto / signature-help can descend in.
    CmdSubst {
        source: String,
        span: Span,
        body: Vec<Stmt>,
    },
    Escape {
        value: char,
        span: Span,
    },
}

impl WordPart {
    pub fn span(&self) -> Span {
        match self {
            WordPart::Text { span, .. }
            | WordPart::VarRef { span, .. }
            | WordPart::CmdSubst { span, .. }
            | WordPart::Escape { span, .. } => *span,
        }
    }
}
