// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The workspaces one instance is holding.
//!
//! An environment used to be one workspace, so an instance was one
//! tree and one content store, both fixed at startup. Three instances
//! is a lot of rack to spend on a single checkout, so an environment
//! now holds as many workspaces as are synchronized to it and an
//! instance keeps one of everything per workspace: a tree, a store
//! standing behind it, somewhere to send its artifacts, and the lock
//! that stops two syncs interleaving.
//!
//! They are made as they are first asked for. Nothing declares which
//! workspaces an environment has — a workspace is here because
//! somebody synchronized one — so there is no list to consult and
//! nothing to keep in step with the filesystem. The filesystem is the
//! list.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use slog::{info, o, Logger};
use vw_api_types_versions::latest::{CleanResult, S3Credentials};
use vw_sync::Store;

use crate::artifacts;

/// Where a workspace's last commit time is written, inside its content
/// store.
///
/// In the store rather than in the tree because the tree is made to
/// match a manifest, and a file the manifest does not mention is a
/// file the next commit deletes. The store is the agent's own, and
/// keys in it are 64 hex characters sharded two deep, so a name like
/// this cannot collide with content.
///
/// Clearing a sync empties the store and takes this with it, which is
/// right: a workspace that has been cleared has no last commit.
const LAST_SYNCED: &str = "last-synced";

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspaceError {
    #[error("'{name}' is not a usable workspace name: {detail}")]
    BadName { name: String, detail: String },
    #[error("creating {0}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("removing {0}")]
    Remove(Utf8PathBuf, #[source] std::io::Error),
}

/// Everything an instance keeps on behalf of one workspace.
pub(crate) struct Workspace {
    /// Where its source tree lives.
    pub(crate) root: Utf8PathBuf,
    /// Content delivered towards that tree, waiting to be put in place.
    pub(crate) store: Store,
    /// Where its finished artifacts go, once anyone has said.
    pub(crate) artifact_target:
        tokio::sync::watch::Sender<Option<S3Credentials>>,
    /// How to ask its uploader to send everything and say when it is
    /// done. `None` on an instance that builds nothing.
    pub(crate) flushes: Option<tokio::sync::mpsc::Sender<artifacts::Flush>>,
    /// Held while its tree is being made to match a manifest.
    ///
    /// One lock per workspace rather than one for the instance. Two
    /// syncs of the same tree still take turns, which is the thing
    /// that matters; two syncs of different trees no longer wait on
    /// each other, and with several workspaces on one environment
    /// under `--watch` that is most of the time.
    pub(crate) materializing: tokio::sync::Mutex<()>,
    /// Where its last commit time is recorded.
    synced_at: Utf8PathBuf,
}

impl Workspace {
    /// Note that the tree has just been made to match a manifest.
    ///
    /// Best effort: a workspace whose sync time cannot be written is
    /// still synchronized, and the cost of saying so is one line of a
    /// listing being blank.
    pub(crate) fn mark_synced(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let _ = std::fs::create_dir_all(self.store.root());
        let _ = std::fs::write(&self.synced_at, now.to_string());
    }
}

/// The workspaces this instance holds, made as they are asked for.
pub(crate) struct Registry {
    /// One tree per workspace lives under here.
    trees: Utf8PathBuf,
    /// And one content store per workspace under here.
    stores: Utf8PathBuf,
    /// Where every workspace's artifact target is remembered.
    targets: Utf8PathBuf,
    /// Whether this instance produces artifacts, and so runs an
    /// uploader per workspace.
    builds: bool,
    log: Logger,
    live: tokio::sync::Mutex<BTreeMap<String, std::sync::Arc<Workspace>>>,
}

impl Registry {
    pub(crate) fn new(
        trees: Utf8PathBuf,
        stores: Utf8PathBuf,
        targets: Utf8PathBuf,
        builds: bool,
        log: Logger,
    ) -> Registry {
        Registry {
            trees,
            stores,
            targets,
            builds,
            log,
            live: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// The workspace called `name`, made if this is the first time
    /// anyone has asked for it.
    ///
    /// The name is validated here rather than taken on trust. It
    /// arrives over the wire and becomes a directory, and while
    /// `vw-svc` checks it too, a name that reaches a path is worth
    /// refusing twice — this is the side where getting it wrong means
    /// writing outside the tree.
    pub(crate) async fn open(
        &self,
        name: &str,
    ) -> Result<std::sync::Arc<Workspace>, WorkspaceError> {
        vw_lib::validate_workspace_name(name).map_err(|e| {
            WorkspaceError::BadName {
                name: name.to_owned(),
                detail: e.to_string(),
            }
        })?;

        let mut live = self.live.lock().await;
        if let Some(workspace) = live.get(name) {
            return Ok(workspace.clone());
        }

        let workspace = std::sync::Arc::new(self.make(name)?);
        live.insert(name.to_owned(), workspace.clone());
        Ok(workspace)
    }

    /// Build one workspace's state and start whatever it needs running.
    fn make(&self, name: &str) -> Result<Workspace, WorkspaceError> {
        let root = self.trees.join(name);
        let store_root = self.stores.join(name);
        for directory in [&root, &store_root] {
            std::fs::create_dir_all(directory)
                .map_err(|e| WorkspaceError::CreateDir(directory.clone(), e))?;
        }

        // What this workspace was last told, if anything. An instance
        // that reboots between two builds still knows where the first
        // one's output was going.
        let remembered = artifacts::recall(&self.targets)
            .inspect_err(|e| {
                slog::warn!(self.log, "cannot read the remembered targets";
                    slog_error_chain::InlineErrorChain::new(e),
                );
            })
            .unwrap_or_default()
            .remove(name);

        let (artifact_target, changes) =
            tokio::sync::watch::channel(remembered);

        let log = self.log.new(o!("workspace" => name.to_owned()));

        // One uploader per workspace, each walking only its own build
        // output. A single uploader over all of them would have to keep
        // one record of what had been sent for trees that go to
        // different buckets.
        let flushes = self.builds.then(|| {
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            tokio::spawn(artifacts::synchronize(
                root.clone(),
                changes,
                receiver,
                log.new(o!("task" => "artifacts")),
            ));
            sender
        });

        info!(log, "holding a workspace"; "root" => %root);

        Ok(Workspace {
            synced_at: store_root.join(LAST_SYNCED),
            root,
            store: Store::new(store_root),
            artifact_target,
            flushes,
            materializing: tokio::sync::Mutex::new(()),
        })
    }

    /// Take up again with every workspace already on disk.
    ///
    /// Run at startup, so an instance that rebooted between a build
    /// finishing and its artifacts going up starts the uploader that
    /// will send them, rather than waiting for somebody to touch that
    /// workspace again.
    pub(crate) async fn restore(&self) {
        for name in self.on_disk() {
            if let Err(e) = self.open(&name).await {
                slog::warn!(self.log, "ignoring something in the tree \
                                       directory that is not a workspace";
                    "name" => &name,
                    slog_error_chain::InlineErrorChain::new(&e),
                );
            }
        }
    }

    /// What this instance is holding.
    pub(crate) fn list(
        &self,
        measure: bool,
    ) -> Vec<vw_api_types_versions::latest::Workspace> {
        self.on_disk()
            .into_iter()
            .map(|name| {
                let root = self.trees.join(&name);
                vw_api_types_versions::latest::Workspace {
                    last_synced: self.synced_at(&name),
                    bytes: if measure { occupied(&root) } else { 0 },
                    name,
                }
            })
            .collect()
    }

    /// Remove a workspace and everything belonging to it.
    ///
    /// Its tree, the content delivered towards it, whatever a build
    /// wrote under it, and the note of where its artifacts went. Not
    /// the artifacts themselves — those are in a bucket on another
    /// instance, and only the service can see both.
    ///
    /// A workspace that was not here answers the same way, because the
    /// caller wanted it gone and it is.
    pub(crate) async fn forget(
        &self,
        name: &str,
    ) -> Result<CleanResult, WorkspaceError> {
        vw_lib::validate_workspace_name(name).map_err(|e| {
            WorkspaceError::BadName {
                name: name.to_owned(),
                detail: e.to_string(),
            }
        })?;

        // Out of the map first. Dropping the last handle drops the
        // channels this workspace's uploader is waiting on, which is
        // how that task learns to stop — before the directory it walks
        // goes away rather than after.
        let mut live = self.live.lock().await;
        live.remove(name);

        let root = self.trees.join(name);
        let store = self.stores.join(name);
        let bytes = occupied(&root);
        let existed = root.exists();

        for directory in [&root, &store] {
            match std::fs::remove_dir_all(directory) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(WorkspaceError::Remove(directory.clone(), e))
                }
            }
        }

        let _ = artifacts::forget(&self.targets, name);

        info!(self.log, "forgot a workspace";
            "workspace" => name,
            "existed" => existed,
            "bytes" => bytes,
        );

        Ok(CleanResult { existed, bytes })
    }

    /// The names of the trees on disk.
    fn on_disk(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.trees) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| vw_lib::validate_workspace_name(name).is_ok())
            .collect();
        names.sort();
        names
    }

    /// When `name` was last committed to.
    fn synced_at(&self, name: &str) -> Option<u64> {
        std::fs::read_to_string(self.stores.join(name).join(LAST_SYNCED))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

/// What everything under `root` adds up to.
///
/// Only asked for when somebody is deciding what to remove, because on
/// a vivado instance this walks a build tree that runs to gigabytes
/// across a great many files.
fn occupied(root: &Utf8Path) -> u64 {
    fn walk(path: &std::path::Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), total);
            } else if kind.is_file() {
                if let Ok(meta) = entry.metadata() {
                    *total += meta.len();
                }
            }
        }
    }

    let mut total = 0;
    walk(root.as_std_path(), &mut total);
    total
}
