//! Making a directory match a manifest.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use vw_api_types_versions::latest::{
    CommitResult, Digest, FileEntry, SyncPlan, TreeManifest,
};

use crate::{scan, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("'{0}' is not a path a manifest may name")]
    UnsafePath(String),
    #[error("scanning the tree at {0}")]
    Scan(Utf8PathBuf, #[source] crate::ScanError),
    #[error("no content for {digest}, wanted at {path}")]
    MissingContent { path: String, digest: Digest },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("creating {0}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("removing {0}")]
    Remove(Utf8PathBuf, #[source] std::io::Error),
}

/// The content a manifest needs that is nowhere to be found.
///
/// Content already sitting somewhere in the tree is not reported, whatever
/// path it is under. A rename or a directory restructure therefore costs
/// nothing over the wire — the bytes are already here, and applying the
/// manifest copies them into their new place locally.
pub fn missing(
    root: &Utf8Path,
    store: &Store,
    manifest: &TreeManifest,
) -> Result<SyncPlan, ApplyError> {
    let held = held_content(root)?;

    let mut missing: Vec<Digest> = manifest
        .entries
        .iter()
        .map(|entry| &entry.digest)
        .filter(|digest| !held.contains_key(*digest) && !store.has(digest))
        .cloned()
        .collect();

    // One request per digest, however many paths want it.
    missing.sort();
    missing.dedup();

    Ok(SyncPlan { missing })
}

/// Make the tree at `root` match `manifest`.
///
/// Writes and updates first, then removes what the manifest does not mention.
/// That order is what makes a rename work: the content of the old path is
/// still there to be copied to the new one when it is needed.
///
/// Only files the scan can see are candidates for removal, so anything a build
/// produced — everything under a `target` directory, anything a `.gitignore`
/// covers — is left alone. The receiver reads those rules from the tree it was
/// handed, which is why they cannot disagree with the sender's.
pub fn apply(
    root: &Utf8Path,
    store: &Store,
    manifest: &TreeManifest,
) -> Result<CommitResult, ApplyError> {
    for entry in &manifest.entries {
        check_path(&entry.path)?;
    }

    let held = held_content(root)?;
    let existing =
        scan::scan(root).map_err(|e| ApplyError::Scan(root.to_owned(), e))?;
    let current: BTreeMap<&str, &FileEntry> = existing
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    let mut result = CommitResult::default();

    for entry in &manifest.entries {
        let path = root.join(&entry.path);

        match current.get(entry.path.as_str()) {
            Some(have)
                if have.digest == entry.digest
                    && have.executable == entry.executable =>
            {
                result.unchanged += 1;
                continue;
            }
            Some(_) => result.updated += 1,
            None => result.created += 1,
        }

        let contents = content_for(entry, store, &held)?;
        write_file(&path, &contents, entry.executable)?;
    }

    // Everything the manifest does not ask for.
    let wanted: BTreeMap<&str, ()> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), ()))
        .collect();
    for entry in &existing.entries {
        if wanted.contains_key(entry.path.as_str()) {
            continue;
        }
        let path = root.join(&entry.path);
        std::fs::remove_file(&path).map_err(|e| ApplyError::Remove(path, e))?;
        result.deleted += 1;
    }

    if result.deleted > 0 {
        prune_empty_dirs(root)?;
    }

    Ok(result)
}

/// Discard everything synchronization has put here.
///
/// The tree is made to match an empty manifest and the content store is
/// emptied. Between them that removes every trace of what a sender last said,
/// while leaving anything a build produced exactly where it was — the same
/// rules decide what may be deleted here as anywhere else.
///
/// Nothing needs this to stay correct. A commit already replaces whatever
/// differs from the manifest, so an ordinary sync is enough to fix a tree that
/// is merely out of date. This is for the case where the receiver's account of
/// itself is the thing in doubt: with nothing held and nothing in the tree,
/// there is no account left to be wrong, and the sync that follows sends the
/// whole tree because it genuinely is all missing.
pub fn clear(
    root: &Utf8Path,
    store: &Store,
) -> Result<CommitResult, ApplyError> {
    store.empty()?;
    apply(root, store, &TreeManifest::default())
}

/// Content the tree already holds, indexed by digest rather than by path.
///
/// By digest because that is the question worth asking: whether the bytes are
/// here at all, not whether they are here under the name they are wanted
/// under.
fn held_content(
    root: &Utf8Path,
) -> Result<BTreeMap<Digest, Utf8PathBuf>, ApplyError> {
    if !root.is_dir() {
        return Ok(BTreeMap::new());
    }

    let manifest =
        scan::scan(root).map_err(|e| ApplyError::Scan(root.to_owned(), e))?;

    Ok(manifest
        .entries
        .into_iter()
        .map(|entry| (entry.digest, root.join(entry.path)))
        .collect())
}

fn content_for(
    entry: &FileEntry,
    store: &Store,
    held: &BTreeMap<Digest, Utf8PathBuf>,
) -> Result<Vec<u8>, ApplyError> {
    // Delivered content first: it is what a sender just took the trouble to
    // upload, so preferring it keeps a freshly delivered file from being
    // shadowed by a stale copy that happens to collide.
    if store.has(&entry.digest) {
        return Ok(store.get(&entry.digest)?);
    }

    // Otherwise it may already be in the tree under another name.
    if let Some(source) = held.get(&entry.digest) {
        return std::fs::read(source)
            .map_err(|e| ApplyError::Write(source.clone(), e));
    }

    Err(ApplyError::MissingContent {
        path: entry.path.clone(),
        digest: entry.digest.clone(),
    })
}

fn write_file(
    path: &Utf8Path,
    contents: &[u8],
    executable: bool,
) -> Result<(), ApplyError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApplyError::CreateDir(parent.to_owned(), e))?;
    }

    // Removed rather than truncated: a source file checked out read-only
    // cannot be opened for writing even by its owner, and replacing it is the
    // whole point.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ApplyError::Remove(path.to_owned(), e)),
    }

    std::fs::write(path, contents)
        .map_err(|e| ApplyError::Write(path.to_owned(), e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| ApplyError::Write(path.to_owned(), e))?;
    }
    #[cfg(not(unix))]
    let _ = executable;

    Ok(())
}

/// Reject anything that would put a file outside the tree.
///
/// A manifest arrives over the wire and its paths become filesystem paths, so
/// `../../.ssh/authorized_keys` has to stop here. Absolute paths are refused
/// for the same reason, and a component of `.` is refused because it is only
/// ever noise in a path a scan produced.
fn check_path(path: &str) -> Result<(), ApplyError> {
    let unsafe_path = || ApplyError::UnsafePath(path.to_owned());

    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(unsafe_path());
    }
    // Windows drive letters would be absolute over there and are meaningless
    // here either way.
    if path.chars().nth(1) == Some(':') {
        return Err(unsafe_path());
    }
    for component in path.split('/') {
        if component.is_empty() || component == ".." || component == "." {
            return Err(unsafe_path());
        }
    }

    Ok(())
}

/// Remove directories left behind with nothing in them.
///
/// Deleting the last file in a directory otherwise leaves the directory, and a
/// build tool that globs would keep finding a package that no longer has any
/// sources in it.
fn prune_empty_dirs(root: &Utf8Path) -> Result<(), ApplyError> {
    // Depth first, so a directory whose only content was other now-empty
    // directories is caught in the same pass.
    let mut directories = Vec::new();
    collect_dirs(root, &mut directories)?;
    directories
        .sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    for directory in directories {
        if directory == root {
            continue;
        }
        let empty = std::fs::read_dir(&directory)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if empty {
            std::fs::remove_dir(&directory)
                .map_err(|e| ApplyError::Remove(directory, e))?;
        }
    }

    Ok(())
}

fn collect_dirs(
    root: &Utf8Path,
    into: &mut Vec<Utf8PathBuf>,
) -> Result<(), ApplyError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        // A build's output directory is not ours to tidy.
        if crate::ALWAYS_IGNORED
            .iter()
            .any(|name| path.file_name() == Some(*name))
        {
            continue;
        }
        collect_dirs(&path, into)?;
        into.push(path);
    }

    Ok(())
}

/// What removing a tree's build output came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// Whether there was anything there.
    pub existed: bool,
    /// How much space it was taking.
    ///
    /// Measured before removal rather than inferred from free space, which on
    /// a shared instance is being moved by other things at the same time.
    pub bytes: u64,
}

/// Remove everything a build wrote under `root`.
///
/// The counterpart to what synchronization refuses to touch: the same
/// directory it will never send and never delete is the one this exists to
/// delete, and both read the name from the same place.
///
/// Source is left alone. A cleaned tree is one a build starts over in, not one
/// that has to be pushed again.
pub fn clean(root: &Utf8Path) -> Result<Cleaned, ApplyError> {
    let output = root.join(crate::BUILD_OUTPUT);
    if !output.is_dir() {
        return Ok(Cleaned::default());
    }

    let bytes = size_of(&output);
    std::fs::remove_dir_all(&output)
        .map_err(|e| ApplyError::Remove(output, e))?;

    Ok(Cleaned {
        existed: true,
        bytes,
    })
}

/// How much a directory holds, following nothing.
///
/// Symlinks are counted as themselves rather than followed: a build that
/// linked to something outside its own output should not have that thing's
/// size attributed to it, and following one out of the tree to measure it
/// would be a strange thing to do on the way to a delete.
fn size_of(path: &Utf8Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };

    entries
        .flatten()
        .map(|entry| {
            let Ok(metadata) = entry.metadata() else {
                return 0;
            };
            if metadata.is_dir() {
                Utf8PathBuf::from_path_buf(entry.path())
                    .map(|child| size_of(&child))
                    .unwrap_or(0)
            } else {
                metadata.len()
            }
        })
        .sum()
}
