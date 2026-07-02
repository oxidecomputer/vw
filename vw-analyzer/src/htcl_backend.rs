// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! htcl [`LanguageBackend`] — native, in-process, using `vw-htcl`.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, Documentation, Hover, HoverContents, InsertTextFormat,
    Location, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel,
    Position, Range, SignatureHelp, SignatureInformation, SymbolInformation,
    SymbolKind, TextEdit, Url,
};
use vw_htcl::{
    complete_at, definition_at, hover_at, parse, signature_help_at, validate,
    Attribute, AttributeValue, CommandKind, Completion, CompletionKind,
    HoverTarget, LineCol, LineIndex, ProcArg, ProcSignature, Severity, Stmt,
};

use crate::backend::LanguageBackend;

#[derive(Default)]
pub struct HtclBackend {
    docs: Arc<RwLock<HashMap<Url, DocState>>>,
    /// Editor-supplied workspace roots (LSP `rootUri` /
    /// `workspaceFolders`). Consulted as a fallback when the file
    /// currently being analyzed sits outside the enclosing
    /// `vw.toml` — e.g. after a goto-def has taken the user into a
    /// dep-cache dir. Without this, dep names declared only in the
    /// editor-root workspace fail to resolve and every `@name/…`
    /// import in the visited file goes dead.
    workspace_roots: Arc<RwLock<Vec<std::path::PathBuf>>>,
}

struct DocState {
    text: String,
}

impl HtclBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the editor-supplied workspace roots. Callers
    /// pass this into [`crate::workspace::build_view`] etc. as
    /// fallback dep-lookup roots — see `workspace_roots`
    /// on the struct for the rationale. Cloned so the lock isn't
    /// held across the (potentially I/O-heavy) view build.
    async fn workspace_roots_snapshot(&self) -> Vec<std::path::PathBuf> {
        self.workspace_roots.read().await.clone()
    }

    /// Build a workspace view honoring the editor-supplied root
    /// fallback. Convenience wrapper around
    /// [`crate::workspace::build_view`].
    async fn build_view(
        &self,
        uri: &Url,
        text: &str,
    ) -> crate::workspace::WorkspaceView {
        let roots = self.workspace_roots_snapshot().await;
        crate::workspace::build_view(uri, text, &roots)
    }

    /// Resolve a `src` import path from `entry_file`'s directory,
    /// honoring the editor-supplied root fallback.
    async fn resolve_import(
        &self,
        entry_file: &std::path::Path,
        raw: &str,
    ) -> Option<std::path::PathBuf> {
        let roots = self.workspace_roots_snapshot().await;
        crate::workspace::resolve_import(entry_file, raw, &roots)
    }
}

#[async_trait]
impl LanguageBackend for HtclBackend {
    fn language_id(&self) -> &str {
        "htcl"
    }

    fn handles(&self, uri: &Url) -> bool {
        uri.path().ends_with(".htcl")
    }

    async fn set_text(&self, uri: Url, text: String) {
        self.docs.write().await.insert(uri, DocState { text });
    }

    async fn set_workspace_roots(&self, roots: Vec<std::path::PathBuf>) {
        *self.workspace_roots.write().await = roots;
    }

    async fn close(&self, uri: &Url) {
        self.docs.write().await.remove(uri);
    }

    async fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        // Parse errors are file-local: report from the open document's
        // own parse. (Imports' parse errors are diagnosed when their
        // file is the open one.)
        let parsed_local = parse(&doc.text);
        let line_index = LineIndex::new(&doc.text);
        let mut out = Vec::new();
        for err in &parsed_local.errors {
            let (start, end) = line_index.range(err.span);
            out.push(Diagnostic {
                range: Range {
                    start: lc_to_pos(start),
                    end: lc_to_pos(end),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("vw-htcl".into()),
                message: err.message.clone(),
                ..Default::default()
            });
        }

        // For validator diagnostics: validate the workspace view so
        // imported proc signatures are in scope, then keep only the
        // diagnostics that land in this file. That way calling an
        // imported proc no longer reads as "unknown proc" but a typo
        // *in* this file still does.
        let view = self.build_view(uri, &doc.text).await;
        let parsed_view = parse(&view.view_source);
        for d in validate(&parsed_view.document, &view.view_source) {
            if d.span.start >= view.local_len {
                continue;
            }
            let (start, end) = line_index.range(d.span);
            let severity = match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
            };
            out.push(Diagnostic {
                range: Range {
                    start: lc_to_pos(start),
                    end: lc_to_pos(end),
                },
                severity: Some(severity),
                source: Some("vw-htcl".into()),
                message: d.message,
                ..Default::default()
            });
        }
        out
    }

    async fn document_symbols(&self, uri: &Url) -> Vec<DocumentSymbol> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        let parsed = parse(&doc.text);
        let line_index = LineIndex::new(&doc.text);
        let mut symbols = Vec::new();
        for stmt in &parsed.document.stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            let CommandKind::Proc(proc) = &cmd.kind else {
                continue;
            };
            let name = proc.name.clone().unwrap_or_else(|| "<proc>".into());
            let (cmd_start, cmd_end) = line_index.range(cmd.span);
            let (name_start, name_end) = line_index.range(proc.name_span);
            let detail = if cmd.doc_comments.is_empty() {
                None
            } else {
                Some(cmd.doc_comments.join("\n"))
            };
            #[allow(deprecated)]
            symbols.push(DocumentSymbol {
                name,
                detail,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: Range {
                    start: lc_to_pos(cmd_start),
                    end: lc_to_pos(cmd_end),
                },
                selection_range: Range {
                    start: lc_to_pos(name_start),
                    end: lc_to_pos(name_end),
                },
                children: None,
            });
        }
        symbols
    }

    async fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        // Cap the response so a wide picker scroll doesn't pay for
        // thousands of entries when the user hasn't narrowed yet. The
        // editor applies its own scoring on top, so any reasonable
        // ceiling keeps the UX responsive.
        const MAX_RESULTS: usize = 500;

        let needle = query.to_ascii_lowercase();
        let docs = self.docs.read().await;
        // Files we've already harvested — dedupe so a header imported
        // by multiple open docs doesn't double up. Keyed on the URI as
        // a string for hashability.
        let mut seen_files: HashMap<String, ()> = HashMap::new();
        let mut out: Vec<SymbolInformation> = Vec::new();

        for (uri, doc) in docs.iter() {
            // Visit the open doc itself first, then everything it
            // transitively `src`s. `build_view` already canonicalizes
            // paths during the walk, so the import file_uris are
            // stable across docs.
            if seen_files.insert(uri.to_string(), ()).is_none() {
                collect_workspace_symbols(
                    uri,
                    &doc.text,
                    &needle,
                    &mut out,
                    MAX_RESULTS,
                );
                if out.len() >= MAX_RESULTS {
                    return out;
                }
            }

            let view = self.build_view(uri, &doc.text).await;
            for import in &view.imports {
                let key = import.file_uri.to_string();
                if seen_files.insert(key, ()).is_some() {
                    continue;
                }
                let text = &view.view_source
                    [import.start as usize..import.end as usize];
                collect_workspace_symbols(
                    &import.file_uri,
                    text,
                    &needle,
                    &mut out,
                    MAX_RESULTS,
                );
                if out.len() >= MAX_RESULTS {
                    return out;
                }
            }
        }
        out
    }

    async fn goto_definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<Location> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        let line_index = LineIndex::new(&doc.text);
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });

        // Special case: cursor on a `src @dep/foo` path → jump to the
        // imported file. Resolved through the same `vw-lib` machinery
        // the CLI uses, so editor and CLI agree on the same target.
        let parsed_local = parse(&doc.text);
        if let Some(import) = src_import_at(&parsed_local.document, offset) {
            if let Some(raw) = import.path.as_deref() {
                let Ok(file_path) = uri.to_file_path() else {
                    return Vec::new();
                };
                if let Some(resolved) =
                    self.resolve_import(&file_path, raw).await
                {
                    if let Ok(target_uri) = Url::from_file_path(resolved) {
                        return vec![Location {
                            uri: target_uri,
                            range: Range::default(),
                        }];
                    }
                }
            }
            return Vec::new();
        }

        // General case: resolve against the workspace view so calls to
        // imported procs jump to the right file.
        let view = self.build_view(uri, &doc.text).await;
        let parsed_view = parse(&view.view_source);
        let Some(target_span) =
            definition_at(&parsed_view.document, &view.view_source, offset)
        else {
            return Vec::new();
        };

        // Translate the target span back to its source file: local
        // file when in the original region, otherwise the imported
        // file whose appended region contains it.
        match view.locate(target_span.start) {
            None => {
                let (start, end) = line_index.range(target_span);
                vec![Location {
                    uri: uri.clone(),
                    range: Range {
                        start: lc_to_pos(start),
                        end: lc_to_pos(end),
                    },
                }]
            }
            Some((region, _)) => {
                // Read the imported file's text so we can build a
                // file-local line index. (Already on disk; cheap.)
                let Ok(import_path) = region.file_uri.to_file_path() else {
                    return Vec::new();
                };
                let Ok(import_text) = std::fs::read_to_string(&import_path)
                else {
                    return Vec::new();
                };
                let import_index = LineIndex::new(&import_text);
                let local_start = target_span.start - region.start;
                let local_end = target_span.end - region.start;
                let (s, e) = import_index
                    .range(vw_htcl::Span::new(local_start, local_end));
                vec![Location {
                    uri: region.file_uri.clone(),
                    range: Range {
                        start: lc_to_pos(s),
                        end: lc_to_pos(e),
                    },
                }]
            }
        }
    }

    async fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let docs = self.docs.read().await;
        let doc = docs.get(uri)?;
        let line_index = LineIndex::new(&doc.text);
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        // Use the workspace view so a hover on a call to an imported
        // proc shows that proc's signature, not nothing.
        let view = self.build_view(uri, &doc.text).await;
        let parsed = parse(&view.view_source);
        let target = hover_at(&parsed.document, &view.view_source, offset)?;
        // The hover span is in view-source coordinates; only translate
        // back to line/col when it lands in the local file (which is
        // always true for a cursor hover from this editor).
        if target.span().start >= view.local_len {
            return None;
        }
        let (start, end) = line_index.range(target.span());
        // The proc's own doc comments live on the surrounding Command,
        // not on its `Proc` payload — fetch them up here so the
        // formatters can stay focused on shape, not lookup plumbing.
        let proc_doc_comments = match &target {
            HoverTarget::ProcDef { proc, .. } => {
                proc_doc_comments_for(&parsed.document, proc)
            }
            HoverTarget::CallSite { proc_name, .. } => {
                proc_doc_comments_by_name(&parsed.document, proc_name)
            }
            _ => Vec::new(),
        };
        let markdown = format_hover(&target, &proc_doc_comments);
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(Range {
                start: lc_to_pos(start),
                end: lc_to_pos(end),
            }),
        })
    }

    async fn completion(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<CompletionItem> {
        let docs = self.docs.read().await;
        let Some(doc) = docs.get(uri) else {
            return Vec::new();
        };
        let line_index = LineIndex::new(&doc.text);
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });

        // `src <partial>` is filesystem-aware, so it takes its own
        // path before we fall back to the htcl-level analyzer.
        let line = vw_htcl::cmdline::analyze(&doc.text, offset);
        if crate::src_complete::is_src_path_context(&line) {
            if let Ok(entry_file) = uri.to_file_path() {
                let resolver = crate::workspace::build_resolver(&entry_file);
                return crate::src_complete::src_path_completions(
                    &entry_file,
                    &line,
                    &line_index,
                    &resolver,
                );
            }
        }

        // Workspace view here too: command-position completion picks
        // up imported proc names.
        let view = self.build_view(uri, &doc.text).await;
        let parsed = parse(&view.view_source);
        complete_at(&parsed.document, &view.view_source, offset)
            .into_iter()
            // The completion result's `replace` span is in view
            // coordinates; if it slipped past the local region we
            // drop it (shouldn't happen for in-file cursors, but
            // defensive).
            .filter(|c| c.replace.start < view.local_len)
            .map(|c| completion_item(c, &line_index))
            .collect()
    }

    async fn signature_help(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<SignatureHelp> {
        let docs = self.docs.read().await;
        let doc = docs.get(uri)?;
        let line_index = LineIndex::new(&doc.text);
        let offset = line_index.offset_of(LineCol {
            line: position.line,
            character: position.character,
        });
        // Workspace view so signatures of imported procs surface, and
        // so the cmdline scan can step into a `[ … ]` substitution
        // (the parser now carries a `body` inside `CmdSubst` and the
        // scan already treats `[` as a command boundary).
        let view = self.build_view(uri, &doc.text).await;
        let parsed = parse(&view.view_source);
        let help =
            signature_help_at(&parsed.document, &view.view_source, offset)?;
        Some(signature_help_response(&help))
    }
}

// --- completion / signature-help formatters -------------------------------

fn completion_item(c: Completion, line_index: &LineIndex) -> CompletionItem {
    let kind = match c.kind {
        CompletionKind::Proc => CompletionItemKind::FUNCTION,
        CompletionKind::Flag => CompletionItemKind::FIELD,
        CompletionKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
    };
    let (start, end) = line_index.range(c.replace);
    let text_edit = TextEdit {
        range: Range {
            start: lc_to_pos(start),
            end: lc_to_pos(end),
        },
        new_text: c.label.clone(),
    };
    CompletionItem {
        label: c.label,
        kind: Some(kind),
        detail: c.detail,
        documentation: c.documentation.map(|value| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        }),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(
            text_edit,
        )),
        ..Default::default()
    }
}

fn signature_help_response(help: &vw_htcl::SignatureHelp<'_>) -> SignatureHelp {
    // Build the rendered signature label and, in lockstep, the
    // [start, end) offsets each parameter occupies within it so the
    // editor highlights the active one. Names are identifiers, so
    // UTF-16 and char counts coincide.
    let mut label = help.proc_name.clone();
    let mut parameters = Vec::with_capacity(help.signature.args.len());
    for arg in &help.signature.args {
        label.push(' ');
        let start = label.chars().count() as u32;
        label.push('-');
        label.push_str(&arg.name);
        if let Some(ty) = arg.type_annotation.as_ref() {
            label.push_str(": ");
            label.push_str(&render_type(ty));
        }
        let end = label.chars().count() as u32;
        parameters.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([start, end]),
            documentation: vw_htcl::doc::brief(&arg.doc_comments)
                .map(Documentation::String),
        });
    }
    // Append the return type to the signature label when present.
    // Renders as `proc-name -arg1 -arg2 → bd_cell`.
    if let Some(ty) = help.signature.return_type.as_ref() {
        label.push_str(" → ");
        label.push_str(&render_type(ty));
    }

    let reflowed = vw_htcl::doc::reflow_doc_comments(help.doc_comments);
    let documentation = (!reflowed.is_empty()).then_some({
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: reflowed,
        })
    });

    #[allow(deprecated)] // `active_parameter` field on SignatureInformation
    let info = SignatureInformation {
        label,
        documentation,
        parameters: Some(parameters),
        active_parameter: help.active_parameter,
    };

    SignatureHelp {
        signatures: vec![info],
        active_signature: Some(0),
        active_parameter: help.active_parameter,
    }
}

// --- src import lookup ----------------------------------------------------

/// If the cursor at `offset` is on the path word of a `src <path>`
/// statement, return that import. Used by `goto_definition` to jump
/// to the imported module.
fn src_import_at(
    document: &vw_htcl::Document,
    offset: u32,
) -> Option<&vw_htcl::SrcImport> {
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Src(import) = &cmd.kind else {
            continue;
        };
        if import.path_span.contains(offset) {
            return Some(import);
        }
    }
    None
}

// --- doc-comment lookup ---------------------------------------------------

fn proc_doc_comments_for(
    document: &vw_htcl::Document,
    proc: &vw_htcl::Proc,
) -> Vec<String> {
    proc_doc_comments_for_in(&document.stmts, proc).unwrap_or_default()
}

fn proc_doc_comments_for_in(
    stmts: &[Stmt],
    proc: &vw_htcl::Proc,
) -> Option<Vec<String>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(p)
                // Pointer-identity match: `proc` was looked up out
                // of this same parse, so its address inside the AST
                // is unique.
                if std::ptr::eq(p, proc) => {
                    return Some(cmd.doc_comments.clone());
                }
            CommandKind::NamespaceEval(ns) => {
                if let Some(found) = proc_doc_comments_for_in(&ns.body, proc) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn proc_doc_comments_by_name(
    document: &vw_htcl::Document,
    name: &str,
) -> Vec<String> {
    proc_doc_comments_by_name_in(&document.stmts, "", name).unwrap_or_default()
}

fn proc_doc_comments_by_name_in(
    stmts: &[Stmt],
    prefix: &str,
    name: &str,
) -> Option<Vec<String>> {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(p) => {
                let Some(decl_name) = p.name.as_deref() else {
                    continue;
                };
                let qualified = if prefix.is_empty() {
                    decl_name.to_string()
                } else {
                    format!("{prefix}::{decl_name}")
                };
                if qualified == name {
                    return Some(cmd.doc_comments.clone());
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(ns_name) = ns.name.as_deref() else {
                    continue;
                };
                let nested = if prefix.is_empty() {
                    ns_name.to_string()
                } else {
                    format!("{prefix}::{ns_name}")
                };
                if let Some(found) =
                    proc_doc_comments_by_name_in(&ns.body, &nested, name)
                {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// Render a type expression in the canonical user-facing form —
/// `dict<string,bd_cell>`, `list<int>`, etc. Used by hover and
/// signature-help so the displayed type matches what the user
/// would write in source.
fn render_type(ty: &vw_htcl::TypeExpr) -> String {
    match ty {
        vw_htcl::TypeExpr::Named { name, .. } => name.clone(),
        vw_htcl::TypeExpr::Generic { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{name}<{}>", inner.join(","))
        }
        vw_htcl::TypeExpr::Qualified {
            namespace, variant, ..
        } => {
            format!("{namespace}::{variant}")
        }
    }
}

// --- markdown formatters --------------------------------------------------

fn format_hover(target: &HoverTarget, proc_doc_comments: &[String]) -> String {
    match target {
        HoverTarget::ProcDef { proc, .. } => format_proc(
            proc.name.as_deref().unwrap_or("<proc>"),
            proc.signature.as_ref(),
            proc_doc_comments,
        ),
        HoverTarget::CallSite {
            proc_name,
            signature,
            ..
        } => format_proc(proc_name, Some(signature), proc_doc_comments),
        HoverTarget::ProcArgDef { arg, .. }
        | HoverTarget::CallArg { arg, .. } => format_arg(arg),
        HoverTarget::LocalVar { name, .. } => format_local_var(name),
        HoverTarget::EnumDef { decl, .. } => format_enum(decl),
    }
}

fn format_enum(decl: &vw_htcl::EnumDecl) -> String {
    let mut out = String::new();
    let name = decl.name.as_deref().unwrap_or("<enum>");
    writeln!(out, "```htcl").unwrap();
    writeln!(out, "enum {name} = {{").unwrap();
    for v in &decl.variants {
        match v.payload.as_ref() {
            Some(p) => {
                writeln!(out, "  {}: {}", v.name, render_type(p)).unwrap()
            }
            None => writeln!(out, "  {}", v.name).unwrap(),
        }
    }
    writeln!(out, "}}").unwrap();
    writeln!(out, "```").unwrap();
    out.push_str("\nTagged sum type. The compiler auto-generates ");
    out.push_str("constructors (`<Enum>::<Variant>`), repr, and ");
    out.push_str("`tag`/`payload` accessors. See ");
    out.push_str("docs/htcl-enums.md for the full semantics.\n");
    out
}

fn format_local_var(name: &str) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    writeln!(out, "${name}").unwrap();
    writeln!(out, "```").unwrap();
    out.push_str("\nLocal variable.\n");
    out
}

fn format_proc(
    name: &str,
    signature: Option<&ProcSignature>,
    proc_doc_comments: &[String],
) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    // Include the return type in the proc header when annotated:
    //   proc foo → string
    // Unannotated procs render unchanged (`proc foo`).
    let return_ty = signature.and_then(|s| s.return_type.as_ref());
    match return_ty {
        Some(ty) => {
            writeln!(out, "proc {name} → {}", render_type(ty)).unwrap();
        }
        None => {
            writeln!(out, "proc {name}").unwrap();
        }
    }
    writeln!(out, "```").unwrap();
    let reflowed = vw_htcl::doc::reflow_doc_comments(proc_doc_comments);
    if !reflowed.is_empty() {
        out.push('\n');
        out.push_str(&reflowed);
        out.push('\n');
    }
    if let Some(sig) = signature {
        if !sig.args.is_empty() {
            out.push_str("\n### Parameters\n\n");
            for arg in &sig.args {
                match arg.type_annotation.as_ref() {
                    Some(ty) => {
                        write!(out, "- `-{}: {}`", arg.name, render_type(ty))
                            .unwrap();
                    }
                    None => {
                        write!(out, "- `-{}`", arg.name).unwrap();
                    }
                }
                let reflowed =
                    vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
                let mut paragraphs = reflowed.split("\n\n");
                if let Some(brief) = paragraphs.next().filter(|s| !s.is_empty())
                {
                    write!(out, " — {brief}").unwrap();
                }
                out.push('\n');
                for extra in paragraphs.filter(|s| !s.is_empty()) {
                    writeln!(out, "  {extra}").unwrap();
                }
                for attr in &arg.attributes {
                    writeln!(out, "  - `{}`", format_attribute(attr)).unwrap();
                }
            }
        }
    }
    out
}

fn format_arg(arg: &ProcArg) -> String {
    let mut out = String::new();
    writeln!(out, "```htcl").unwrap();
    match arg.type_annotation.as_ref() {
        Some(ty) => {
            writeln!(out, "-{}: {}", arg.name, render_type(ty)).unwrap()
        }
        None => writeln!(out, "-{}", arg.name).unwrap(),
    }
    writeln!(out, "```").unwrap();
    let reflowed = vw_htcl::doc::reflow_doc_comments(&arg.doc_comments);
    if !reflowed.is_empty() {
        out.push('\n');
        out.push_str(&reflowed);
        out.push('\n');
    }
    if !arg.attributes.is_empty() {
        out.push('\n');
        for attr in &arg.attributes {
            writeln!(out, "- `{}`", format_attribute(attr)).unwrap();
        }
    }
    out
}

fn format_attribute(attr: &Attribute) -> String {
    if attr.values.is_empty() {
        format!("@{}", attr.name)
    } else {
        let values: Vec<String> =
            attr.values.iter().map(format_attribute_value).collect();
        format!("@{}({})", attr.name, values.join(", "))
    }
}

fn format_attribute_value(v: &AttributeValue) -> String {
    match v {
        AttributeValue::Integer { value, .. } => value.to_string(),
        AttributeValue::Ident { value, .. } => value.clone(),
        AttributeValue::String { value, .. } => format!("\"{value}\""),
    }
}

fn lc_to_pos(lc: LineCol) -> Position {
    Position {
        line: lc.line,
        character: lc.character,
    }
}

/// Parse one htcl `text` and push every `proc` / `type` / `enum`
/// declaration whose name contains `needle` (case-insensitive, empty
/// `needle` matches all) into `out`. Stops as soon as `out` reaches
/// `cap` entries so a `workspace/symbol` request never assembles an
/// unbounded response. Variants of an enum are emitted as siblings
/// with `container_name` set to the enum, matching how
/// rust-analyzer surfaces variants in the workspace picker.
fn collect_workspace_symbols(
    uri: &Url,
    text: &str,
    needle: &str,
    out: &mut Vec<SymbolInformation>,
    cap: usize,
) {
    let parsed = parse(text);
    let line_index = LineIndex::new(text);
    for stmt in &parsed.document.stmts {
        if out.len() >= cap {
            return;
        }
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                if let Some(name) = proc.name.as_deref() {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        proc.name_span,
                        SymbolKind::FUNCTION,
                        None,
                        needle,
                        out,
                    );
                }
            }
            CommandKind::TypeDecl(td) => {
                if let Some(name) = td.name.as_deref() {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        td.name_span,
                        SymbolKind::STRUCT,
                        None,
                        needle,
                        out,
                    );
                }
            }
            CommandKind::EnumDecl(ed) => {
                let enum_name = ed.name.as_deref();
                if let Some(name) = enum_name {
                    push_symbol(
                        uri,
                        &line_index,
                        name,
                        ed.name_span,
                        SymbolKind::ENUM,
                        None,
                        needle,
                        out,
                    );
                }
                for v in &ed.variants {
                    if out.len() >= cap {
                        return;
                    }
                    push_symbol(
                        uri,
                        &line_index,
                        &v.name,
                        v.name_span,
                        SymbolKind::ENUM_MEMBER,
                        enum_name.map(str::to_string),
                        needle,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_symbol(
    uri: &Url,
    line_index: &LineIndex,
    name: &str,
    span: vw_htcl::Span,
    kind: SymbolKind,
    container_name: Option<String>,
    needle: &str,
    out: &mut Vec<SymbolInformation>,
) {
    if !needle.is_empty() && !name.to_ascii_lowercase().contains(needle) {
        return;
    }
    let (start, end) = line_index.range(span);
    #[allow(deprecated)]
    out.push(SymbolInformation {
        name: name.to_string(),
        kind,
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: Range {
                start: lc_to_pos(start),
                end: lc_to_pos(end),
            },
        },
        container_name,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///tmp/x.htcl").unwrap()
    }

    #[tokio::test]
    async fn handles_htcl_extension() {
        let backend = HtclBackend::new();
        assert!(backend.handles(&uri()));
        assert!(!backend.handles(&Url::parse("file:///tmp/x.vhd").unwrap()));
    }

    #[tokio::test]
    async fn diagnostics_for_unterminated_string() {
        let backend = HtclBackend::new();
        backend
            .set_text(uri(), "puts \"oops\nputs ok\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diags[0].message.contains("unterminated string"));
    }

    #[tokio::test]
    async fn document_symbols_include_proc() {
        let backend = HtclBackend::new();
        backend
            .set_text(
                uri(),
                "## greet someone\nproc greet {name} { puts hi }\n".into(),
            )
            .await;
        let symbols = backend.document_symbols(&uri()).await;
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "greet");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(symbols[0].detail.as_deref(), Some("greet someone"));
    }

    #[tokio::test]
    async fn workspace_symbols_surface_procs_types_and_enum_variants() {
        let backend = HtclBackend::new();
        backend
            .set_text(
                uri(),
                "proc greet {name} { puts hi }\n\
                 type Foo = int\n\
                 enum Color = {\n  Red\n  Green\n  Blue\n}\n"
                    .into(),
            )
            .await;
        let all = backend.workspace_symbols("").await;
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "{names:?}");
        assert!(names.contains(&"Foo"), "{names:?}");
        assert!(names.contains(&"Color"), "{names:?}");
        assert!(names.contains(&"Red"), "{names:?}");
        let red = all.iter().find(|s| s.name == "Red").unwrap();
        assert_eq!(red.kind, SymbolKind::ENUM_MEMBER);
        assert_eq!(red.container_name.as_deref(), Some("Color"));

        // Substring filter, case-insensitive.
        let filtered = backend.workspace_symbols("gre").await;
        assert!(filtered.iter().any(|s| s.name == "greet"));
        assert!(filtered.iter().any(|s| s.name == "Green"));
        assert!(!filtered.iter().any(|s| s.name == "Foo"));
    }

    #[tokio::test]
    async fn validator_diagnostics_surface_in_lsp() {
        let backend = HtclBackend::new();
        backend
            .set_text(
                uri(),
                "proc axis {\n  @enum(1, 2, 4) width\n} { puts $width }\n\
                 axis -width 3\n"
                    .into(),
            )
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(
            diags.iter().any(|d| d.message.contains("@enum")),
            "{:?}",
            diags
        );
    }

    /// Unused-variable warnings from the `vw-htcl::unused` pass
    /// reach LSP clients with `DiagnosticSeverity::WARNING` and
    /// point at the offending decl. Underscore-prefixed names are
    /// exempt.
    #[tokio::test]
    async fn unused_var_warning_surfaces_in_lsp() {
        let backend = HtclBackend::new();
        backend
            .set_text(uri(), "proc f {unused_arg} { return 1 }\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
            .filter(|d| d.message.contains("unused proc arg"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", diags);
        assert!(
            warnings[0].message.contains("unused_arg"),
            "{:?}",
            warnings[0]
        );
    }

    #[tokio::test]
    async fn unused_var_underscore_prefix_suppresses_lsp_warning() {
        let backend = HtclBackend::new();
        backend
            .set_text(uri(), "proc f {_ignored} { return 1 }\n".into())
            .await;
        let diags = backend.diagnostics(&uri()).await;
        assert!(
            !diags.iter().any(|d| d.message.contains("unused")),
            "{:?}",
            diags
        );
    }

    #[tokio::test]
    async fn hover_on_call_site_shows_signature() {
        let backend = HtclBackend::new();
        let src = "\
## Greet someone by name.\n\
proc greet {\n\
  ## Who to greet.\n\
  @default(\"world\") name\n\
} { puts \"hi $name\" }\n\
greet -name there\n";
        backend.set_text(uri(), src.into()).await;
        // Cursor on the `g` of the call-site `greet`. Line indices
        // are 0-based.
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 5,
                    character: 0,
                },
            )
            .await
            .expect("hover should return content");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(body.contains("proc greet"), "{body}");
        assert!(body.contains("Greet someone by name."), "{body}");
        assert!(body.contains("### Parameters"), "{body}");
        assert!(body.contains("-name"), "{body}");
        assert!(body.contains("Who to greet."), "{body}");
        assert!(body.contains("@default"), "{body}");
    }

    #[tokio::test]
    async fn hover_on_call_arg_shows_arg_doc() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {\n\
  ## Who to greet.\n\
  @default(\"world\") name\n\
} { puts hi }\n\
greet -name there\n";
        backend.set_text(uri(), src.into()).await;
        // Position cursor on `-name` of the call site (line 4 in the
        // 0-indexed scheme).
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 4,
                    character: 7,
                },
            )
            .await
            .expect("hover should return content");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(body.contains("-name"), "{body}");
        assert!(body.contains("Who to greet."), "{body}");
        assert!(body.contains("@default"), "{body}");
        // Shouldn't include the proc-level header.
        assert!(!body.contains("### Parameters"), "{body}");
    }

    #[tokio::test]
    async fn hover_outside_known_construct_returns_none() {
        let backend = HtclBackend::new();
        backend.set_text(uri(), "puts hello world\n".into()).await;
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(hover.is_none());
    }

    #[tokio::test]
    async fn goto_definition_jumps_call_to_proc_decl() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {\n  name\n} { puts hi }\n\
greet -name there\n";
        backend.set_text(uri(), src.into()).await;
        // Cursor on the `g` of the call-site `greet` (line 3).
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 3,
                    character: 0,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        // Decl name `greet` is on line 0 at character 5.
        assert_eq!(locs[0].range.start.line, 0);
        assert_eq!(locs[0].range.start.character, 5);
    }

    #[tokio::test]
    async fn goto_definition_resolves_attribute_ident() {
        let backend = HtclBackend::new();
        let src = "\
proc f {\n  has_a\n  @requires(has_a) has_b\n} { }\n";
        backend.set_text(uri(), src.into()).await;
        // Cursor on `has_a` inside `@requires(has_a)`.
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 2,
                    character: 13,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        // Decl `has_a` is on line 1 at character 2.
        assert_eq!(locs[0].range.start.line, 1);
        assert_eq!(locs[0].range.start.character, 2);
    }

    #[tokio::test]
    async fn completion_offers_proc_names_in_command_position() {
        let backend = HtclBackend::new();
        let src = "\
proc greet {} { }\n\
proc grumble {} { }\n\
gr\n";
        backend.set_text(uri(), src.into()).await;
        // Cursor at end of `gr` on line 2.
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 2,
                    character: 2,
                },
            )
            .await;
        let mut labels: Vec<String> =
            items.iter().map(|i| i.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["greet", "grumble"]);
        assert_eq!(items[0].kind, Some(CompletionItemKind::FUNCTION));
    }

    #[tokio::test]
    async fn completion_offers_flags_in_argument_position() {
        let backend = HtclBackend::new();
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg \n";
        backend.set_text(uri(), src.into()).await;
        // Line 4, just after `cfg ` (character 4).
        let items = backend
            .completion(
                &uri(),
                Position {
                    line: 4,
                    character: 4,
                },
            )
            .await;
        let mut labels: Vec<String> =
            items.iter().map(|i| i.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["-depth", "-width"]);
        assert_eq!(items[0].kind, Some(CompletionItemKind::FIELD));
    }

    #[tokio::test]
    async fn signature_help_highlights_active_parameter() {
        let backend = HtclBackend::new();
        let src = "\
## Configure the bus.\n\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -depth \n";
        backend.set_text(uri(), src.into()).await;
        // Line 5, after `cfg -depth ` (character 11).
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 5,
                    character: 11,
                },
            )
            .await
            .expect("signature help expected");
        assert_eq!(help.active_parameter, Some(1));
        let info = &help.signatures[0];
        assert!(info.label.starts_with("cfg "), "{}", info.label);
        assert_eq!(info.parameters.as_ref().unwrap().len(), 2);
        match &info.documentation {
            Some(Documentation::MarkupContent(m)) => {
                assert!(m.value.contains("Configure the bus."), "{}", m.value);
            }
            other => panic!("expected markup documentation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn signature_help_includes_return_type_arrow() {
        let backend = HtclBackend::new();
        let src = "\
proc make_widget {} bd_cell { return foo }\n\
make_widget \n";
        backend.set_text(uri(), src.into()).await;
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await
            .expect("signature help expected");
        let info = &help.signatures[0];
        // Label should carry the `→ bd_cell` suffix.
        assert!(info.label.contains("→ bd_cell"), "{}", info.label);
    }

    #[tokio::test]
    async fn hover_on_enum_decl_shows_variants() {
        let backend = HtclBackend::new();
        let src = "\
enum Property = {\n  Scalar: string\n  Nested: int\n}\n";
        backend.set_text(uri(), src.into()).await;
        // Cursor on the enum name (line 0, col 5: 'Property').
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 0,
                    character: 7,
                },
            )
            .await
            .expect("hover on enum decl name");
        if let HoverContents::Markup(MarkupContent { value, .. }) =
            hover.contents
        {
            assert!(value.contains("enum Property"), "{value}");
            assert!(value.contains("Scalar: string"), "{value}");
            assert!(value.contains("Nested: int"), "{value}");
        } else {
            panic!("expected Markup hover");
        }
    }

    #[tokio::test]
    async fn hover_proc_def_includes_return_type() {
        let backend = HtclBackend::new();
        let src = "\
## Builds a widget.\n\
proc make_widget {} dict<string,bd_cell> { return {} }\n";
        backend.set_text(uri(), src.into()).await;
        // Hover on the proc name `make_widget` at line 1.
        let hover = backend
            .hover(
                &uri(),
                Position {
                    line: 1,
                    character: 8,
                },
            )
            .await
            .expect("hover expected on proc def");
        if let HoverContents::Markup(MarkupContent { value, .. }) =
            hover.contents
        {
            assert!(
                value.contains("→ dict<string,bd_cell>"),
                "expected return type in hover: {value}"
            );
        } else {
            panic!("expected Markup hover contents");
        }
    }

    #[tokio::test]
    async fn signature_help_none_outside_call() {
        let backend = HtclBackend::new();
        backend.set_text(uri(), "puts hi\n".into()).await;
        let help = backend
            .signature_help(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(help.is_none());
    }

    #[tokio::test]
    async fn goto_definition_unknown_returns_empty() {
        let backend = HtclBackend::new();
        backend.set_text(uri(), "puts hello\n".into()).await;
        let locs = backend
            .goto_definition(
                &uri(),
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await;
        assert!(locs.is_empty());
    }

    // --- cross-file (workspace view) tests --------------------------------

    /// Build a temp workspace with a `lib.htcl` defining `greet` and
    /// a `main.htcl` that imports it. Returns the backend with both
    /// files already opened and the URIs.
    async fn temp_workspace_with_import() -> (
        tempfile::TempDir,
        HtclBackend,
        Url, // main.htcl
        Url, // lib.htcl
    ) {
        let dir = tempfile::tempdir().unwrap();
        let lib_path = dir.path().join("lib.htcl");
        std::fs::write(
            &lib_path,
            "## Greet someone.\n\
proc greet {\n  ## Who to greet.\n  who\n} { puts \"hi $who\" }\n",
        )
        .unwrap();
        let main_path = dir.path().join("main.htcl");
        let main_src = "src lib\ngreet -who world\n";
        std::fs::write(&main_path, main_src).unwrap();

        let backend = HtclBackend::new();
        let main_uri = Url::from_file_path(&main_path).unwrap();
        let lib_uri = Url::from_file_path(&lib_path).unwrap();
        backend.set_text(main_uri.clone(), main_src.into()).await;
        (dir, backend, main_uri, lib_uri)
    }

    #[tokio::test]
    async fn goto_on_src_import_jumps_to_imported_file() {
        let (_dir, backend, main_uri, lib_uri) =
            temp_workspace_with_import().await;
        // Cursor on the `l` of `src lib` (line 0, col 4).
        let locs = backend
            .goto_definition(
                &main_uri,
                Position {
                    line: 0,
                    character: 4,
                },
            )
            .await;
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, lib_uri);
    }

    #[tokio::test]
    async fn goto_on_call_to_imported_proc_jumps_to_lib() {
        let (_dir, backend, main_uri, lib_uri) =
            temp_workspace_with_import().await;
        // Cursor on `greet` at line 1.
        let locs = backend
            .goto_definition(
                &main_uri,
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .await;
        assert_eq!(locs.len(), 1, "{locs:?}");
        assert_eq!(locs[0].uri, lib_uri);
        // The declaration of `greet` is on lib.htcl line 1 col 5.
        assert_eq!(locs[0].range.start.line, 1);
        assert_eq!(locs[0].range.start.character, 5);
    }

    /// Regression: a call from inside an `if { … }` body should
    /// still find its proc's declaration. The parser leaves the
    /// brace-body as an opaque word, so without an explicit
    /// reparse pass in [`vw_htcl::goto`] the search never reaches
    /// the nested call.
    #[tokio::test]
    async fn goto_from_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.htcl");
        let src = "proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   target -x 1\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text(uri.clone(), src.into()).await;
        let locs = backend
            .goto_definition(
                &uri,
                Position {
                    line: 3,
                    character: 4,
                },
            )
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def from inside `if {{…}}` body failed"
        );
    }

    /// Regression: a call from inside `[…]` command substitution
    /// inside `if {…} { … }` — the double-nested shape the IP
    /// wrapper's `if {$bd} { set cell [create_bd_cell …] }
    /// else { set cell [create_ip …] }` scaffold produces. The
    /// reparse pass has to also run `populate_procs` so the
    /// inner CmdSubst.body gets filled in.
    #[tokio::test]
    async fn goto_from_cmdsubst_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.htcl");
        let src = "proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   set cell [target -x 1]\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text(uri.clone(), src.into()).await;
        // Cursor on `target` inside `[target -x 1]` on line 3.
        // Line 3 is `    set cell [target -x 1]`; `target` starts
        // at col 17.
        let locs = backend
            .goto_definition(
                &uri,
                Position {
                    line: 3,
                    character: 17,
                },
            )
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def from inside `[[…]]`-inside-`if` failed"
        );
    }

    /// Companion to [`goto_from_cmdsubst_inside_if_body`] for hover.
    #[tokio::test]
    async fn hover_from_cmdsubst_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.htcl");
        let src = "## Target proc doc.\n\
                   proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   set cell [target -x 1]\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text(uri.clone(), src.into()).await;
        // Cursor on `target` inside `[target -x 1]` on line 4.
        let hover = backend
            .hover(
                &uri,
                Position {
                    line: 4,
                    character: 17,
                },
            )
            .await;
        assert!(
            hover.is_some(),
            "hover from inside `[[…]]`-inside-`if` returned None"
        );
    }

    /// Same regression as [`goto_from_inside_if_body`], but for
    /// hover — the two share the "reparse brace-body" fix in
    /// [`vw_htcl::goto`] / [`vw_htcl::hover`].
    #[tokio::test]
    async fn hover_from_inside_if_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.htcl");
        let src = "## Target proc doc.\n\
                   proc target { x } { }\n\
                   proc caller { } {\n  \
                   if {1} {\n    \
                   target -x 1\n  \
                   }\n\
                   }\n";
        std::fs::write(&path, src).unwrap();
        let backend = HtclBackend::new();
        let uri = Url::from_file_path(&path).unwrap();
        backend.set_text(uri.clone(), src.into()).await;
        let hover = backend
            .hover(
                &uri,
                Position {
                    line: 4,
                    character: 4,
                },
            )
            .await;
        assert!(
            hover.is_some(),
            "hover from inside `if {{…}}` body returned None"
        );
    }

    /// Reproduces the exact user scenario against the on-disk
    /// `~/src/htcl/amd/` tree. Only runs when that path exists, so
    /// the test is a no-op in CI / fresh checkouts.
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep_real_htcl_tree() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            eprintln!("skipping — {} not present", cpm5_module.display());
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text(cpm5_uri.clone(), text.clone()).await;

        // Find the line + column of `vivado_cmd::set_property` —
        // avoids hard-coding a line number that will drift as the
        // wrapper regenerates.
        let mut target_line = None;
        for (i, line) in text.lines().enumerate() {
            if let Some(col) = line.find("vivado_cmd::set_property") {
                // Cursor on the `set_property` word, past the
                // `vivado_cmd::` prefix (12 chars).
                target_line = Some((i as u32, (col + 12) as u32));
                break;
            }
        }
        let Some((line, character)) = target_line else {
            panic!("no `vivado_cmd::set_property` in cpm5/module.htcl");
        };
        let locs = backend
            .goto_definition(&cpm5_uri, Position { line, character })
            .await;
        assert!(
            !locs.is_empty(),
            "goto-def against real htcl tree returned no location \
             for cpm5/module.htcl:{line}:{character}"
        );
        let hit = &locs[0];
        let path = hit.uri.to_file_path().unwrap();
        assert!(
            path.to_string_lossy().contains("vivado-cmd"),
            "expected to land in the vivado-cmd tree, got {:?}",
            hit
        );
    }

    /// Regression against the on-disk cpm5 tree for BOTH goto and
    /// hover on `vivado_cmd::create_bd_cell` and
    /// `vivado_cmd::create_ip` — the two `[…]` calls inside the
    /// `if {$bd} { … } else { … }` scaffold at the top of
    /// `create_cpm5`.
    #[tokio::test]
    async fn goto_and_hover_on_create_bd_cell_and_create_ip_in_cpm5() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text(cpm5_uri.clone(), text.clone()).await;
        for needle in &["vivado_cmd::create_bd_cell", "vivado_cmd::create_ip"] {
            let (line, character) = text
                .lines()
                .enumerate()
                .find_map(|(i, l)| {
                    l.find(needle).map(|c| (i as u32, (c + 12) as u32))
                })
                .unwrap_or_else(|| panic!("no {needle} in cpm5/module.htcl"));
            let locs = backend
                .goto_definition(&cpm5_uri, Position { line, character })
                .await;
            assert!(
                !locs.is_empty(),
                "goto-def on {needle} at {line}:{character} returned nothing"
            );
            let hover =
                backend.hover(&cpm5_uri, Position { line, character }).await;
            assert!(
                hover.is_some(),
                "hover on {needle} at {line}:{character} returned None"
            );
        }
    }

    /// Companion to [`goto_finds_sibling_workspace_dep_real_htcl_tree`]
    /// for hover — same file, same cursor position, same expected
    /// outcome: the imported proc's signature resolves and hover
    /// returns something rather than `None`.
    #[tokio::test]
    async fn hover_finds_imported_proc_real_htcl_tree() {
        let cpm5_module =
            std::path::PathBuf::from("/home/ry/src/htcl/amd/cpm5/module.htcl");
        if !cpm5_module.exists() {
            return;
        }
        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        let text = std::fs::read_to_string(&cpm5_module).unwrap();
        backend.set_text(cpm5_uri.clone(), text.clone()).await;
        let target = text.lines().enumerate().find_map(|(i, line)| {
            line.find("vivado_cmd::set_property")
                .map(|col| (i as u32, (col + 12) as u32))
        });
        let Some((line, character)) = target else {
            panic!("no `vivado_cmd::set_property` in cpm5/module.htcl");
        };
        let hover =
            backend.hover(&cpm5_uri, Position { line, character }).await;
        assert!(
            hover.is_some(),
            "hover against real htcl tree returned None \
             for cpm5/module.htcl:{line}:{character}"
        );
    }

    /// Sibling-workspace fallback with a NESTED src chain — mirrors
    /// the real vivado-cmd layout where `module.htcl` re-sources
    /// per-command files under `cmd/`. `set_property` doesn't live
    /// in the module.htcl entry directly; it's reached through
    /// `src "cmd/set_property.htcl"` inside the dep module. This
    /// caught the actual reproduction case where a shallower test
    /// (proc in the dep's module.htcl) passed but goto-def against
    /// the real vivado-cmd tree still returned nothing.
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep_via_nested_src() {
        let dir = tempfile::tempdir().unwrap();
        let amd = dir.path().join("amd");
        let cpm5 = amd.join("cpm5");
        let vivado_cmd = amd.join("vivado-cmd");
        let vivado_cmd_cmd = vivado_cmd.join("cmd");
        std::fs::create_dir_all(&cpm5).unwrap();
        std::fs::create_dir_all(&vivado_cmd_cmd).unwrap();
        std::fs::write(
            cpm5.join("vw.toml"),
            "[workspace]\nname=\"cpm5\"\nversion=\"0.1.0\"\n\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(
            vivado_cmd.join("vw.toml"),
            "[workspace]\nname=\"vivado-cmd\"\nversion=\"0.1.0\"\n\n\
             [dependencies]\n",
        )
        .unwrap();
        // vivado-cmd/module.htcl re-sources set_property.htcl —
        // matching the real layout.
        std::fs::write(
            vivado_cmd.join("module.htcl"),
            "src \"cmd/set_property.htcl\"\n",
        )
        .unwrap();
        // vivado-cmd/cmd/set_property.htcl defines the proc.
        let set_property_path = vivado_cmd_cmd.join("set_property.htcl");
        std::fs::write(
            &set_property_path,
            "namespace eval vivado_cmd {\n  \
                proc set_property { args } { }\n}\n",
        )
        .unwrap();
        let cpm5_module = cpm5.join("module.htcl");
        std::fs::write(
            &cpm5_module,
            "src @vivado-cmd\nvivado_cmd::set_property -dict {} -objects x\n",
        )
        .unwrap();

        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        backend
            .set_text(
                cpm5_uri.clone(),
                std::fs::read_to_string(&cpm5_module).unwrap(),
            )
            .await;
        // Cursor on `set_property` — the call is
        // `vivado_cmd::set_property ...` (col 0). `vivado_cmd::`
        // is 12 chars; `set_property` starts at col 12.
        let locs = backend
            .goto_definition(
                &cpm5_uri,
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await;
        assert!(!locs.is_empty(), "goto-def returned no location");
        let set_property_uri = Url::from_file_path(&set_property_path).unwrap();
        assert_eq!(
            locs[0].uri, set_property_uri,
            "expected jump to {set_property_uri}, got {:?}",
            locs[0]
        );
    }

    /// Sibling-workspace fallback: when a file's own workspace
    /// doesn't declare a `@dep/…` import but a sibling directory
    /// under a shared parent DOES have its own `vw.toml` with a
    /// matching basename, the resolver should still find it.
    ///
    /// Layout (mirrors `~/src/htcl/amd/{cpm5,vivado-cmd}` as the
    /// user's actual reproduction):
    ///
    ///   <tmp>/amd/cpm5/vw.toml            # empty deps
    ///        /amd/cpm5/module.htcl        # calls vivado_cmd::foo
    ///        /amd/vivado-cmd/vw.toml
    ///        /amd/vivado-cmd/module.htcl  # namespace eval vivado_cmd { proc foo … }
    ///
    /// Regression for "goto-def returns 'No definition found' once
    /// I'm in a vw-tracked dependency."
    #[tokio::test]
    async fn goto_finds_sibling_workspace_dep() {
        let dir = tempfile::tempdir().unwrap();
        let amd = dir.path().join("amd");
        let cpm5 = amd.join("cpm5");
        let vivado_cmd = amd.join("vivado-cmd");
        std::fs::create_dir_all(&cpm5).unwrap();
        std::fs::create_dir_all(&vivado_cmd).unwrap();
        std::fs::write(
            cpm5.join("vw.toml"),
            "[workspace]\nname=\"cpm5\"\nversion=\"0.1.0\"\n\n[dependencies]\n",
        )
        .unwrap();
        std::fs::write(
            vivado_cmd.join("vw.toml"),
            "[workspace]\nname=\"vivado-cmd\"\nversion=\"0.1.0\"\n\n\
             [dependencies]\n",
        )
        .unwrap();
        // The vivado-cmd module: define namespace `vivado_cmd` with
        // a `foo` proc so a call to `vivado_cmd::foo` from cpm5 has
        // somewhere to land.
        let vivado_module = vivado_cmd.join("module.htcl");
        std::fs::write(
            &vivado_module,
            "namespace eval vivado_cmd {\n  proc foo { x } { }\n}\n",
        )
        .unwrap();
        let cpm5_module = cpm5.join("module.htcl");
        std::fs::write(&cpm5_module, "src @vivado-cmd\nvivado_cmd::foo -x 1\n")
            .unwrap();

        let backend = HtclBackend::new();
        let cpm5_uri = Url::from_file_path(&cpm5_module).unwrap();
        backend
            .set_text(
                cpm5_uri.clone(),
                std::fs::read_to_string(&cpm5_module).unwrap(),
            )
            .await;

        // Cursor on `foo` — line 1, at the start of the call word
        // (`vivado_cmd::foo` starts at column 0, `foo` starts after
        // `vivado_cmd::` which is 12 chars).
        let locs = backend
            .goto_definition(
                &cpm5_uri,
                Position {
                    line: 1,
                    character: 12,
                },
            )
            .await;
        assert!(!locs.is_empty(), "goto-def returned no location");
        let vivado_uri = Url::from_file_path(&vivado_module).unwrap();
        assert_eq!(locs[0].uri, vivado_uri, "landed on wrong file");
    }

    #[tokio::test]
    async fn completion_in_command_position_lists_imported_procs() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Append a partial proc name at end of file so cursor lands in
        // command position.
        let new_text = "src lib\ngreet -who world\ngre\n";
        backend.set_text(main_uri.clone(), new_text.into()).await;
        let items = backend
            .completion(
                &main_uri,
                Position {
                    line: 2,
                    character: 3,
                },
            )
            .await;
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"greet"), "labels = {labels:?}");
    }

    #[tokio::test]
    async fn hover_on_imported_call_shows_signature() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Hover on `greet` on line 1.
        let hover = backend
            .hover(
                &main_uri,
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .await
            .expect("hover");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!(),
        };
        assert!(body.contains("proc greet"), "{body}");
        assert!(body.contains("Greet someone."), "{body}");
        assert!(body.contains("-who"), "{body}");
    }

    #[tokio::test]
    async fn diagnostics_accept_calls_to_imported_procs() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // No errors when the call matches the imported signature.
        let diags = backend.diagnostics(&main_uri).await;
        let errs: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[tokio::test]
    async fn hover_works_on_call_inside_command_substitution() {
        // Mirrors the user's cips.htcl shape:
        //   src lib
        //   set cell [greet -who x]
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        let new_text = "src lib\nset cell [greet -who x]\n";
        backend.set_text(main_uri.clone(), new_text.into()).await;
        // Cursor on `greet` inside the `[ … ]` on line 1.
        let hover = backend
            .hover(
                &main_uri,
                Position {
                    line: 1,
                    character: 11,
                },
            )
            .await
            .expect("hover should resolve calls inside `[…]`");
        let body = match hover.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!(),
        };
        assert!(body.contains("proc greet"), "{body}");
    }

    #[tokio::test]
    async fn signature_help_works_on_call_inside_command_substitution() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        // Cursor right after `greet ` inside `[ … ]`.
        let new_text = "src lib\nset cell [greet ]\n";
        backend.set_text(main_uri.clone(), new_text.into()).await;
        let help = backend
            .signature_help(
                &main_uri,
                Position {
                    line: 1,
                    character: 16,
                },
            )
            .await
            .expect("signature help inside `[…]`");
        assert!(
            help.signatures[0].label.starts_with("greet"),
            "{:?}",
            help.signatures[0].label
        );
    }

    #[tokio::test]
    async fn diagnostics_still_flag_wrong_flag_on_imported_call() {
        let (_dir, backend, main_uri, _lib_uri) =
            temp_workspace_with_import().await;
        backend
            .set_text(main_uri.clone(), "src lib\ngreet -whoz world\n".into())
            .await;
        let diags = backend.diagnostics(&main_uri).await;
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("undefined argument -whoz")),
            "{diags:?}"
        );
    }
}
