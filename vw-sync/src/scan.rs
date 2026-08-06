//! Turning a directory into a manifest.

use camino::{Utf8Path, Utf8PathBuf};
use ignore::WalkBuilder;
use vw_api_types_versions::latest::{FileEntry, TreeManifest};

/// Directories never synchronized, whatever the ignore files say.
///
/// A vw workspace puts every generated artifact under a `target` directory —
/// vivado's synthesis output at the root, cargo's under each crate — and both
/// are already in a `.gitignore`. This is a floor underneath that, so a
/// one-line edit to an ignore file cannot turn a keystroke into a transfer of
/// somebody's entire synthesis run.
///
/// `.git` is here for the same reason rather than for size: the receiver has
/// no use for history, and a half-copied object store is worse than none.
pub const ALWAYS_IGNORED: [&str; 2] = [BUILD_OUTPUT, ".git"];

/// The directory a build writes its output to.
///
/// Named once here because two things depend on knowing it: synchronization,
/// which must never send or delete it, and `vw clean`, whose entire job is to
/// delete it. Those are opposite behaviours over the same directory, and them
/// disagreeing about which directory would be a bad afternoon either way.
pub const BUILD_OUTPUT: &str = "target";

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("walking {0}")]
    Walk(Utf8PathBuf, #[source] ignore::Error),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("{0} is not valid utf-8, which a manifest path has to be")]
    NotUtf8(std::path::PathBuf),
}

/// Describe every file under `root` that should be synchronized.
///
/// Honours `.gitignore` hierarchically, so a nested crate's own ignore rules
/// apply to its subtree the way git would read them, on top of
/// [`ALWAYS_IGNORED`].
///
/// Entries come back sorted by path. That is not cosmetic: a manifest is
/// compared and hashed by consumers, and a walk order that varies with the
/// filesystem would make identical trees look different.
pub fn scan(root: &Utf8Path) -> Result<TreeManifest, ScanError> {
    let mut entries = Vec::new();

    let mut walker = WalkBuilder::new(root);
    walker
        // Read .gitignore files, including nested ones, but do not require the
        // tree to be a git repository — a synchronized copy on the receiver is
        // not one, and it still has to reach the same answer.
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .hidden(false)
        .parents(false)
        // One predicate covering every name: `filter_entry` keeps only the
        // last closure it is given, so a loop calling it per name would
        // silently apply just the final one.
        .filter_entry(|entry| {
            !ALWAYS_IGNORED
                .iter()
                .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
        });

    for entry in walker.build() {
        let entry = entry.map_err(|e| ScanError::Walk(root.to_owned(), e))?;

        // Directories are implied by the paths of the files in them, and
        // anything that is neither a file nor a directory — a socket, a fifo —
        // has no meaning on the far end.
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }

        let path = Utf8Path::from_path(entry.path())
            .ok_or_else(|| ScanError::NotUtf8(entry.path().to_owned()))?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| ScanError::NotUtf8(path.as_std_path().to_owned()))?;

        let contents = std::fs::read(path)
            .map_err(|e| ScanError::Read(path.to_owned(), e))?;

        entries.push(FileEntry {
            path: relative.as_str().to_owned(),
            digest: crate::digest_bytes(&contents),
            executable: is_executable(path),
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TreeManifest { entries })
}

#[cfg(unix)]
fn is_executable(path: &Utf8Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Utf8Path) -> bool {
    false
}
