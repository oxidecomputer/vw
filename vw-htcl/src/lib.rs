// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! htcl language layer.
//!
//! Provides the parser, concrete syntax tree, and analysis passes that
//! every htcl-consuming subcommand of `vw` shares. The same code drives
//! `vw run`, `vw check`, the LSP (`vw analyzer`), and (eventually) the
//! REPL (`vw repl`). Keeping a single source of truth for parsing and
//! analysis is the durable fix for the "compiler vs. IDE drift" failure
//! mode of language tooling.
//!
//! This v0 covers the Phase 0 subset from the project plan: literals,
//! variables, command substitution, `set`, `proc` (vanilla form),
//! generic command invocations, and comments. Control flow, structured
//! proc grammar, modules, and dependency-aware imports come in later
//! phases.

// `quote_tcl!` (and `quote_htcl!`) generate code that names this
// crate as `::vw_htcl::…`. Within the crate itself that path
// doesn't resolve by default; this directive aliases the current
// crate as `vw_htcl` so the macros work uniformly inside and
// outside of vw-htcl. Standard Rust idiom for self-targeting
// proc-macros.
extern crate self as vw_htcl;

pub mod ast;
pub mod cmdline;
pub mod complete;
pub mod doc;
pub mod emit;
pub mod enum_parse;
pub mod goto;
pub mod hover;
pub mod line_index;
pub mod loader;
pub mod lower;
pub mod overload;
pub mod parser;
pub mod proc_args;
pub mod rename;
pub mod repr;
pub mod scope;
pub mod signature_help;
pub mod span;
pub mod src_path;
pub mod type_parse;
pub mod unused;
pub mod validate;

pub use complete::{complete_at, Completion, CompletionKind};
pub use goto::definition_at;
pub use hover::{hover_at, HoverTarget};
pub use loader::{
    load as load_program, load_with_observer as load_program_with_observer,
    ImportEdge, LoadError, LoadObserver, LoadedFile, LoadedProgram,
    SourceRegion,
};
pub use lower::{
    extern_rename_prelude, is_extern_call, lower_command,
    lower_proc_decl_with_name, rewrite_externs, signature_table, ExternRewrite,
    SignatureTable, EXTERN_PREFIX,
};
pub use overload::emit_dispatcher;
pub use rename::{rename_at, RenameEdit};
pub use repr::{
    emit_enum_prelude, emit_primitive_prelude, emit_repr, emit_repr_with_types,
};
pub use signature_help::{signature_help_at, SignatureHelp};
pub use src_path::{
    classify as classify_src_path, PathKind, ResolveError, Resolver,
};
pub use validate::{
    build_enum_decl_table, build_signature_table_with_overloads,
    build_type_decl_table, mangle_specialization, validate,
    validate_with_all_extras, validate_with_extras, validate_with_signatures,
    Diagnostic as ValidatorDiagnostic, OverloadTable, Severity,
};

pub use ast::{
    Attribute, AttributeValue, Command, CommandKind, Document, EnumDecl,
    EnumVariant, OverloadInfo, OverloadVariant, Proc, ProcArg, ProcSignature,
    SrcImport, Stmt, TypeDecl, TypeExpr, Word, WordPart,
};
pub use line_index::{LineCol, LineIndex};
pub use parser::{parse, ParseError, ParseOutput};
pub use span::Span;
