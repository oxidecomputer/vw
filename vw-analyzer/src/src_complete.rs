// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Filesystem-aware completion for `src` import paths.
//!
//! The `vw-htcl` crate stays free of filesystem concerns, so this
//! lives in the analyzer alongside the workspace resolver. When the
//! cursor sits in the path-position of a `src` command, we
//! enumerate the directory implied by the partial path and offer:
//!
//! - every `.htcl` file at that level, labelled by basename (no
//!   extension), and
//! - every subdirectory at that level that transitively contains at
//!   least one `.htcl` file, labelled with a trailing `/`.
//!
//! Three flavors of partial are recognized, matching
//! [`vw_htcl::src_path::classify`]:
//!
//! - `@<dep>/...` — resolve against the workspace dependency's cached
//!   root.
//! - `/abs/...`   — filesystem-absolute.
//! - anything else — relative to the importing file's directory.
//!
//! When the partial is just `@` or `@<prefix>` (no `/` yet), suggest
//! dependency names from the workspace resolver instead.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, InsertTextFormat,
    Position, Range, TextEdit,
};
use vw_htcl::cmdline::CmdLine;
use vw_htcl::src_path::{classify, PathKind};
use vw_htcl::{LineCol, LineIndex, Resolver, Span};

/// True when the cursor sits in the path-position of a `src` command
/// (i.e. the first complete word is `src` and we're typing the path).
pub fn is_src_path_context(line: &CmdLine<'_>) -> bool {
    line.words.first().copied() == Some("src") && line.words.len() == 1
}

/// Generate path completions for `line.partial`, treating it as the
/// `<path>` of `src <path>`.
///
/// `entry_file` is the open file's path on disk; it anchors relative
/// imports and lets us walk up to the workspace's `vw.toml` for dep
/// resolution. `line_index` maps source offsets to LSP positions.
pub fn src_path_completions(
    entry_file: &Path,
    line: &CmdLine<'_>,
    line_index: &LineIndex,
    resolver: &Resolver,
) -> Vec<CompletionItem> {
    let partial = line.partial;

    // `@<prefix>` with no `/` yet → dep-name completion. Replace the
    // whole partial with `@<name>/` so the next completion fires on
    // the contents.
    if let Some(prefix_after_at) = partial.strip_prefix('@') {
        if !prefix_after_at.contains('/') {
            return dep_name_completions(
                resolver,
                prefix_after_at,
                line.partial_span,
                line_index,
            );
        }
    }

    // Otherwise: resolve the directory the partial points into, then
    // enumerate it.
    let Some((dir, segment_start)) = resolve_dir(entry_file, resolver, partial)
    else {
        return Vec::new();
    };
    let segment = &partial[segment_start..];
    let replace = Span::new(
        line.partial_span.start + segment_start as u32,
        line.partial_span.end,
    );
    enumerate_entries(&dir, segment, replace, line_index)
}

/// Resolve the *directory* part of `partial` to an on-disk path, plus
/// the byte offset into `partial` where the trailing (still-being-
/// typed) segment begins. Returns `None` when the partial points at a
/// dep that doesn't exist or a path that can't be classified.
fn resolve_dir(
    entry_file: &Path,
    resolver: &Resolver,
    partial: &str,
) -> Option<(PathBuf, usize)> {
    let kind = classify(partial).kind;
    let (base, body) = match &kind {
        PathKind::Relative => {
            let dir = entry_file.parent()?.to_path_buf();
            (dir, partial)
        }
        PathKind::Absolute => {
            (PathBuf::from("/"), partial.trim_start_matches('/'))
        }
        PathKind::Named { name, subpath } => {
            let root = resolver.dep_root(name)?.to_path_buf();
            (root, subpath.as_str())
        }
    };
    // Split `body` at its last `/`: everything before is the
    // sub-directory walk; everything after is the segment being typed
    // (used for the replace range and ignored for enumeration).
    let (subdir, trailing_segment) = match body.rfind('/') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => ("", body),
    };
    let mut dir = base;
    if !subdir.is_empty() {
        dir.push(subdir);
    }
    let segment_start = partial.len() - trailing_segment.len();
    Some((dir, segment_start))
}

fn enumerate_entries(
    dir: &Path,
    segment: &str,
    replace: Span,
    line_index: &LineIndex,
) -> Vec<CompletionItem> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let _ = segment; // LSP client filters by prefix; we list everything.

    let mut out: Vec<(String, CompletionItemKind)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type().ok();
        if ft.is_some_and(|t| t.is_dir()) {
            if dir_has_htcl(&path) {
                out.push((format!("{name}/"), CompletionItemKind::FOLDER));
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("htcl") {
            let stem =
                path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
            // `module.htcl` is the dep's default entry point, already
            // reachable as bare `@<dep>` — listing it here as
            // `@<dep>/module` would just be a noisier alias.
            if stem == vw_htcl::src_path::DEFAULT_MODULE {
                continue;
            }
            out.push((stem.to_string(), CompletionItemKind::FILE));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let range = lsp_range(replace, line_index);
    out.into_iter()
        .map(|(label, kind)| build_item(label, kind, range))
        .collect()
}

fn dep_name_completions(
    resolver: &Resolver,
    _prefix: &str,
    partial_span: Span,
    line_index: &LineIndex,
) -> Vec<CompletionItem> {
    let mut deps: Vec<(&str, &Path)> = resolver.deps().collect();
    deps.sort_by_key(|(n, _)| *n);
    let range = lsp_range(partial_span, line_index);
    // Bare `@<dep>` is a complete import on its own (resolves to the
    // dep's `module.htcl`), so don't append a trailing `/` — that
    // would leave behind invalid syntax for a user who just wanted
    // the default module. Users who want to drill in still type `/`
    // themselves, which retriggers completion against the dep root.
    deps.into_iter()
        .map(|(name, _)| {
            build_item(format!("@{name}"), CompletionItemKind::MODULE, range)
        })
        .collect()
}

/// True if `dir` contains, or transitively contains, any `.htcl` file.
/// Short-circuits on the first hit.
fn dir_has_htcl(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            if path.extension().and_then(|s| s.to_str()) == Some("htcl") {
                return true;
            }
        } else if ft.is_dir() {
            // Skip dot-dirs to keep `.git`, `.svn`, etc. out of the
            // walk.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            if dir_has_htcl(&path) {
                return true;
            }
        }
    }
    false
}

fn build_item(
    label: String,
    kind: CompletionItemKind,
    range: Range,
) -> CompletionItem {
    let new_text = label.clone();
    CompletionItem {
        label,
        kind: Some(kind),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
        ..Default::default()
    }
}

fn lsp_range(span: Span, line_index: &LineIndex) -> Range {
    let (start, end) = line_index.range(span);
    Range {
        start: lc_to_pos(start),
        end: lc_to_pos(end),
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
    use std::fs;
    use vw_htcl::cmdline;

    fn workspace_fixture() -> (tempfile::TempDir, PathBuf, Resolver) {
        // amd-htcl/
        //   module.htcl                  ← default entry, HIDDEN from list
        //   cmd.htcl
        //   ip.htcl
        //   cmd/foo.htcl
        //   scripts/                     ← no .htcl, should NOT appear
        //   ip/bd/cell.htcl              ← nested, ip/ should appear
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("amd-htcl");
        fs::create_dir_all(dep.join("cmd")).unwrap();
        fs::create_dir_all(dep.join("scripts")).unwrap();
        fs::create_dir_all(dep.join("ip/bd")).unwrap();
        fs::write(dep.join("module.htcl"), "# entry").unwrap();
        fs::write(dep.join("cmd.htcl"), "# stub").unwrap();
        fs::write(dep.join("ip.htcl"), "# stub").unwrap();
        fs::write(dep.join("cmd/foo.htcl"), "# stub").unwrap();
        fs::write(dep.join("scripts/notes.txt"), "not htcl").unwrap();
        fs::write(dep.join("ip/bd/cell.htcl"), "# stub").unwrap();
        // entry file
        let entry = dir.path().join("prime.htcl");
        fs::write(&entry, "src @amd-htcl/cmd\n").unwrap();
        let resolver = Resolver::new().with_dep("amd-htcl", dep);
        // hold dir handle so files persist for the test
        (dir, entry, resolver)
    }

    fn labels_for(src: &str, entry: &Path, resolver: &Resolver) -> Vec<String> {
        let line = cmdline::analyze(src, src.len() as u32);
        let idx = LineIndex::new(src);
        let items = src_path_completions(entry, &line, &idx, resolver);
        let mut labels: Vec<String> =
            items.into_iter().map(|c| c.label).collect();
        labels.sort();
        labels
    }

    #[test]
    fn lists_dep_root_after_trailing_slash() {
        let (_dir, entry, resolver) = workspace_fixture();
        let labels = labels_for("src @amd-htcl/", &entry, &resolver);
        // .htcl files: cmd, ip. dirs with .htcl: cmd/, ip/.
        // scripts/ is omitted (no .htcl inside).
        assert_eq!(labels, vec!["cmd", "cmd/", "ip", "ip/"]);
    }

    #[test]
    fn lists_dep_subdirectory() {
        let (_dir, entry, resolver) = workspace_fixture();
        let labels = labels_for("src @amd-htcl/ip/", &entry, &resolver);
        // ip/ has bd/ (containing cell.htcl) — bd/ should show; no other entries.
        assert_eq!(labels, vec!["bd/"]);
    }

    #[test]
    fn partial_segment_replaces_only_the_segment() {
        // User has typed `src @amd-htcl/c` — replace should cover just
        // the `c`, not the whole `@amd-htcl/c`.
        let src = "src @amd-htcl/c";
        let (_dir, entry, resolver) = workspace_fixture();
        let line = cmdline::analyze(src, src.len() as u32);
        let idx = LineIndex::new(src);
        let items = src_path_completions(&entry, &line, &idx, &resolver);
        let labels: Vec<String> =
            items.iter().map(|c| c.label.clone()).collect();
        // Both `cmd` and `cmd/` start with `c`.
        assert!(labels.contains(&"cmd".to_string()), "{labels:?}");
        // The text-edit range should cover only the `c` (single char on line 0).
        let edit = match items[0].text_edit.as_ref() {
            Some(CompletionTextEdit::Edit(e)) => e,
            _ => panic!("expected text edit"),
        };
        assert_eq!(edit.range.start.character, 14, "{:?}", edit.range);
        assert_eq!(edit.range.end.character, 15);
    }

    #[test]
    fn dep_name_completion_when_no_slash_yet() {
        // Bare `@<dep>` is a complete import on its own, so the
        // completion shouldn't append `/` — selecting `@amd-htcl`
        // alone should leave valid syntax that resolves to
        // `<dep>/module.htcl`.
        let (_dir, entry, resolver) = workspace_fixture();
        let labels = labels_for("src @", &entry, &resolver);
        assert_eq!(labels, vec!["@amd-htcl"]);
    }

    #[test]
    fn dep_root_listing_hides_module_htcl() {
        // `module.htcl` is the default entry — already importable as
        // bare `@amd-htcl`, so it should not show up as `module` in
        // the per-dep file listing.
        let (_dir, entry, resolver) = workspace_fixture();
        let labels = labels_for("src @amd-htcl/", &entry, &resolver);
        assert!(!labels.contains(&"module".to_string()), "{labels:?}");
        // Sanity: the non-default modules still show.
        assert!(labels.contains(&"cmd".to_string()));
        assert!(labels.contains(&"ip".to_string()));
    }

    #[test]
    fn relative_completion_uses_entry_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("ip")).unwrap();
        fs::write(dir.path().join("ip/cips.htcl"), "# stub").unwrap();
        let entry = dir.path().join("prime.htcl");
        fs::write(&entry, "src ip/\n").unwrap();
        let resolver = Resolver::new();
        let labels = labels_for("src ip/", &entry, &resolver);
        assert_eq!(labels, vec!["cips"]);
    }
}
