// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Classification and resolution of `src` import paths.
//!
//! The plan defines three path shapes:
//!
//! - `relative/path` — relative to the importing file's directory.
//! - `/absolute/path` — filesystem-absolute (allowed but discouraged).
//! - `@name/path` — resolved via `vw.toml`'s `[dependencies.<name>]`
//!   entry; the cached repo root comes from `vw-lib`'s dependency
//!   resolver and `<name>` plus the rest of the path identify a file
//!   in that repo.
//!
//! Resolution is split into two stages so the parser/AST side has no
//! filesystem dependency: [`classify`] decides which shape a path is,
//! [`Resolver`] turns a classified path into an actual on-disk file.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// Relative to the importing file's directory.
    Relative,
    /// Filesystem-absolute (starts with `/`).
    Absolute,
    /// Resolved via a workspace dependency named `name`. `subpath` is
    /// the rest of the path after `@name/` (may be empty).
    Named { name: String, subpath: String },
}

#[derive(Clone, Debug)]
pub struct ClassifiedPath<'a> {
    pub kind: PathKind,
    /// The original path text, retained for diagnostics.
    pub raw: &'a str,
}

/// Classify an import path. Doesn't touch the filesystem.
pub fn classify(path: &str) -> ClassifiedPath<'_> {
    let kind = if let Some(rest) = path.strip_prefix('@') {
        let (name, subpath) = match rest.split_once('/') {
            Some((n, s)) => (n.to_string(), s.to_string()),
            None => (rest.to_string(), String::new()),
        };
        PathKind::Named { name, subpath }
    } else if path.starts_with('/') {
        PathKind::Absolute
    } else {
        PathKind::Relative
    };
    ClassifiedPath { kind, raw: path }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(
        "unknown dependency `{name}` in `src @{name}{}`; \
         add a `[dependencies.{name}]` entry to your workspace's \
         vw.toml or run `vw add` to fetch it",
        if .subpath.is_empty() { String::new() } else { format!("/{}", .subpath) }
    )]
    UnknownDependency { name: String, subpath: String },

    #[error("imported file does not exist: {path}")]
    NotFound { path: PathBuf },

    #[error(
        "import path `{raw}` reduces to an empty file path; \
         a `src` must name a real file"
    )]
    EmptyPath { raw: String },
}

/// Bare `src @<dep>` resolves to `<dep_root>/{DEFAULT_MODULE}.htcl`.
/// The convention is intentionally fixed (no `vw.toml` knob) so every
/// htcl module is laid out the same way — a reader can open
/// `module.htcl` and know they're at the entry point.
///
/// The same convention applies to any directory that appears in a
/// `src` path — `src ip` where `ip/` is a directory resolves to
/// `ip/{DEFAULT_MODULE}.htcl` (analogous to Rust's `mod foo;`
/// picking `foo/mod.rs` when `foo.rs` is absent).
pub const DEFAULT_MODULE: &str = "module";

/// Resolver that turns import paths into on-disk file paths. Construct
/// one per workspace and reuse it across imports.
///
/// Named deps are looked up in `cached_deps`, a `name → cache root`
/// map normally built from `vw.lock` via `vw-lib`. The caller is
/// responsible for filling this in — the htcl crate stays free of
/// `vw-lib` and filesystem-cache concerns.
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    cached_deps: std::collections::HashMap<String, PathBuf>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a dependency's cached root path (typically
    /// `~/.vw/deps/<name>-<sha>`).
    pub fn with_dep(mut self, name: impl Into<String>, root: PathBuf) -> Self {
        self.cached_deps.insert(name.into(), root);
        self
    }

    /// Same as [`with_dep`], but only registers `name` when no
    /// entry already exists. Cargo-parity semantic for
    /// self-injecting the enclosing workspace as `@<workspace_name>`
    /// — a user-declared dep with the same name (rare but
    /// possible) still wins.
    pub fn with_dep_if_absent(
        mut self,
        name: impl Into<String>,
        root: PathBuf,
    ) -> Self {
        let name = name.into();
        self.cached_deps.entry(name).or_insert(root);
        self
    }

    /// Iterate the registered dependencies as `(name, root)` pairs.
    /// Order is unspecified — callers that care should sort.
    pub fn deps(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.cached_deps
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_path()))
    }

    /// Look up a dependency's cached root by name.
    pub fn dep_root(&self, name: &str) -> Option<&Path> {
        self.cached_deps.get(name).map(PathBuf::as_path)
    }

    /// Resolve `path` (as written in a `src` statement) against the
    /// directory containing the importing file. Returns the canonical
    /// path to the imported file, with `.htcl` appended if absent.
    pub fn resolve(
        &self,
        importing_file_dir: &Path,
        path: &str,
    ) -> Result<PathBuf, ResolveError> {
        let classified = classify(path);
        let candidate = match &classified.kind {
            PathKind::Relative => importing_file_dir.join(path),
            PathKind::Absolute => PathBuf::from(path),
            PathKind::Named { name, subpath } => {
                let Some(root) = self.cached_deps.get(name) else {
                    return Err(ResolveError::UnknownDependency {
                        name: name.clone(),
                        subpath: subpath.clone(),
                    });
                };
                // Bare `@<dep>` resolves to the dep's default entry
                // point — `module.htcl` at the dep root, analogous to
                // Rust's `src/lib.rs`. `@<dep>/<sub>` still picks a
                // specific module under the dep.
                if subpath.is_empty() {
                    root.join(DEFAULT_MODULE)
                } else {
                    root.join(subpath)
                }
            }
        };

        // Directory-as-module: `src ip` where `ip/` is a real
        // directory containing `module.htcl` resolves to
        // `ip/module.htcl`. Mirrors the bare-`@dep` behavior — a
        // dep root and an in-tree subdirectory both use
        // `module.htcl` as the entry point — and gives users the
        // Rust-style choice between `foo.htcl` and
        // `foo/module.htcl` for a growing module. Checked BEFORE
        // the `.htcl` append so a directory with a sibling
        // `<name>.htcl` file favors the file (predictable when
        // both happen to exist during a rename).
        if candidate.extension().is_none() {
            let with_ext = candidate.with_extension("htcl");
            if with_ext.exists() {
                return Ok(with_ext.canonicalize().unwrap_or(with_ext));
            }
            if candidate.is_dir() {
                let module =
                    candidate.join(DEFAULT_MODULE).with_extension("htcl");
                if module.exists() {
                    return Ok(module.canonicalize().unwrap_or(module));
                }
                // Directory exists but has no module.htcl — surface
                // the module path in the error so the fix is obvious
                // ("create ip/module.htcl") rather than pointing at
                // the sibling `.htcl` we tried first.
                return Err(ResolveError::NotFound { path: module });
            }
            return Err(ResolveError::NotFound { path: with_ext });
        }

        // Path already carries an extension — take it verbatim.
        if !candidate.exists() {
            return Err(ResolveError::NotFound { path: candidate });
        }
        Ok(candidate.canonicalize().unwrap_or(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn classify_relative() {
        assert_eq!(classify("foo/bar").kind, PathKind::Relative);
        assert_eq!(classify("bar").kind, PathKind::Relative);
    }

    #[test]
    fn classify_absolute() {
        assert_eq!(classify("/opt/x/y").kind, PathKind::Absolute);
    }

    #[test]
    fn classify_named() {
        assert_eq!(
            classify("@quartz/ip/bacd").kind,
            PathKind::Named {
                name: "quartz".into(),
                subpath: "ip/bacd".into()
            }
        );
        assert_eq!(
            classify("@bare").kind,
            PathKind::Named {
                name: "bare".into(),
                subpath: String::new()
            }
        );
    }

    fn fixture() -> (tempfile::TempDir, Resolver) {
        let dir = tempfile::tempdir().unwrap();
        let dep_root = dir.path().join("dep");
        fs::create_dir_all(dep_root.join("ip")).unwrap();
        fs::write(dep_root.join("ip").join("bacd.htcl"), "## stub\n").unwrap();
        fs::write(dir.path().join("local.htcl"), "## local\n").unwrap();
        let resolver = Resolver::new().with_dep("quartz", dep_root);
        (dir, resolver)
    }

    #[test]
    fn resolve_relative_appends_htcl() {
        let (dir, resolver) = fixture();
        let resolved = resolver.resolve(dir.path(), "local").unwrap();
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some("local.htcl")
        );
    }

    #[test]
    fn resolve_named_dependency() {
        let (dir, resolver) = fixture();
        let resolved = resolver.resolve(dir.path(), "@quartz/ip/bacd").unwrap();
        assert!(resolved.ends_with("dep/ip/bacd.htcl"), "{resolved:?}");
    }

    #[test]
    fn bare_named_dep_resolves_to_module_htcl() {
        // `src @quartz` → `<dep_root>/module.htcl` (analogous to
        // Rust's `use crate` resolving to `src/lib.rs`).
        let dir = tempfile::tempdir().unwrap();
        let dep_root = dir.path().join("dep");
        fs::create_dir_all(&dep_root).unwrap();
        fs::write(dep_root.join("module.htcl"), "# entry\n").unwrap();
        let resolver = Resolver::new().with_dep("quartz", dep_root.clone());
        let resolved = resolver.resolve(dir.path(), "@quartz").unwrap();
        assert!(resolved.ends_with("dep/module.htcl"), "{resolved:?}");
    }

    #[test]
    fn unknown_dep_errors_cleanly() {
        let (dir, resolver) = fixture();
        let err = resolver.resolve(dir.path(), "@nope/foo").unwrap_err();
        assert!(
            matches!(err, ResolveError::UnknownDependency { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn missing_file_errors() {
        let (dir, resolver) = fixture();
        let err = resolver.resolve(dir.path(), "does/not/exist").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn directory_with_module_htcl_resolves() {
        // `src ip` where `ip/` is a directory with `module.htcl`
        // inside → `ip/module.htcl`. The layout the metroid
        // project switched to after reorganizing per-IP files
        // into per-IP directories.
        let dir = tempfile::tempdir().unwrap();
        let ip = dir.path().join("ip");
        fs::create_dir_all(&ip).unwrap();
        fs::write(ip.join("module.htcl"), "# entry\n").unwrap();
        let resolver = Resolver::new();
        let resolved = resolver.resolve(dir.path(), "ip").unwrap();
        assert!(resolved.ends_with("ip/module.htcl"), "{resolved:?}");
    }

    #[test]
    fn sibling_htcl_wins_over_directory_module() {
        // When both `foo.htcl` and `foo/module.htcl` exist, prefer
        // the file. Predictable during a rename: users incrementally
        // migrating a single-file module to a directory don't get
        // a surprise resolution swap the moment the directory
        // sprouts a `module.htcl`.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("foo")).unwrap();
        fs::write(dir.path().join("foo.htcl"), "# file\n").unwrap();
        fs::write(dir.path().join("foo").join("module.htcl"), "# dir\n")
            .unwrap();
        let resolver = Resolver::new();
        let resolved = resolver.resolve(dir.path(), "foo").unwrap();
        assert!(resolved.ends_with("foo.htcl"), "{resolved:?}");
        assert!(!resolved.ends_with("module.htcl"), "{resolved:?}");
    }

    #[test]
    fn directory_without_module_htcl_errors_pointing_at_module() {
        // Directory exists but lacks `module.htcl` — the error
        // path should name the missing `module.htcl`, not the
        // sibling `.htcl` the resolver also considered. That's
        // what tells the user "add module.htcl here" instead of
        // "create a sibling file."
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("ip")).unwrap();
        let resolver = Resolver::new();
        let err = resolver.resolve(dir.path(), "ip").unwrap_err();
        match err {
            ResolveError::NotFound { path } => {
                assert!(path.ends_with("ip/module.htcl"), "{path:?}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn named_dep_subpath_can_be_directory_module() {
        // `src @quartz/ip` where the dep has `ip/module.htcl` —
        // same directory-as-module rule applies to subpaths of a
        // named dep, not just to workspace-local paths.
        let dir = tempfile::tempdir().unwrap();
        let dep_root = dir.path().join("dep");
        fs::create_dir_all(dep_root.join("ip")).unwrap();
        fs::write(dep_root.join("ip").join("module.htcl"), "# entry\n")
            .unwrap();
        let resolver = Resolver::new().with_dep("quartz", dep_root);
        let resolved = resolver.resolve(dir.path(), "@quartz/ip").unwrap();
        assert!(resolved.ends_with("dep/ip/module.htcl"), "{resolved:?}");
    }

    #[test]
    fn with_dep_if_absent_leaves_existing_alone() {
        // A user-declared dep of the same name must shadow the
        // self-injection — same policy Cargo uses for the crate-
        // self reference. Without this, a library that legitimately
        // depends on an external `foo` couldn't also self-reference.
        let existing = PathBuf::from("/tmp/existing");
        let new_path = PathBuf::from("/tmp/new");
        let resolver = Resolver::new()
            .with_dep("foo", existing.clone())
            .with_dep_if_absent("foo", new_path);
        assert_eq!(resolver.dep_root("foo"), Some(existing.as_path()));
    }

    #[test]
    fn with_dep_if_absent_registers_when_missing() {
        let path = PathBuf::from("/tmp/self");
        let resolver = Resolver::new().with_dep_if_absent("self", path.clone());
        assert_eq!(resolver.dep_root("self"), Some(path.as_path()));
    }

    #[test]
    fn self_referential_workspace_resolves() {
        // Simulate a library named `foo` that sources one of its
        // own sibling modules via `src @foo/bar`. The resolver has
        // `foo` self-injected to the workspace root, so `@foo/bar`
        // resolves to `<ws>/bar.htcl`.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bar.htcl"), "## sib\n").unwrap();
        fs::write(dir.path().join("vw.toml"), "[workspace]\nname = \"foo\"\n")
            .unwrap();
        let resolver =
            Resolver::new().with_dep_if_absent("foo", dir.path().to_path_buf());
        let resolved = resolver.resolve(dir.path(), "@foo/bar").unwrap();
        assert!(resolved.ends_with("bar.htcl"), "{resolved:?}");
    }
}
