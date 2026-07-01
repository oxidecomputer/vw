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
///
/// `extra_roots` supplies fallback workspace roots (typically the
/// editor's `rootUri` / `workspaceFolders`) so files opened outside
/// the enclosing `vw.toml` — e.g. via goto-def into a dep cache
/// dir — still resolve `@name/…` imports through the outer
/// workspace's dep graph. Pass `&[]` when the caller doesn't have
/// that context.
pub fn build_view(
    file_uri: &Url,
    local_text: &str,
    extra_roots: &[PathBuf],
) -> WorkspaceView {
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
    let resolver = build_resolver_with(&file_path, extra_roots);

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

/// Build a [`Resolver`] for the workspace that owns `entry_file`.
/// Convenience wrapper — see [`build_resolver_with`] for the full
/// variant that also honors editor-supplied fallback workspace
/// roots (LSP `rootUri` / `workspaceFolders`).
pub fn build_resolver(entry_file: &Path) -> Resolver {
    build_resolver_with(entry_file, &[])
}

/// Build a [`Resolver`] for `entry_file`, merging dep declarations
/// from every source that could plausibly resolve a `@name/…`
/// import when the file's own workspace doesn't declare it:
///
/// 1. The file's own workspace — walk up to the nearest `vw.toml`
///    and expand its dep graph via
///    [`vw_lib::transitive_dep_cache_paths`]. Highest precedence.
/// 2. Each path in `extra_roots` — treated as a workspace directory
///    and expanded the same way. Used to plumb the LSP's `rootUri`
///    (or `workspaceFolders`) so a file opened via goto-def *out*
///    of the editor's root workspace still inherits its dep names.
/// 3. Sibling-workspace layout scan — for every ancestor directory
///    of `entry_file`, treat each direct subdirectory that itself
///    contains a `vw.toml` as an implicit dep whose name is the
///    subdirectory basename. This lets a
///    `~/src/htcl/amd/cpm5/module.htcl` still see `@vivado-cmd`
///    at `~/src/htcl/amd/vivado-cmd/` even when neither its own
///    workspace nor the editor's root declares the dep — a monorepo
///    layout that's typical of a `foo-htcl` collection of siblings.
///
/// First-seen wins on name collisions in the order above, so the
/// file's own workspace's choice never gets overridden.
pub fn build_resolver_with(
    entry_file: &Path,
    extra_roots: &[PathBuf],
) -> Resolver {
    let mut merged: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    if let Some(workspace_dir) = find_workspace_dir(entry_file) {
        // Transitive: a library that does `src @other-lib/...`
        // shouldn't force every consumer to redeclare `other-lib`
        // in their own `vw.toml`. The walker pulls in each dep's
        // own deps so the resolver sees the whole graph
        // (Cargo-style first-seen-wins on name conflicts).
        if let Ok(paths) = vw_lib::transitive_dep_cache_paths(&workspace_dir) {
            for (name, path) in paths {
                merged.entry(name).or_insert(path);
            }
        }
    }
    for root in extra_roots {
        let Ok(root_utf8) = Utf8PathBuf::from_path_buf(root.clone()) else {
            continue;
        };
        if !root_utf8.join("vw.toml").exists() {
            continue;
        }
        if let Ok(paths) = vw_lib::transitive_dep_cache_paths(&root_utf8) {
            for (name, path) in paths {
                merged.entry(name).or_insert(path);
            }
        }
    }
    collect_sibling_workspaces(entry_file, &mut merged);
    let mut resolver = Resolver::new();
    for (name, path) in merged {
        resolver = resolver.with_dep(name, path);
    }
    resolver
}

/// Walk up from `entry_file`, and at each ancestor directory add
/// every subdirectory that contains its own `vw.toml` as an
/// implicit dep — keyed by the subdirectory basename.
///
/// This mirrors a monorepo layout that's common for htcl workspaces:
/// `~/src/htcl/amd/{cips,cpm5,clk-wizard,vivado-cmd}/`. From any
/// one of those, the others are visible as siblings even when
/// no `vw.toml` explicitly declares them. Without this heuristic,
/// jumping into a dep-module file from an editor whose LSP has
/// restarted rooted at that dep's own `vw.toml` (helix's
/// `roots = ["vw.toml"]` behavior) would strand the analyzer with
/// no way to resolve `@sibling-name/…` imports.
///
/// Stops at the filesystem root or after a handful of ancestors —
/// scanning `~` or `/` for candidate workspaces would be both slow
/// and semantically wrong.
fn collect_sibling_workspaces(
    entry_file: &Path,
    merged: &mut std::collections::HashMap<String, PathBuf>,
) {
    // Cap the walk so we don't scan every level up to `/`. Six
    // ancestors covers the typical `~/src/<org>/<repo>/<module>/`
    // + a few extra tolerance for deeper nestings.
    const MAX_ANCESTORS: usize = 6;
    let mut cursor = match entry_file.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    for _ in 0..MAX_ANCESTORS {
        let read_dir = match std::fs::read_dir(&cursor) {
            Ok(r) => r,
            Err(_) => break,
        };
        for entry in read_dir.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let sub = entry.path();
            if !sub.join("vw.toml").is_file() {
                continue;
            }
            let Some(name) = sub.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            merged.entry(name.to_string()).or_insert(sub);
        }
        cursor = match cursor.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
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
///
/// `extra_roots` — see [`build_view`] for the same rationale — lets
/// callers plumb through the editor's workspace roots so a
/// `src @dep/file.htcl` in a file outside the enclosing workspace
/// still resolves.
pub fn resolve_import(
    entry_file: &Path,
    raw: &str,
    extra_roots: &[PathBuf],
) -> Option<PathBuf> {
    let parent = entry_file.parent()?;
    build_resolver_with(entry_file, extra_roots)
        .resolve(parent, raw)
        .ok()
}

/// Allow `&Utf8Path` callers to canonicalize through us.
#[allow(dead_code)]
pub fn workspace_root(entry_file: &Utf8Path) -> Option<Utf8PathBuf> {
    find_workspace_dir(Path::new(entry_file.as_str()))
}
