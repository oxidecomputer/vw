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

pub mod ast;
pub mod cmdline;
pub mod complete;
pub mod emit;
pub mod goto;
pub mod hover;
pub mod line_index;
pub mod loader;
pub mod lower;
pub mod parser;
pub mod proc_args;
pub mod scope;
pub mod signature_help;
pub mod span;
pub mod src_path;
pub mod validate;

pub use complete::{complete_at, Completion, CompletionKind};
pub use goto::definition_at;
pub use hover::{hover_at, HoverTarget};
pub use loader::{
    load as load_program, load_with_observer as load_program_with_observer,
    LoadError, LoadObserver, LoadedProgram,
};
pub use lower::{lower_command, signature_table, SignatureTable};
pub use signature_help::{signature_help_at, SignatureHelp};
pub use src_path::{
    classify as classify_src_path, PathKind, ResolveError, Resolver,
};
pub use validate::{validate, Diagnostic as ValidatorDiagnostic, Severity};

pub use ast::{
    Attribute, AttributeValue, Command, CommandKind, Document, Proc, ProcArg,
    ProcSignature, SrcImport, Stmt, Word, WordPart,
};
pub use line_index::{LineCol, LineIndex};
pub use parser::{parse, ParseError, ParseOutput};
pub use span::Span;
