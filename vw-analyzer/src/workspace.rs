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
    /// Names of every dep the file's resolver knows about
    /// (workspace `vw.toml` + editor extra_roots + sibling
    /// scan). Passed to the validator's undefined-src-module
    /// check so `src @<name>` where `<name>` isn't in the set
    /// gets a spanned Error diagnostic. Empty when no workspace
    /// context resolved.
    pub dep_names: std::collections::HashSet<String>,
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
    build_view_in(
        file_uri,
        local_text,
        extra_roots,
        vw_lib::deps_directory().ok().as_deref(),
    )
}

/// [`build_view`] with the dependency cache directory passed in —
/// see [`build_resolver_in`] for why the seam exists.
fn build_view_in(
    file_uri: &Url,
    local_text: &str,
    extra_roots: &[PathBuf],
    deps_cache: Option<&Path>,
) -> WorkspaceView {
    let mut view = WorkspaceView {
        view_source: local_text.to_string(),
        local_len: local_text.len() as u32,
        imports: Vec::new(),
        dep_names: std::collections::HashSet::new(),
    };

    let Ok(file_path) = file_uri.to_file_path() else {
        return view;
    };
    let parent = file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let resolver = build_resolver_in(&file_path, extra_roots, deps_cache);
    // Snapshot dep names now — the resolver may get moved into
    // collect_imports below; the diagnostics pass needs the
    // set as a plain HashSet<String>.
    view.dep_names =
        resolver.deps().map(|(name, _)| name.to_string()).collect();

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

/// True when `entry_file` sits inside the workspace's `test/`
/// directory subtree. Used to decide whether the analyzer's
/// resolver includes `[test-dependencies]` for LSP goto-def /
/// hover / diagnostics inside test files.
fn is_test_file(entry_file: &Path, workspace_dir: &Utf8Path) -> bool {
    let ws_std = workspace_dir.as_std_path();
    let Ok(rel) = entry_file.strip_prefix(ws_std) else {
        return false;
    };
    rel.components()
        .next()
        .is_some_and(|c| std::path::Component::Normal("test".as_ref()) == c)
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
///
/// One post-pass runs after the merge: for a file inside the
/// dependency cache, any root the merge landed on that was never
/// actually fetched gets repointed at the sha that was — see
/// [`repair_unmaterialized_cache_deps`].
pub fn build_resolver_with(
    entry_file: &Path,
    extra_roots: &[PathBuf],
) -> Resolver {
    build_resolver_in(
        entry_file,
        extra_roots,
        vw_lib::deps_directory().ok().as_deref(),
    )
}

/// [`build_resolver_with`] with the dependency cache directory
/// passed in rather than read from the environment, so tests can
/// stage a cache without mutating `VW_DEPS_DIR` process-wide (the
/// analyzer's test binary builds resolvers from many threads at
/// once). `None` means no cache is available — resolution then
/// behaves exactly as it did before the cache repair existed.
fn build_resolver_in(
    entry_file: &Path,
    extra_roots: &[PathBuf],
    deps_cache: Option<&Path>,
) -> Resolver {
    let mut merged: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    if let Some(workspace_dir) = find_workspace_dir(entry_file) {
        // A file under `<ws>/test/**` is a test file — pull in
        // `[test-dependencies]` too so `src @<test-dep>` resolves
        // in the analyzer just like it does in `vw test`. Matches
        // the CLI's `check_htcl_with_mode(_, include_test)`
        // behavior.
        let include_test = is_test_file(entry_file, &workspace_dir);
        // Transitive: a library that does `src @other-lib/...`
        // shouldn't force every consumer to redeclare `other-lib`
        // in their own `vw.toml`. The walker pulls in each dep's
        // own deps so the resolver sees the whole graph
        // (Cargo-style first-seen-wins on name conflicts).
        if let Ok(paths) = vw_lib::transitive_dep_cache_paths_with_test(
            &workspace_dir,
            include_test,
        ) {
            for (name, path) in paths {
                merged.entry(name).or_insert(path);
            }
        }
        // Cargo-parity self-reference: a workspace named `foo`
        // resolves `src @foo/bar` to `<ws>/bar.htcl`. Uses
        // `entry(...).or_insert(...)` so a legitimately-declared
        // external `foo` (rare but possible) still wins.
        if let Ok(cfg) = vw_lib::load_workspace_config(&workspace_dir) {
            merged
                .entry(cfg.workspace.name)
                .or_insert_with(|| workspace_dir.as_std_path().to_path_buf());
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
    if let Some(cache) = deps_cache {
        repair_unmaterialized_cache_deps(cache, entry_file, &mut merged);
    }
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

/// Repoint dep roots that name a cache directory which was never
/// fetched.
///
/// Dependency unification is first-seen-wins from the *entry*
/// workspace, so only the entry's pins are ever downloaded into
/// `~/.vw/deps`. Every cached dep still carries its own `vw.lock`
/// recording the shas *it* resolved independently, and those
/// directories generally don't exist locally — nothing ever needed
/// them, because the entry's pin of the same name won.
///
/// That stale lock stays invisible until you goto-def into a dep:
/// the opened file's nearest `vw.toml` is the dep's own, and an
/// editor that roots the server per `vw.toml` (Helix's
/// `roots = ["vw.toml"]`) leaves the analyzer no other workspace to
/// consult. Each `src @other-dep` in the dep then resolves to a
/// directory that isn't there, so no imported file loads at all and
/// every proc and namespace the dep pulled in reads as undefined.
///
/// Inside the cache, what's materialized is the better authority:
/// entries are named `<dep-name>-<sha>`, so a missing `<name>-<sha>`
/// can be repointed at whichever `<name>-<sha>` actually got
/// fetched. That copy is the one the consuming workspace unified on
/// — the same file `vw run` resolves the import to.
///
/// Deliberately restricted to files under the cache. In a real
/// project a dep root that isn't there means "run `vw update`", and
/// quietly resolving against some other sha would paper over that.
fn repair_unmaterialized_cache_deps(
    cache: &Path,
    entry_file: &Path,
    merged: &mut std::collections::HashMap<String, PathBuf>,
) {
    if !entry_file.starts_with(cache) {
        return;
    }
    let missing: Vec<String> = merged
        .iter()
        .filter(|(_, root)| !root.exists())
        .map(|(name, _)| name.clone())
        .collect();
    for name in missing {
        if let Some(root) = materialized_cache_entry(cache, &name) {
            merged.insert(name, root);
        }
    }
}

/// The cache directory holding `name`, or `None` when no sha of it
/// was ever fetched.
///
/// Candidates are `<cache>/<name>-<sha>` where `sha` is a full git
/// object id. Requiring 40 hex digits is what stops a dep named
/// `clk` from claiming `clk-wizard-<sha>` — dep names may
/// themselves contain `-`, so the prefix alone isn't enough. When
/// several shas of one dep are cached (two projects pinning
/// different commits), the most recently written wins; ties break on
/// the directory name so the pick is stable across runs.
fn materialized_cache_entry(cache: &Path, name: &str) -> Option<PathBuf> {
    let prefix = format!("{name}-");
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in fs::read_dir(cache).ok()?.flatten() {
        let file_name = entry.file_name();
        let Some(base) = file_name.to_str() else {
            continue;
        };
        let Some(sha) = base.strip_prefix(&prefix) else {
            continue;
        };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let candidate = (mtime, base.to_string());
        if best.as_ref().is_none_or(|best| *best < candidate) {
            best = Some(candidate);
        }
    }
    best.map(|(_, base)| cache.join(base))
}

/// Walk up from `start`'s parent directory looking for a `vw.toml`.
pub(crate) fn find_workspace_dir(start: &Path) -> Option<Utf8PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_FETCHED: &str = "1111111111111111111111111111111111111111";
    const SHA_PINNED: &str = "2222222222222222222222222222222222222222";

    /// A dependency cache holding `<name>-<sha>` roots, exactly as
    /// `vw update` materializes them.
    fn staged_cache() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize: macOS hands out `/var/...` temp dirs that are
        // really `/private/var/...`, and the repair keys off
        // `entry_file.starts_with(cache)`.
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    /// Write `<cache>/<name>-<sha>/` as an htcl workspace whose
    /// `module.htcl` is `body`, plus a `vw.lock` pinning each
    /// `(dep, sha)` in `locked`.
    fn seed(
        cache: &Path,
        name: &str,
        sha: &str,
        body: &str,
        locked: &[(&str, &str)],
    ) -> PathBuf {
        let root = cache.join(format!("{name}-{sha}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("vw.toml"),
            format!("[workspace]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        if !locked.is_empty() {
            let lock: String = locked
                .iter()
                .map(|(dep, dep_sha)| {
                    format!(
                        "[dependencies.{dep}]\n\
                         repo = \"https://example.invalid/{dep}\"\n\
                         branch = \"main\"\ncommit = \"{dep_sha}\"\n\
                         path = \"{dep}-{dep_sha}\"\n"
                    )
                })
                .collect();
            fs::write(root.join("vw.lock"), lock).unwrap();
        }
        fs::write(root.join("module.htcl"), body).unwrap();
        root
    }

    /// The reported bug: goto-def into a cached dep opened a file
    /// whose own `vw.lock` pins its deps at shas that were never
    /// fetched — the entry workspace's pin won unification, so only
    /// *that* sha is on disk. Every `src @<dep>` then resolved to a
    /// missing directory, no imported file loaded, and every proc
    /// and namespace the dep imported read as undefined.
    #[test]
    fn cached_dep_resolves_imports_against_the_fetched_sha() {
        let (_tmp, cache) = staged_cache();
        seed(
            &cache,
            "lib",
            SHA_FETCHED,
            "proc helper {} string {\n}\n",
            &[],
        );
        // `consumer` pins lib at a sha nobody ever downloaded.
        let consumer = seed(
            &cache,
            "consumer",
            SHA_FETCHED,
            "src @lib\n",
            &[("lib", SHA_PINNED)],
        );
        let entry = consumer.join("module.htcl");

        let resolver = build_resolver_in(&entry, &[], Some(&cache));
        assert_eq!(
            resolver.resolve(&consumer, "@lib").unwrap(),
            cache.join(format!("lib-{SHA_FETCHED}")).join("module.htcl"),
        );

        // End to end: the import's text actually lands in the view,
        // which is what makes `lib::helper` defined for diagnostics,
        // hover and goto.
        let text = fs::read_to_string(&entry).unwrap();
        let view = build_view_in(
            &Url::from_file_path(&entry).unwrap(),
            &text,
            &[],
            Some(&cache),
        );
        assert_eq!(view.imports.len(), 1, "{:?}", view.view_source);
        assert!(view.view_source.contains("proc helper"));
    }

    /// The repair is scoped to files inside the cache. A real
    /// project with a dep it hasn't fetched must keep pointing at
    /// the pinned root so the user still gets told to run
    /// `vw update`, rather than silently reading whatever sha some
    /// other project happens to have downloaded.
    #[test]
    fn a_project_outside_the_cache_keeps_its_own_pin() {
        let (_tmp, cache) = staged_cache();
        seed(
            &cache,
            "lib",
            SHA_FETCHED,
            "proc helper {} string {\n}\n",
            &[],
        );
        let (_proj_tmp, project) = staged_cache();
        fs::write(
            project.join("vw.toml"),
            "[workspace]\nname = \"proj\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            project.join("vw.lock"),
            format!(
                "[dependencies.lib]\nrepo = \"https://example.invalid/lib\"\n\
                 branch = \"main\"\ncommit = \"{SHA_PINNED}\"\n\
                 path = \"lib-{SHA_PINNED}\"\n"
            ),
        )
        .unwrap();
        let entry = project.join("module.htcl");
        fs::write(&entry, "src @lib\n").unwrap();

        // A `vw.lock` path is relative and resolves against the
        // real cache dir, so the pin is asserted by basename: what
        // matters is that the staged `lib-<SHA_FETCHED>` next door
        // was not adopted in its place.
        let resolver = build_resolver_in(&entry, &[], Some(&cache));
        let (_, root) = resolver.deps().find(|(n, _)| *n == "lib").unwrap();
        assert!(root.ends_with(format!("lib-{SHA_PINNED}")), "{root:?}");
    }

    /// A root the merge produced that *does* exist is authoritative
    /// — the repair only touches unfetched ones, so a cache holding
    /// two shas of the same dep never drags a file off its own pin.
    #[test]
    fn a_fetched_pin_is_left_alone() {
        let (_tmp, cache) = staged_cache();
        seed(&cache, "lib", SHA_FETCHED, "proc old {} string {\n}\n", &[]);
        seed(&cache, "lib", SHA_PINNED, "proc new {} string {\n}\n", &[]);
        let consumer = seed(
            &cache,
            "consumer",
            SHA_FETCHED,
            "src @lib\n",
            &[("lib", SHA_PINNED)],
        );
        let entry = consumer.join("module.htcl");

        let resolver = build_resolver_in(&entry, &[], Some(&cache));
        assert_eq!(
            resolver.resolve(&consumer, "@lib").unwrap(),
            cache.join(format!("lib-{SHA_PINNED}")).join("module.htcl"),
        );
    }

    /// Dep names contain `-` too, so prefix matching alone would let
    /// `clk` swallow `clk-wizard-<sha>`. The 40-hex-digit suffix
    /// check is what keeps them apart.
    #[test]
    fn a_dep_name_prefix_does_not_claim_a_longer_neighbor() {
        let (_tmp, cache) = staged_cache();
        seed(&cache, "clk-wizard", SHA_FETCHED, "", &[]);
        assert_eq!(materialized_cache_entry(&cache, "clk"), None);
        assert_eq!(
            materialized_cache_entry(&cache, "clk-wizard"),
            Some(cache.join(format!("clk-wizard-{SHA_FETCHED}"))),
        );
    }

    /// Nothing to fall back on when the dep was never fetched under
    /// any sha: the entry keeps its pinned (missing) root and the
    /// import fails to resolve as before.
    #[test]
    fn an_entirely_unfetched_dep_is_left_pinned() {
        let (_tmp, cache) = staged_cache();
        let consumer = seed(
            &cache,
            "consumer",
            SHA_FETCHED,
            "src @lib\n",
            &[("lib", SHA_PINNED)],
        );
        let entry = consumer.join("module.htcl");

        let resolver = build_resolver_in(&entry, &[], Some(&cache));
        let (_, root) = resolver.deps().find(|(n, _)| *n == "lib").unwrap();
        assert!(root.ends_with(format!("lib-{SHA_PINNED}")), "{root:?}");
    }
}
