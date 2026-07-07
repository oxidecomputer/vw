// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! In-memory REPL session, held as parsed batches rather than a
//! re-concatenated text blob.
//!
//! Every successful input contributes one [`SessionBatch`] — the
//! loaded program (own source + import edges), its parsed
//! [`vw_htcl::Document`], and the map from proc-name to
//! [`ProcLocation`] for every proc the batch declared. Prior
//! batches are read by [`crate::lower::prepare`] when lowering the
//! next input: their signatures resolve unknown calls; their proc
//! locations let the error renderer translate Tcl's `(procedure
//! "X" line N)` frames back to the real `.htcl` file the wrapper
//! body was declared in.
//!
//! Why this shape (vs. the original text-blob prelude):
//!
//! 1. **Performance.** After a few `src @lib` imports the prelude
//!    is hundreds of thousands of lines. Re-parsing and re-walking
//!    it on every input is what made the REPL feel laggy. Storing
//!    parsed state means each new input parses only its own
//!    content + transitive imports — O(new), not O(total).
//! 2. **Error rendering.** A drill-down frame for a wrapper proc
//!    declared in an earlier batch knows the real `.htcl` path it
//!    came from, so `(procedure "vivado::create_bd_design" line
//!    2)` resolves to `vivado-cmd/bd.htcl:42` instead of
//!    `(input):199185` of the giant combined scratch.

use std::collections::HashMap;

use vw_htcl::{Document, LoadedProgram, ProcSignature, TypeDecl};

use crate::lower::ProcLocation;

/// One committed input: the parsed program it produced, plus the
/// proc-location map the lowerer derived from it. Stored on a
/// per-batch basis so signatures and proc lookups can fold across
/// the whole session without ever re-parsing a prior batch.
#[derive(Debug)]
pub struct SessionBatch {
    /// Loader output for this batch — file paths, import edges,
    /// and the flattened source. Held alongside the parsed
    /// document so future analyzer features (completion, goto-
    /// def, hover) can walk back to per-file context without
    /// re-running the loader. Spans inside [`document`] are
    /// offsets into [`program.source`](LoadedProgram::source);
    /// keeping the program alive keeps those spans meaningful.
    #[allow(dead_code)]
    pub program: LoadedProgram,
    pub document: Document,
    pub procs: HashMap<String, ProcLocation>,
}

/// A live REPL session: every committed batch in order.
#[derive(Debug, Default)]
pub struct Session {
    batches: Vec<SessionBatch>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a batch — called from the App after every successful
    /// eval (including the pure-`src` no-Tcl-to-eval case, which
    /// commits immediately because no eval can fail).
    pub fn commit(&mut self, batch: SessionBatch) {
        self.batches.push(batch);
    }

    /// Build a merged signature table covering every proc declared
    /// in the session so far. Later batches shadow earlier ones,
    /// matching Tcl's "second `proc` redefines" semantics. The
    /// returned map borrows from `self`; held only for the duration
    /// of the next batch's prepare() call.
    pub fn signature_table(&self) -> HashMap<String, &ProcSignature> {
        let mut table: HashMap<String, &ProcSignature> = HashMap::new();
        for batch in &self.batches {
            // Per-batch table merges into the running table; later
            // batches' entries overwrite earlier ones via `insert`.
            let batch_table = vw_htcl::signature_table(&batch.document);
            for (name, sig) in batch_table {
                table.insert(name, sig);
            }
        }
        table
    }

    /// Same as [`signature_table`] but for `type NAME = …`
    /// declarations. Needed when wrapping a typed expression's
    /// result through its `repr` proc — the dispatch type may be
    /// a newtype declared in a prior batch (e.g. `Properties`
    /// from a sourced `@vivado-cmd` library), and the repr
    /// codegen walks the underlying to emit the dependent generic
    /// repr (`dict_string_Property::repr` in that case).
    pub fn type_decl_table(&self) -> HashMap<String, &TypeDecl> {
        let mut table: HashMap<String, &TypeDecl> = HashMap::new();
        for batch in &self.batches {
            let mut diags = Vec::new();
            let batch_table =
                vw_htcl::build_type_decl_table(&batch.document, &mut diags);
            for (name, td) in batch_table {
                table.insert(name, td);
            }
        }
        table
    }

    /// Union of every top-level variable name defined across every
    /// batch. Passed to the validator so the undef-var pass doesn't
    /// false-positive `set p …` in an earlier REPL input followed
    /// by `$p` in a later one. Only top-level names — proc-body
    /// locals don't leak across evals (Tcl semantics).
    pub fn top_level_var_names(&self) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        for batch in &self.batches {
            names.extend(vw_htcl::top_level_var_names(
                &batch.document,
                &batch.program.source,
            ));
        }
        names
    }

    /// Companion to [`top_level_var_names`] returning inferred
    /// types for the top-level `set` bindings across every
    /// committed batch. Later batches shadow earlier ones so
    /// re-binding `set foo […]` overrides the previous entry.
    ///
    /// The signature table is rebuilt per batch here — batches
    /// commit in order and each is inspected independently, so
    /// value_type inside a batch resolves against that batch's
    /// own procs. Cross-batch signature resolution would require
    /// threading the full accumulated sig table, which currently
    /// isn't needed for the putr use case (values reach `set`
    /// through proc calls the batch itself defines or imports).
    pub fn top_level_var_types(
        &self,
    ) -> std::collections::HashMap<String, vw_htcl::TypeExpr> {
        let mut types = std::collections::HashMap::new();
        for batch in &self.batches {
            let mut sig_diags = Vec::new();
            let sig_table = vw_htcl::validate::build_signature_table(
                &batch.document,
                &mut sig_diags,
            );
            let batch_types =
                vw_htcl::top_level_var_types(&batch.document, &sig_table);
            for (name, ty) in batch_types {
                types.insert(name, ty);
            }
        }
        types
    }

    /// Look up the most-recent proc location across every batch.
    /// Returns `None` when no batch has declared that proc — the
    /// error renderer's drill-down path silently skips such frames
    /// (Tcl proc frames for builtins, dynamically-defined procs,
    /// etc.).
    /// Iterate committed batches newest-first. Used by the
    /// signature-help / hover paths to walk back through documents
    /// looking for a proc's doc comments — Tcl's "later proc
    /// shadows earlier" semantics mean the most-recent definition
    /// is the one the user expects to see described.
    pub fn batches_for_doc_search(
        &self,
    ) -> impl Iterator<Item = &SessionBatch> {
        self.batches.iter().rev()
    }

    pub fn lookup_proc(&self, name: &str) -> Option<&ProcLocation> {
        for batch in self.batches.iter().rev() {
            if let Some(loc) = batch.procs.get(name) {
                return Some(loc);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vw_htcl::parse;

    fn batch_from(source: &str) -> SessionBatch {
        // Build a minimal in-memory LoadedProgram from a string for
        // tests that don't care about the loader pipeline.
        let parsed = parse(source);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        SessionBatch {
            program: LoadedProgram {
                source: source.to_string(),
                files: Vec::new(),
                regions: Vec::new(),
            },
            document: parsed.document,
            procs: HashMap::new(),
        }
    }

    #[test]
    fn signature_table_folds_across_batches() {
        let mut s = Session::new();
        s.commit(batch_from("proc foo { x } { }\n"));
        s.commit(batch_from("proc bar { y } { }\n"));
        let table = s.signature_table();
        assert!(table.contains_key("foo"));
        assert!(table.contains_key("bar"));
    }

    #[test]
    fn later_batch_shadows_earlier_signature() {
        // Second `proc foo` redefines the first — the merged table
        // returns the newer signature.
        let mut s = Session::new();
        s.commit(batch_from("proc foo { x } { }\n"));
        s.commit(batch_from("proc foo { y z } { }\n"));
        let table = s.signature_table();
        let sig = table.get("foo").unwrap();
        let arg_names: Vec<&str> =
            sig.args.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(arg_names, vec!["y", "z"]);
    }

    #[test]
    fn lookup_proc_returns_latest_batch() {
        // Two batches both register `foo` in their `procs` map (the
        // lowerer normally does this, but here we set it manually).
        // `lookup_proc` returns the entry from the most recent
        // batch.
        let mut a = batch_from("proc foo { x } { }\n");
        let mut b = batch_from("proc foo { y } { }\n");
        a.procs.insert(
            "foo".into(),
            ProcLocation {
                file: None,
                body_start_line: 10,
                body_lines: vec!["from-a".into()],
            },
        );
        b.procs.insert(
            "foo".into(),
            ProcLocation {
                file: None,
                body_start_line: 20,
                body_lines: vec!["from-b".into()],
            },
        );
        let mut s = Session::new();
        s.commit(a);
        s.commit(b);
        let got = s.lookup_proc("foo").unwrap();
        assert_eq!(got.body_start_line, 20);
        assert_eq!(got.body_lines[0], "from-b");
        assert!(s.lookup_proc("missing").is_none());
    }
}
