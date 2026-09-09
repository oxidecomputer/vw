// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Publishing dependencies into the shared cache.
//!
//! `$HOME/.vw/deps` is one directory shared by every workspace on a
//! machine, and a dependency in it is named `<name>-<sha>` — content
//! addressed, so two workspaces that want the same commit want the
//! same directory. That sharing is the point: it is what stops a
//! second checkout of a project paying for a download the first one
//! already made.
//!
//! It also means two processes can arrive at one path at the same
//! moment, which is what this module exists to make safe. Three rules
//! do it.
//!
//! **A download is assembled somewhere private.** Every one gets its
//! own directory under [`STAGING`], named with a random suffix so no
//! two can collide. Nothing else ever looks inside one, so there is
//! no shared state there to corrupt.
//!
//! **A dependency appears at its final path complete or not at all.**
//! Publishing is a `rename`, which is atomic within a filesystem —
//! which is why staging sits beside the cache rather than in `/tmp`,
//! where the rename would cross a device boundary and quietly stop
//! being one.
//!
//! **A published tree is never written again.** Nothing modifies a
//! dependency in place, so any number of readers can use one with no
//! coordination whatsoever. The single exception is a directory with
//! no [`COMPLETE_MARKER`] in it, which by definition no reader will
//! touch — that is what makes clearing one away safe.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{Result, VwError};

/// Written inside a staging directory as the last act before it is
/// published, so it can only ever be seen on a finished tree.
///
/// Completeness is stated rather than guessed at. The obvious test —
/// "the directory has something in it" — is true of a download that
/// has written its first file and has hours left to run, which is how
/// a half-extracted dependency gets built against.
pub const COMPLETE_MARKER: &str = ".vw-complete";

/// Where downloads are assembled, relative to the cache root.
///
/// Under the cache rather than beside it so that publishing is a
/// rename within one filesystem. Hidden so that the walks which
/// enumerate the cache — all of which match on a `<name>-` prefix —
/// have no chance of taking it for a dependency.
const STAGING: &str = ".staging";

/// How long a staging directory is left alone before [`sweep`] treats
/// it as abandoned.
///
/// Deliberately generous, because this is a safety property rather
/// than a tuning knob: a staging directory that is still being
/// written to belongs to a live download, and removing one would
/// reintroduce exactly the corruption the rest of this module
/// removes. Every ordinary ending — success, failure, `?` — takes its
/// own staging directory with it when [`Staged`] drops, so this only
/// ever governs what a killed process left behind, and waiting a day
/// to reclaim a few hundred megabytes of that costs nothing.
const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether the cache holds a finished dependency at `path`.
///
/// The only question worth asking about a cache entry, and the only
/// one any reader should ask. A directory that answers `false` is not
/// half a dependency to be repaired — it is one nobody is entitled to
/// read, which is what lets a writer replace it.
pub fn is_complete(path: &Path) -> bool {
    path.join(COMPLETE_MARKER).is_file()
}

/// A private directory for one download, removed when it drops.
///
/// The dropping is most of the value. A download that fails, times
/// out, or returns early through `?` leaves nothing behind, so the
/// only way to orphan a staging directory is to kill the process
/// outright — which is what makes [`sweep`]'s deadline a backstop
/// rather than the mechanism.
pub(crate) struct Staged {
    dir: tempfile::TempDir,
}

impl Staged {
    /// Claim a staging directory under `deps_dir`.
    pub(crate) fn new(deps_dir: &Path) -> Result<Staged> {
        let staging = deps_dir.join(STAGING);
        fs::create_dir_all(&staging).map_err(|e| VwError::FileSystem {
            message: format!(
                "Failed to create staging directory {}: {e}",
                staging.display()
            ),
        })?;

        // The random suffix is the whole mechanism: two processes
        // fetching one dependency have to get two directories, or
        // they are back to writing over each other one level down.
        let dir = tempfile::Builder::new()
            .prefix("dep-")
            .tempdir_in(&staging)
            .map_err(|e| VwError::FileSystem {
                message: format!(
                    "Failed to create a staging directory in {}: {e}",
                    staging.display()
                ),
            })?;

        Ok(Staged { dir })
    }

    /// Where the download should write.
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Move this download to `destination`, where readers will find
    /// it.
    ///
    /// Losing a race is a success, not a failure: another process
    /// publishing the same `<name>-<sha>` published the same bytes,
    /// by construction, so there is nothing to reconcile and nothing
    /// to report. Ours goes away with `self`.
    pub(crate) fn publish(self, destination: &Path) -> Result<()> {
        let staged = self.dir.path().to_path_buf();

        // Last, so that it cannot appear on anything unfinished — and
        // inside, so that it arrives with the tree rather than after
        // it.
        fs::write(staged.join(COMPLETE_MARKER), []).map_err(|e| {
            VwError::FileSystem {
                message: format!(
                    "Failed to mark {} complete: {e}",
                    staged.display()
                ),
            }
        })?;

        if fs::rename(&staged, destination).is_ok() {
            // No longer ours to clean up.
            let _ = self.dir.keep();
            return Ok(());
        }

        // A rename onto a directory fails when there is already one
        // there, which is the only failure worth distinguishing.
        if is_complete(destination) {
            return Ok(());
        }

        // Something is in the way that no reader considers usable: a
        // tree left by a vw that predates this module, or by a
        // download killed midway. Nobody can be reading it, so it can
        // go.
        let _ = fs::remove_dir_all(destination);

        match fs::rename(&staged, destination) {
            Ok(()) => {
                let _ = self.dir.keep();
                Ok(())
            }
            // Somebody published into the gap we just made. Theirs is
            // as good as ours.
            Err(_) if is_complete(destination) => Ok(()),
            Err(e) => Err(VwError::FileSystem {
                message: format!(
                    "Failed to publish {} to {}: {e}",
                    staged.display(),
                    destination.display()
                ),
            }),
        }
    }
}

/// Remove staging directories that no process is using any more.
///
/// Only reachable after a process was killed outright — anything
/// gentler cleans up after itself — so this normally has nothing to
/// do and costs one `readdir` of an empty directory.
///
/// Best effort throughout. Two of these running at once will both
/// find the same abandoned directory and one will lose, and a cache
/// that cannot be tidied is not a reason to fail somebody's build.
pub(crate) fn sweep(deps_dir: &Path) {
    let staging = deps_dir.join(STAGING);
    let Ok(entries) = fs::read_dir(&staging) else {
        return;
    };

    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        if !path.is_dir() {
            continue;
        }

        // A modification time in the future means a clock that moved,
        // not a directory that is old; `duration_since` fails and the
        // directory is left alone, which is the right way to be
        // wrong.
        let abandoned = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > ORPHAN_GRACE);

        if abandoned {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A cache directory, and a file to stand in for a downloaded
    /// dependency's contents.
    fn cache() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn a_published_dependency_is_complete() {
        let cache = cache();
        let staged = Staged::new(cache.path()).unwrap();
        write(staged.path(), "module.htcl", "contents");

        let destination = cache.path().join("quartz-abc");
        staged.publish(&destination).unwrap();

        assert!(is_complete(&destination));
        assert_eq!(
            fs::read_to_string(destination.join("module.htcl")).unwrap(),
            "contents"
        );
    }

    /// The whole reason the deadline in [`sweep`] is a backstop rather
    /// than the mechanism: an ordinary ending takes its own staging
    /// directory with it.
    #[test]
    fn a_staging_directory_goes_away_when_it_is_dropped() {
        let cache = cache();
        let path = {
            let staged = Staged::new(cache.path()).unwrap();
            write(staged.path(), "half-a-file", "…");
            staged.path().to_path_buf()
        };

        assert!(!path.exists());
    }

    /// The failure this module exists to remove. A tree that is merely
    /// present is not a tree anybody may read.
    #[test]
    fn a_download_in_flight_is_not_complete() {
        let cache = cache();
        let destination = cache.path().join("quartz-abc");
        write(&destination, "first-of-many.vhd", "…");

        assert!(!is_complete(&destination));
    }

    /// Two processes fetching one dependency have to get two
    /// directories. Sharing one is the original bug.
    #[test]
    fn two_downloads_of_one_dependency_are_kept_apart() {
        let cache = cache();
        let first = Staged::new(cache.path()).unwrap();
        let second = Staged::new(cache.path()).unwrap();

        assert_ne!(first.path(), second.path());
    }

    /// Losing the race is an ordinary outcome, not a failure: the
    /// winner published the same commit, so there is nothing to
    /// reconcile.
    #[test]
    fn losing_a_publish_race_leaves_the_winner_alone() {
        let cache = cache();
        let destination = cache.path().join("quartz-abc");

        let winner = Staged::new(cache.path()).unwrap();
        write(winner.path(), "module.htcl", "winner");
        winner.publish(&destination).unwrap();

        let loser = Staged::new(cache.path()).unwrap();
        write(loser.path(), "module.htcl", "loser");
        let loser_path = loser.path().to_path_buf();
        loser.publish(&destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("module.htcl")).unwrap(),
            "winner"
        );
        // And the redundant copy did not become garbage.
        assert!(!loser_path.exists());
    }

    /// What a vw that predates this module could leave behind, and
    /// what a killed download left before there was a marker to go by.
    /// Nothing considers it usable, so publishing over it is safe.
    #[test]
    fn a_partial_from_an_older_vw_is_replaced() {
        let cache = cache();
        let destination = cache.path().join("quartz-abc");
        write(&destination, "truncated.vhd", "half");

        let staged = Staged::new(cache.path()).unwrap();
        write(staged.path(), "module.htcl", "whole");
        staged.publish(&destination).unwrap();

        assert!(is_complete(&destination));
        assert_eq!(
            fs::read_to_string(destination.join("module.htcl")).unwrap(),
            "whole"
        );
        assert!(!destination.join("truncated.vhd").exists());
    }

    /// A live download is exactly what a staging directory looks like,
    /// so a sweep that went by presence rather than age would delete
    /// one — reintroducing the corruption from the other side.
    #[test]
    fn a_sweep_leaves_a_live_download_alone() {
        let cache = cache();
        let staged = Staged::new(cache.path()).unwrap();
        write(staged.path(), "arriving.vhd", "…");

        sweep(cache.path());

        assert!(staged.path().exists());
    }

    #[test]
    fn sweeping_a_cache_that_has_never_staged_anything_is_fine() {
        let cache = cache();
        sweep(cache.path());
    }

    /// The marker is what `clear_cache` retracts before it removes a
    /// tree, so that a reader arriving mid-removal fetches its own
    /// copy instead of reading one that is going away.
    #[test]
    fn retracting_the_marker_makes_a_tree_unusable() {
        let cache = cache();
        let staged = Staged::new(cache.path()).unwrap();
        write(staged.path(), "module.htcl", "contents");
        let destination = cache.path().join("quartz-abc");
        staged.publish(&destination).unwrap();

        fs::remove_file(destination.join(COMPLETE_MARKER)).unwrap();

        assert!(!is_complete(&destination));
    }
}
