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
    Position, Range, SignatureHelp, SignatureInformation, SymbolKind, TextEdit,
    Url,
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
}

struct DocState {
    text: String,
}

impl HtclBackend {
    pub fn new() -> Self {
        Self::default()
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
        let view = crate::workspace::build_view(uri, &doc.text);
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
                    crate::workspace::resolve_import(&file_path, raw)
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
        let view = crate::workspace::build_view(uri, &doc.text);
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
        let view = crate::workspace::build_view(uri, &doc.text);
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
        let view = crate::workspace::build_view(uri, &doc.text);
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
        let view = crate::workspace::build_view(uri, &doc.text);
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
        let end = label.chars().count() as u32;
        parameters.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([start, end]),
            documentation: arg
                .doc_comments
                .first()
                .map(|d| Documentation::String(d.clone())),
        });
    }

    let documentation = (!help.doc_comments.is_empty()).then(|| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: help.doc_comments.join("\n"),
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
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(p) = &cmd.kind else {
            continue;
        };
        // Pointer-identity match: `proc` was looked up out of this
        // same parse, so its address inside the AST is unique.
        if std::ptr::eq(p, proc) {
            return cmd.doc_comments.clone();
        }
    }
    Vec::new()
}

fn proc_doc_comments_by_name(
    document: &vw_htcl::Document,
    name: &str,
) -> Vec<String> {
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(p) = &cmd.kind else {
            continue;
        };
        if p.name.as_deref() == Some(name) {
            return cmd.doc_comments.clone();
        }
    }
    Vec::new()
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
    }
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
    writeln!(out, "proc {name}").unwrap();
    writeln!(out, "```").unwrap();
    if !proc_doc_comments.is_empty() {
        out.push('\n');
        for line in proc_doc_comments {
            writeln!(out, "{line}").unwrap();
        }
    }
    if let Some(sig) = signature {
        if !sig.args.is_empty() {
            out.push_str("\n### Parameters\n\n");
            for arg in &sig.args {
                write!(out, "- `-{}`", arg.name).unwrap();
                if let Some(brief) = arg.doc_comments.first() {
                    write!(out, " — {brief}").unwrap();
                }
                out.push('\n');
                for extra in arg.doc_comments.iter().skip(1) {
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
    writeln!(out, "-{}", arg.name).unwrap();
    writeln!(out, "```").unwrap();
    if !arg.doc_comments.is_empty() {
        out.push('\n');
        for line in &arg.doc_comments {
            writeln!(out, "{line}").unwrap();
        }
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
    async fn validator_diagnostics_surface_in_lsp() {
        let backend = HtclBackend::new();
        backend
            .set_text(
                uri(),
                "proc axis {\n  @enum(1, 2, 4) width\n} { }\n\
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
