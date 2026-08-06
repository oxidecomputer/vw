// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Catalog of every named thing the session knows about — procs
//! (including overload dispatchers), type aliases, enum
//! declarations and their variants. Each entry carries the
//! **library** it came from (the `src @<lib>` import that brought
//! it into scope, or `<entry>` for the user's directly-typed
//! batch) plus its doc comments and a one-line signature brief.
//!
//! Consumed by the fuzzy symbol-search popup (slice 8) and the
//! libraries view (slice 9). The picker doesn't need to know
//! about parse state; it just needs a flat list of `Symbol`s with
//! enough metadata to display, filter, and rank.
//!
//! The index is purely structural — no fuzzy-matching here. That's
//! the picker's job; the index just hands it the candidate list.

use std::collections::BTreeMap;
use std::path::PathBuf;

use vw_htcl::ast::{CommandKind, Document, EnumVariant, ProcSignature, Stmt};
use vw_htcl::loader::LoadedProgram;
use vw_htcl::span::Span;

use crate::session::{Session, SessionBatch};

#[derive(Clone, Debug)]
pub struct Symbol {
    /// Fully-qualified name (`util::props`, `Property::Scalar`, etc.).
    pub name: String,
    pub kind: SymbolKind,
    pub library: LibraryRef,
    /// First paragraph of the doc comments, reflowed for a compact
    /// summary in the picker's result row.
    pub doc_summary: String,
    /// Full reflowed doc body, shown in the picker's preview pane
    /// (when added) and in the hover popup (already wired).
    pub doc_full: String,
    /// One-line "signature brief" — `name -arg: type -arg: type → ret`
    /// for procs, `enum Name = { V1; V2 }` for enums, etc.
    pub signature_brief: String,
    /// Origin span in the source the symbol was declared in. `None`
    /// for variables (whose `set` site isn't a "declaration" in the
    /// AST sense we want to jump to). Used by a future goto-def
    /// keybinding; the picker doesn't need it.
    pub def_span: Option<Span>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolKind {
    Proc,
    Type,
    EnumDecl,
    EnumVariant,
    Variable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryRef {
    /// Symbol was declared in the user's directly-typed batch (no
    /// `src @` import). Shown as `<entry>` in the picker.
    Entry,
    /// Symbol was declared in a file pulled in via a `src @<name>`
    /// import (transitively — nested imports still attribute to the
    /// top-level `@<name>`).
    Import { name: String, path: PathBuf },
}

impl LibraryRef {
    /// Short display name — `<entry>` or the library's import name.
    /// Used as the prefix in picker rows.
    pub fn display(&self) -> String {
        match self {
            LibraryRef::Entry => "<entry>".to_string(),
            LibraryRef::Import { name, .. } => name.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LibraryInfo {
    pub library: LibraryRef,
    /// Number of symbols this library contributes to the index.
    pub symbol_count: usize,
}

/// Flat catalog of every symbol the session + in-flight input
/// knows about. Build via [`SymbolIndex::build`]; iterate via
/// [`SymbolIndex::all`] or [`SymbolIndex::libraries`].
#[derive(Clone, Debug, Default)]
pub struct SymbolIndex {
    all: Vec<Symbol>,
}

impl SymbolIndex {
    pub fn build(
        session: &Session,
        pending: Option<&SessionBatch>,
        in_flight: Option<&Document>,
    ) -> Self {
        let mut symbols = Vec::new();
        for batch in session.batches_for_doc_search() {
            collect_from_batch(batch, &mut symbols);
        }
        // Pending batch — the in-flight eval. During a long
        // `src @vivado-cmd` load, every proc the user just sourced
        // lives here, NOT yet in session.signature_table. Including
        // it makes Ctrl-S / :libs work mid-eval, same way the
        // Tab-completion fix did.
        if let Some(batch) = pending {
            collect_from_batch(batch, &mut symbols);
        }
        if let Some(doc) = in_flight {
            collect_from_doc(doc, None, &mut symbols);
        }
        // Dedupe by (name, kind) — later-batch decls shadow earlier
        // (Tcl's "later proc redefines" semantics). Since we walked
        // session newest-first via batches_for_doc_search, the FIRST
        // occurrence in our list is the one to keep.
        let mut seen: BTreeMap<(String, SymbolKind), ()> = BTreeMap::new();
        symbols.retain(|s| seen.insert((s.name.clone(), s.kind), ()).is_none());
        Self { all: symbols }
    }

    pub fn all(&self) -> &[Symbol] {
        &self.all
    }

    /// Distinct libraries with their symbol counts, sorted by
    /// descending count (heavyweights like `vivado-cmd` first).
    pub fn libraries(&self) -> Vec<LibraryInfo> {
        let mut counts: BTreeMap<String, (LibraryRef, usize)> = BTreeMap::new();
        for sym in &self.all {
            let entry = counts
                .entry(sym.library.display())
                .or_insert_with(|| (sym.library.clone(), 0));
            entry.1 += 1;
        }
        let mut out: Vec<LibraryInfo> = counts
            .into_values()
            .map(|(library, symbol_count)| LibraryInfo {
                library,
                symbol_count,
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.symbol_count));
        out
    }
}

fn collect_from_batch(batch: &SessionBatch, out: &mut Vec<Symbol>) {
    collect_from_doc(&batch.document, Some(&batch.program), out);
}

fn collect_from_doc(
    doc: &Document,
    program: Option<&LoadedProgram>,
    out: &mut Vec<Symbol>,
) {
    walk_stmts(&doc.stmts, "", program, out);
}

fn walk_stmts(
    stmts: &[Stmt],
    prefix: &str,
    program: Option<&LoadedProgram>,
    out: &mut Vec<Symbol>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else {
            continue;
        };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(name) = proc.name.as_deref() else {
                    continue;
                };
                let Some(sig) = proc.signature.as_ref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                let library = library_for_span(program, cmd.span);
                let signature_brief = render_signature(&qualified, sig);
                let doc_summary =
                    vw_htcl::doc::brief(&cmd.doc_comments).unwrap_or_default();
                let doc_full =
                    vw_htcl::doc::reflow_doc_comments(&cmd.doc_comments);
                out.push(Symbol {
                    name: qualified,
                    kind: SymbolKind::Proc,
                    library,
                    doc_summary,
                    doc_full,
                    signature_brief,
                    def_span: Some(proc.name_span),
                });
                // Recurse into proc body so nested procs / type decls
                // also enter the index. Rare but supported.
                walk_stmts(&proc.body, prefix, program, out);
            }
            CommandKind::TypeDecl(td) => {
                let Some(name) = td.name.as_deref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                let library = library_for_span(program, cmd.span);
                let underlying = td
                    .underlying
                    .as_ref()
                    .map(render_type)
                    .unwrap_or_else(|| "?".into());
                let signature_brief =
                    format!("type {qualified} = {underlying}");
                let doc_summary =
                    vw_htcl::doc::brief(&cmd.doc_comments).unwrap_or_default();
                let doc_full =
                    vw_htcl::doc::reflow_doc_comments(&cmd.doc_comments);
                out.push(Symbol {
                    name: qualified,
                    kind: SymbolKind::Type,
                    library,
                    doc_summary,
                    doc_full,
                    signature_brief,
                    def_span: Some(td.name_span),
                });
            }
            CommandKind::EnumDecl(ed) => {
                let Some(name) = ed.name.as_deref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                let library = library_for_span(program, cmd.span);
                let variants_brief: Vec<String> = ed
                    .variants
                    .iter()
                    .map(|v| {
                        if let Some(ty) = v.payload.as_ref() {
                            format!("{}: {}", v.name, render_type(ty))
                        } else {
                            v.name.clone()
                        }
                    })
                    .collect();
                let signature_brief = format!(
                    "enum {qualified} = {{ {} }}",
                    variants_brief.join("; ")
                );
                let doc_summary =
                    vw_htcl::doc::brief(&cmd.doc_comments).unwrap_or_default();
                let doc_full =
                    vw_htcl::doc::reflow_doc_comments(&cmd.doc_comments);
                out.push(Symbol {
                    name: qualified.clone(),
                    kind: SymbolKind::EnumDecl,
                    library: library.clone(),
                    doc_summary: doc_summary.clone(),
                    doc_full: doc_full.clone(),
                    signature_brief,
                    def_span: Some(ed.name_span),
                });
                // One Symbol per variant so users can search for
                // variant names directly (`Scalar`, `Nested`, …).
                for v in &ed.variants {
                    push_variant(&qualified, v, library.clone(), out);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                let nested = qualify(prefix, name);
                walk_stmts(&ns.body, &nested, program, out);
            }
            CommandKind::Set => {
                // Top-level / proc-body variable. Take the first
                // word as the variable name. Variables are tagged
                // with the library of the enclosing batch but get
                // no def_span (set isn't a declaration in the
                // jump-to-def sense we want).
                if let Some(name_word) = cmd.words.get(1) {
                    if let Some(name) = name_word.as_text() {
                        let library = library_for_span(program, cmd.span);
                        out.push(Symbol {
                            name: qualify(prefix, name),
                            kind: SymbolKind::Variable,
                            library,
                            doc_summary: String::new(),
                            doc_full: String::new(),
                            signature_brief: format!("set {name}"),
                            def_span: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_variant(
    enum_qualified: &str,
    v: &EnumVariant,
    library: LibraryRef,
    out: &mut Vec<Symbol>,
) {
    let qualified = format!("{enum_qualified}::{}", v.name);
    let signature_brief = if let Some(ty) = v.payload.as_ref() {
        format!("{qualified}({})", render_type(ty))
    } else {
        qualified.clone()
    };
    out.push(Symbol {
        name: qualified,
        kind: SymbolKind::EnumVariant,
        library,
        doc_summary: String::new(),
        doc_full: String::new(),
        signature_brief,
        def_span: Some(v.name_span),
    });
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

/// Attribute a source span to its originating library. Walks the
/// loader's `regions` + `files` tables to find which file the span
/// came from, then climbs the `imported_via` chain to the
/// top-level import (the `src @<lib>` directly under the entry
/// file). Returns `LibraryRef::Entry` when the span lies in the
/// entry file itself or when no `LoadedProgram` is provided
/// (in-flight input that hasn't been loaded through the import
/// machinery).
fn library_for_span(program: Option<&LoadedProgram>, span: Span) -> LibraryRef {
    let Some(program) = program else {
        return LibraryRef::Entry;
    };
    let Some((file_idx, _)) = program.locate(span.start) else {
        return LibraryRef::Entry;
    };
    // Build the chain: [origin_file, ..., entry_file].
    let mut chain = vec![file_idx];
    let mut cur = file_idx;
    while let Some(edge) = program.files[cur].imported_via {
        chain.push(edge.importer_file);
        cur = edge.importer_file;
    }
    if chain.len() <= 1 {
        // The origin is the entry file.
        return LibraryRef::Entry;
    }
    // The element just before the entry is the top-level imported
    // file — that's our library.
    let top_imported = chain[chain.len() - 2];
    let file = &program.files[top_imported];
    let name = library_name_for_path(&file.path);
    LibraryRef::Import {
        name,
        path: file.path.clone(),
    }
}

/// Short, user-facing library name. We use the parent directory's
/// name when it's available (so
/// `/home/ry/src/htcl/amd/vivado-cmd/module.htcl` becomes
/// `vivado-cmd`); otherwise fall back to the file's stem.
fn library_name_for_path(path: &std::path::Path) -> String {
    if let Some(parent) = path.parent() {
        if let Some(name) = parent.file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn render_type(ty: &vw_htcl::TypeExpr) -> String {
    use vw_htcl::TypeExpr;
    match ty {
        TypeExpr::Named { name, .. } => name.clone(),
        TypeExpr::Generic { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{name}<{}>", inner.join(", "))
        }
        TypeExpr::Qualified {
            namespace, variant, ..
        } => format!("{namespace}::{variant}"),
    }
}

fn render_signature(name: &str, sig: &ProcSignature) -> String {
    let mut out = name.to_string();
    for arg in &sig.args {
        out.push_str(" -");
        out.push_str(&arg.name);
        if let Some(ty) = arg.type_annotation.as_ref() {
            out.push_str(": ");
            out.push_str(&render_type(ty));
        }
    }
    if let Some(ret) = sig.return_type.as_ref() {
        out.push_str(" → ");
        out.push_str(&render_type(ret));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vw_htcl::parser::parse;

    #[test]
    fn empty_session_yields_empty_index() {
        let session = Session::default();
        let idx = SymbolIndex::build(&session, None, None);
        assert!(idx.all().is_empty());
        assert!(idx.libraries().is_empty());
    }

    #[test]
    fn in_flight_proc_decl_indexed() {
        let session = Session::default();
        let parsed = parse("proc foo {x: int} bool { return $x }");
        let idx = SymbolIndex::build(&session, None, Some(&parsed.document));
        assert_eq!(idx.all().len(), 1);
        let sym = &idx.all()[0];
        assert_eq!(sym.name, "foo");
        assert_eq!(sym.kind, SymbolKind::Proc);
        assert_eq!(sym.library, LibraryRef::Entry);
        assert!(sym.signature_brief.contains("foo"));
        assert!(sym.signature_brief.contains("int"));
        assert!(sym.signature_brief.contains("bool"));
    }

    #[test]
    fn namespaced_procs_qualified() {
        let session = Session::default();
        let src = "namespace eval util {\n  proc props {x: int} string { return $x }\n}";
        let parsed = parse(src);
        let idx = SymbolIndex::build(&session, None, Some(&parsed.document));
        assert!(
            idx.all().iter().any(|s| s.name == "util::props"),
            "expected util::props in index: {:?}",
            idx.all().iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn enum_emits_decl_and_variants() {
        let session = Session::default();
        let src =
            "enum Property = {\n  Scalar: string\n  Nested: Properties\n}";
        let parsed = parse(src);
        let idx = SymbolIndex::build(&session, None, Some(&parsed.document));
        let names: Vec<&str> =
            idx.all().iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Property"), "{names:?}");
        assert!(names.contains(&"Property::Scalar"), "{names:?}");
        assert!(names.contains(&"Property::Nested"), "{names:?}");
    }

    #[test]
    fn type_decl_indexed() {
        let session = Session::default();
        let parsed = parse("type Properties = {dict<string, Property>}");
        let idx = SymbolIndex::build(&session, None, Some(&parsed.document));
        let ty = idx
            .all()
            .iter()
            .find(|s| s.name == "Properties" && s.kind == SymbolKind::Type)
            .expect("Properties not found");
        assert!(ty.signature_brief.contains("type"));
        assert!(ty.signature_brief.contains("dict"));
    }

    #[test]
    fn libraries_count_symbols() {
        let session = Session::default();
        let parsed = parse(
            "proc foo {} unit {}\nproc bar {} unit {}\nproc baz {} unit {}",
        );
        let idx = SymbolIndex::build(&session, None, Some(&parsed.document));
        let libs = idx.libraries();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].symbol_count, 3);
    }
}
