//! Where delivered content waits between arriving and being put in place.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};
use vw_api_types_versions::latest::Digest;

/// Where a store records which paths synchronization put in its tree.
///
/// In the store because it describes the store's own deliveries, and because
/// emptying the store is exactly when it should be forgotten. Content is kept
/// under two-character shard directories, so a name like this cannot collide
/// with it.
const APPLIED: &str = "applied";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("'{0}' is not a well formed digest")]
    MalformedDigest(Digest),
    #[error("content for {0} does not match its digest")]
    DigestMismatch(Digest),
    #[error("creating {0}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("emptying {0}")]
    Empty(Utf8PathBuf, #[source] std::io::Error),
}

/// Content held by digest, waiting to be placed into a tree.
///
/// Only a staging area. A commit copies out of it into the working tree, and
/// nothing reads through it afterwards — so it can be emptied at any time and
/// the next sync will simply re-deliver what it needs.
pub struct Store {
    root: Utf8PathBuf,
}

impl Store {
    pub fn new(root: impl Into<Utf8PathBuf>) -> Store {
        Store { root: root.into() }
    }

    /// Whether this content is already held.
    pub fn has(&self, digest: &Digest) -> bool {
        self.path(digest).is_ok_and(|path| path.is_file())
    }

    /// Take delivery of content.
    ///
    /// The digest is verified rather than trusted. It names the file the
    /// content is written to, and every later lookup goes by digest, so
    /// accepting a mismatch would poison the store with content that is wrong
    /// under a name that looks right.
    pub fn put(
        &self,
        digest: &Digest,
        contents: &[u8],
    ) -> Result<(), StoreError> {
        if crate::digest_bytes(contents) != *digest {
            return Err(StoreError::DigestMismatch(digest.clone()));
        }

        let path = self.path(digest)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::CreateDir(parent.to_owned(), e))?;
        }
        std::fs::write(&path, contents)
            .map_err(|e| StoreError::Write(path.clone(), e))?;

        Ok(())
    }

    /// Discard everything held.
    ///
    /// Nothing is lost that a sender cannot deliver again — that is what makes
    /// this safe to offer. A sender that no longer trusts what this store
    /// claims to have empties it, and the next plan asks for the whole tree.
    pub fn empty(&self) -> Result<(), StoreError> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Empty(self.root.clone(), e)),
        }
    }

    pub fn get(&self, digest: &Digest) -> Result<Vec<u8>, StoreError> {
        let path = self.path(digest)?;
        std::fs::read(&path).map_err(|e| StoreError::Read(path, e))
    }

    /// Where content with this digest lives.
    ///
    /// Sharded by the first two hex characters, so a store with a lot of
    /// content does not become one enormous directory.
    ///
    /// The digest is checked for shape first. It arrives over the wire and is
    /// about to become a path, so anything that is not 64 hex characters —
    /// `../../etc/whatever` being the interesting case — is refused here
    /// rather than allowed to escape the store.
    fn path(&self, digest: &Digest) -> Result<Utf8PathBuf, StoreError> {
        if !digest.is_well_formed() {
            return Err(StoreError::MalformedDigest(digest.clone()));
        }
        let (shard, rest) = digest.0.split_at(2);
        Ok(self.root.join(shard).join(rest))
    }

    /// The paths synchronization has put in the tree behind this store, or
    /// `None` if it has never said.
    ///
    /// `None` is a store from before this was kept, or one that was just
    /// emptied, and it is not the same answer as an empty set: an empty set
    /// is a tree that was told to hold nothing, while `None` is a tree nobody
    /// can account for.
    pub fn applied(&self) -> Result<Option<BTreeSet<String>>, StoreError> {
        let path = self.root.join(APPLIED);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                text.split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Read(path, e)),
        }
    }

    /// Record the paths synchronization has put in the tree.
    ///
    /// Written aside and renamed into place, so an agent that dies part way
    /// through leaves the previous record rather than half of this one — half
    /// a record is a list of files that will never be removed.
    pub fn remember_applied<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| StoreError::CreateDir(self.root.clone(), e))?;

        // Separated by NUL, the one byte a path can never contain.
        let mut text = String::new();
        for path in paths {
            text.push_str(path);
            text.push('\0');
        }

        let path = self.root.join(APPLIED);
        let staged = self.root.join(format!("{APPLIED}.new"));
        std::fs::write(&staged, text)
            .map_err(|e| StoreError::Write(staged.clone(), e))?;
        std::fs::rename(&staged, &path)
            .map_err(|e| StoreError::Write(path, e))?;

        Ok(())
    }

    /// Where this store keeps its content.
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }
}
