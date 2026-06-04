// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Workspace-aware helpers for the analyzer.
//!
//! The bare LSP backend deals with one file at a time. Cross-file
//! features — goto-definition into an imported module, completion of
//! procs defined in `@dep/foo`, validating a call against a signature
//! that lives elsewhere — need a view that spans the importing file
//! plus everything it pulled in via `src`.
//!
//! This module computes that view on demand. It's deliberately
//! re-computed per query rather than cached: htcl files are tiny next
//! to a Vivado IP wrapper, the LSP's edits-per-second is modest, and a
//! cache would have to deal with invalidation when an `@dep/...`
//! file on disk changes. A targeted cache is a sensible follow-up once
//! the access pattern is settled.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use camino::{Utf8Path, Utf8PathBuf};
use tower_lsp::lsp_types::Url;

use vw_htcl::{parse, CommandKind, Resolver, SrcImport, Stmt};

/// A flattened source view used for cross-file analysis.
///
/// `view_source` is the local file's text *first* (so the cursor's
/// byte offset in the open document is the same offset in the view —
/// hover/goto/etc. don't need offset translation for the local file),
/// followed by every transitively imported file's text concatenated.
/// Each appended region is recorded in [`imports`](Self::imports) so
/// spans landing there can be mapped back to the file they came from.
pub struct WorkspaceView {
    pub view_source: String,
    /// Byte length of the *local* file's contribution. Spans whose
    /// `start < local_len` belong to the open file; everything past
    /// that lives in some imported file.
    pub local_len: u32,
    pub imports: Vec<ImportRegion>,
}

pub struct ImportRegion {
    /// Inclusive start offset in `view_source`.
    pub start: u32,
    /// Exclusive end offset in `view_source`.
    pub end: u32,
    pub file_uri: Url,
}

impl WorkspaceView {
    /// If `offset` lies inside an imported file's region, return the
    /// import region plus the file-local offset of that span; `None`
    /// means the offset is in the open file itself.
    pub fn locate(&self, offset: u32) -> Option<(&ImportRegion, u32)> {
        if offset < self.local_len {
            return None;
        }
        self.imports
            .iter()
            .find(|r| offset >= r.start && offset < r.end)
            .map(|r| (r, offset - r.start))
    }
}

/// Build a workspace view by reading every file the entry transitively
/// `src`s. Returns a view with `imports` empty when the entry can't be
/// resolved to a filesystem path or has no imports — the analyzer can
/// still use it; it just won't see anything cross-file.
pub fn build_view(file_uri: &Url, local_text: &str) -> WorkspaceView {
    let mut view = WorkspaceView {
        view_source: local_text.to_string(),
        local_len: local_text.len() as u32,
        imports: Vec::new(),
    };

    let Ok(file_path) = file_uri.to_file_path() else {
        return view;
    };
    let parent = file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let resolver = build_resolver(&file_path);

    let mut loaded: HashSet<PathBuf> = HashSet::new();
    if let Ok(canonical) = file_path.canonicalize() {
        loaded.insert(canonical);
    }
    let mut queue: Vec<(PathBuf, String)> = Vec::new();
    collect_imports(local_text, &parent, &resolver, &mut loaded, &mut queue);

    while let Some((path, text)) = queue.pop() {
        view.view_source.push('\n');
        // Record `start` *after* the separator so a span's local
        // offset within the imported file is `span.start - start`
        // with no off-by-one for the inserted newline.
        let start = view.view_source.len() as u32;
        view.view_source.push_str(&text);
        let end = view.view_source.len() as u32;
        if let Ok(import_uri) = Url::from_file_path(&path) {
            view.imports.push(ImportRegion {
                start,
                end,
                file_uri: import_uri,
            });
        }
        // Recurse into this file's own imports.
        let import_parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        collect_imports(
            &text,
            &import_parent,
            &resolver,
            &mut loaded,
            &mut queue,
        );
    }

    view
}

/// Build a [`Resolver`] for the workspace that owns `entry_file`, by
/// walking up to find `vw.toml` and pulling dep cache paths through
/// `vw-lib`. Returns an empty resolver when no workspace is found —
/// relative/absolute `src` imports still work; `@name/` ones won't.
pub fn build_resolver(entry_file: &Path) -> Resolver {
    let mut resolver = Resolver::new();
    let Some(workspace_dir) = find_workspace_dir(entry_file) else {
        return resolver;
    };
    if let Ok(paths) = vw_lib::dep_cache_paths(&workspace_dir) {
        for (name, path) in paths {
            resolver = resolver.with_dep(name, path);
        }
    }
    resolver
}

/// Walk up from `start`'s parent directory looking for a `vw.toml`.
fn find_workspace_dir(start: &Path) -> Option<Utf8PathBuf> {
    let mut cur = start.parent()?.to_path_buf();
    loop {
        if cur.join("vw.toml").exists() {
            return Utf8PathBuf::from_path_buf(cur).ok();
        }
        cur = cur.parent()?.to_path_buf();
    }
}

/// Parse `text` and queue each new (not yet seen) `src` resolution as
/// `(canonical_path, file_text)` for the caller to incorporate.
fn collect_imports(
    text: &str,
    parent_dir: &Path,
    resolver: &Resolver,
    loaded: &mut HashSet<PathBuf>,
    queue: &mut Vec<(PathBuf, String)>,
) {
    let parsed = parse(text);
    for stmt in &parsed.document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Src(SrcImport {
            path: Some(raw), ..
        }) = &cmd.kind
        else {
            continue;
        };
        let Ok(resolved) = resolver.resolve(parent_dir, raw) else {
            continue;
        };
        // Resolver already canonicalizes when possible; defensive
        // dedup either way.
        if !loaded.insert(resolved.clone()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&resolved) else {
            continue;
        };
        queue.push((resolved, content));
    }
}

/// Public helper: resolve the import at `raw` from `entry_file`'s
/// directory. Used by goto-on-import-path so the analyzer can return
/// a Location pointing at the imported file.
pub fn resolve_import(entry_file: &Path, raw: &str) -> Option<PathBuf> {
    let parent = entry_file.parent()?;
    build_resolver(entry_file).resolve(parent, raw).ok()
}

/// Allow `&Utf8Path` callers to canonicalize through us.
#[allow(dead_code)]
pub fn workspace_root(entry_file: &Utf8Path) -> Option<Utf8PathBuf> {
    find_workspace_dir(Path::new(entry_file.as_str()))
}
