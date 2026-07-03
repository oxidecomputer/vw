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

// `Stmt::Command(Command)` is ~320 bytes while the other variants
// are <50; clippy flags the size disparity and suggests boxing
// `Command`. We don't box because:
//  - Commands are by far the most common variant (often >95% of
//    Stmt instances in real source), so the boxed-pointer
//    indirection on the hot path would cost more than the
//    wasted bytes in rare Comment/Error variants.
//  - Boxing would ripple through ~50 pattern-match sites
//    (`let Stmt::Command(cmd) = ...`) and complicate the AST's
//    "by-value clone-and-mutate" rewrite passes.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
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
    /// Source span covering the whole `##` block — from the first
    /// `#` of the first line to the newline after the last line.
    /// `None` when the command has no doc comments. Used by the
    /// analyzer to answer "is the cursor inside this command's doc
    /// block?" so `[NAME]` references inside `##` text can resolve
    /// via goto/hover.
    pub doc_comments_span: Option<Span>,
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
    NamespaceEval(NamespaceEval),
    /// A `type <name> = <underlying>` declaration. Compile-time only
    /// — never lowered to Tcl. Together with the required
    /// `<name>::repr` / `from` / `to` procs (enforced by the
    /// validator), introduces a new newtype the rest of the program
    /// can reference in return-type annotations and (later) arg
    /// annotations.
    TypeDecl(TypeDecl),
    /// An `enum <name> = { <variants> }` declaration. Compile-time
    /// only — the lowerer emits the auto-generated constructor /
    /// repr / accessor procs through the repr-codegen path, NOT via
    /// shipping the source verbatim. Variants are
    /// `IDENT (':' TYPE)?`; the optional `:TYPE` payload makes
    /// empty-payload variants first-class.
    EnumDecl(EnumDecl),
}

/// A `namespace eval <name> { <body> }` block.
///
/// Recognized at parse time so that any `proc` declarations inside
/// the braces register in the document's signature table under the
/// qualified name `<name>::<proc>` (Tcl namespace semantics), and
/// the analyzer can offer the same hover / completion / signature
/// help / goto experience for namespaced procs as for top-level
/// ones. The body parses as a script just like a proc body, so
/// nested `namespace eval` blocks compose.
#[derive(Clone, Debug)]
pub struct NamespaceEval {
    /// Bare-text namespace name when extractable (the common case),
    /// `None` when the name word couldn't be reduced to literal text
    /// (e.g. it contains substitutions). Multi-segment names like
    /// `foo::bar` are preserved as-is and the analyzer uses them as
    /// the full prefix.
    pub name: Option<String>,
    pub name_span: Span,
    pub body_span: Span,
    /// The body parsed into statements. Spans are absolute (whole-
    /// source) coordinates, same convention as [`Proc::body`].
    pub body: Vec<Stmt>,
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
    /// Optional return-type annotation: the 4th word of a
    /// `proc NAME { args } TYPE { body }` declaration, parsed as a
    /// [`TypeExpr`]. `None` means "no annotation present" — the
    /// proc still works, but downstream type-driven machinery
    /// (REPL repr printer, hover, future call-site validation)
    /// falls back to its untyped path. Bracketed forms like
    /// `{dict<string, string>}` are unwrapped before type-parsing.
    pub return_type: Option<TypeExpr>,
    /// Source span of the 4th-word type slot when present; `None`
    /// when the proc has no annotation. The span covers the outer
    /// word including any wrapping braces, so diagnostics can
    /// underline the entire annotation.
    pub return_type_span: Option<Span>,
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

/// A `type NAME = UNDERLYING` declaration.
///
/// Introduces a newtype wrapper around an existing type. The
/// validator requires the user to ALSO define three procs in the
/// `<name>::` namespace: `repr` (rendering to a `string`), `from`
/// (lifting an underlying value, with optional validation), and
/// `to` (extracting the underlying value). See the validator for
/// the exact signature shapes enforced.
///
/// Compile-time only — never lowered to Tcl. The newtype's runtime
/// representation is identical to the underlying type; the
/// distinction lives entirely in the analyzer / printer / future
/// type-checker.
#[derive(Clone, Debug)]
pub struct TypeDecl {
    /// Bare-text type name when extractable; `None` for
    /// programmatically-named declarations (vanishingly rare;
    /// kept consistent with [`Proc::name`]'s convention).
    pub name: Option<String>,
    pub name_span: Span,
    /// The underlying type, parsed from the right-hand side of `=`.
    /// `None` when the right-hand side couldn't be parsed as a
    /// type expression (e.g. mid-edit). Diagnostics for that live
    /// in the document's parse-error list.
    pub underlying: Option<TypeExpr>,
    /// Span of the underlying-type word (outer, including any
    /// wrapping braces) so diagnostics can underline it.
    pub underlying_span: Span,
}

/// An `enum <name> = { <variants> }` declaration. The body is
/// brace-wrapped and newline-separated; each variant is
/// `IDENT (':' TYPE)?`.
///
/// Compile-time only — codegen lowers this to a `namespace eval
/// <Name> { … }` block containing auto-generated constructors,
/// `repr`/`from`/`to`, and `tag`/`payload` accessors. The
/// validator enforces variant-name uniqueness within an enum and
/// that variant payload types reference known type names.
#[derive(Clone, Debug)]
pub struct EnumDecl {
    /// Bare-text enum name when extractable.
    pub name: Option<String>,
    pub name_span: Span,
    /// Declared variants, in source order.
    pub variants: Vec<EnumVariant>,
    /// Span of the brace-wrapped variants block (outer, including
    /// the braces) so diagnostics can underline it.
    pub body_span: Span,
}

/// One variant inside an [`EnumDecl`]. `payload` is `None` for
/// empty-payload variants (e.g. `North` in `enum Direction = {
/// North; South: int }`).
#[derive(Clone, Debug)]
pub struct EnumVariant {
    pub name: String,
    pub name_span: Span,
    pub payload: Option<TypeExpr>,
    /// Span of the payload-type word, or zero-length at the
    /// variant's end position for empty-payload variants.
    pub payload_span: Span,
    /// Span covering the full `NAME (':' TYPE)?` form.
    pub span: Span,
}

/// Side-table entry produced by the validator's overload-classifier
/// pass: for a public proc name that resolved to an enum-overload
/// set, records which enum drives dispatch and where each variant's
/// specialization lives. The codegen step uses this to synthesize
/// the public dispatcher proc; the analyzer uses it to render
/// overload information in hover / signature help.
#[derive(Clone, Debug)]
pub struct OverloadInfo {
    /// The public, user-facing proc name (e.g. `handle_prop`).
    pub public_name: String,
    /// The enum this overload set dispatches on (e.g. `Property`).
    pub enum_name: String,
    /// Shared arg name across all overload arms (e.g. `v`). The
    /// validator enforces every arm uses the same name so the
    /// dispatcher can pass the payload via the kwargs protocol
    /// (`-<name> <payload>`) without per-arm gymnastics.
    pub dispatch_arg_name: String,
    /// One entry per variant, in declaration order on the enum.
    pub variants: Vec<OverloadVariant>,
    /// Span on the first overload's name — used as the diagnostic
    /// anchor for overload-set-wide errors.
    pub anchor_span: Span,
}

#[derive(Clone, Debug)]
pub struct OverloadVariant {
    /// The variant short-name (e.g. `Scalar`, `Nested`).
    pub variant_name: String,
    /// The mangled internal proc name the specialization runs
    /// under at runtime (e.g. `__handle_prop__Scalar`).
    pub mangled_proc_name: String,
    /// Span of the variant-arg annotation on the specialization's
    /// first argument — diagnostic anchor when something's
    /// specifically wrong with this arm.
    pub dispatch_arg_span: Span,
}

/// A type expression — the syntactic form of a type used in
/// `proc NAME { args } TYPE { body }` return annotations and on the
/// right-hand side of `type NAME = TYPE` declarations.
///
/// Newtypes (`bd_cell`, `widget`, user inventions) and primitives
/// (`string`, `int`, `bool`, `unit`) share the [`Named`] variant —
/// the distinction lives in the validator's type table, not the
/// AST. Containers (`list<T>`, `dict<K, V>`, and any future shape
/// with the same `name<args>` surface) are [`Generic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeExpr {
    Named {
        name: String,
        span: Span,
    },
    Generic {
        name: String,
        name_span: Span,
        args: Vec<TypeExpr>,
        /// Full span including `<` … `>`.
        span: Span,
    },
    /// `Enum::Variant` — a qualified path naming a single variant
    /// of a declared enum. Only legal as an arg-type annotation on
    /// an overloaded handler proc (the dispatch indicator); the
    /// validator rejects this variant anywhere else (return types,
    /// generic args, nested positions).
    Qualified {
        namespace: String,
        variant: String,
        namespace_span: Span,
        variant_span: Span,
        /// Full span covering `namespace::variant`.
        span: Span,
    },
}

impl TypeExpr {
    pub fn name(&self) -> &str {
        match self {
            TypeExpr::Named { name, .. } | TypeExpr::Generic { name, .. } => {
                name
            }
            TypeExpr::Qualified { namespace, .. } => namespace,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named { span, .. }
            | TypeExpr::Generic { span, .. }
            | TypeExpr::Qualified { span, .. } => *span,
        }
    }
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
    /// The declared return type, copied here from [`Proc::return_type`]
    /// at parse time so the signature-table-based lookup paths
    /// (REPL formatter, hover) don't have to re-walk back to the
    /// Proc node. `None` for unannotated procs.
    pub return_type: Option<TypeExpr>,
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
    /// Source span covering the whole `##` block that attaches to
    /// this arg — `None` when the arg has no doc comments. See
    /// [`Command::doc_comments_span`] for the analyzer-side rationale.
    pub doc_comments_span: Option<Span>,
    pub attributes: Vec<Attribute>,
    /// Optional `: TYPE` annotation on the arg. `Some` when the
    /// source carries `name: bd_cell` style; `None` when the arg
    /// is untyped (the legacy form). Used by the validator (full
    /// shape check on newtype repr/from/to procs) and by the
    /// analyzer's hover / signature-help displays.
    pub type_annotation: Option<TypeExpr>,
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
