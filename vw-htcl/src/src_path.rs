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
                if subpath.is_empty() {
                    return Err(ResolveError::EmptyPath {
                        raw: path.to_string(),
                    });
                }
                root.join(subpath)
            }
        };

        let with_ext = if candidate.extension().is_some() {
            candidate.clone()
        } else {
            candidate.with_extension("htcl")
        };

        if !with_ext.exists() {
            return Err(ResolveError::NotFound { path: with_ext });
        }
        Ok(with_ext.canonicalize().unwrap_or(with_ext))
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
}
