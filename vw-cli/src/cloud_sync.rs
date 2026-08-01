// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pushing a workspace to the instances that build it.
//!
//! A workspace is split across an environment's two instances: vivado builds
//! the hardware, helios builds whatever drives it, and neither needs the
//! other's sources. Where the line falls is not a question a workspace gets to
//! answer — a vw workspace keeps its driver in `driver`, so that is what goes
//! to helios and everything else goes to vivado.
//!
//! Nothing to declare, nothing to keep in step with the layout, and no way to
//! have a workspace whose configuration disagrees with where its files
//! actually are.
//!
//! Each target is synchronized independently: its own manifest, its own
//! content, its own commit. Nothing is shared between them, so there is no
//! ordering to get right and a failure to reach one instance does not hold up
//! the other.

use camino::{Utf8Path, Utf8PathBuf};
use colored::*;

use futures::{StreamExt, TryStreamExt};

use crate::cloud::CloudError;

/// How many pieces of content are sent at once.
///
/// Enough to keep the link busy across a slow round trip, few enough not to
/// look like an attack to anything in between.
const UPLOAD_CONCURRENCY: usize = 16;
use vw_api_types_versions::latest as types;

/// A target's slice of the workspace, ready to be sent.
pub struct Target {
    pub kind: types::TargetKind,
    /// The directory scanned for this target.
    ///
    /// A target with its own subtree is rooted there, so paths on the instance
    /// are relative to it — `driver`'s `Cargo.toml` lands at `Cargo.toml`,
    /// where cargo expects to find it, not at `driver/Cargo.toml`.
    pub root: Utf8PathBuf,
    /// Paths under the workspace root that belong to another target and must
    /// not be sent with this one.
    pub excluded: Vec<String>,
}

/// The directory a vw workspace keeps its driver in.
///
/// Everything under it is built on helios and nothing else is, which is what
/// makes the split something this can decide rather than something a workspace
/// has to say.
pub const DRIVER: &str = "driver";

/// Work out how a workspace divides across an environment's instances.
///
/// A workspace with no driver gets no helios target, rather than one with an
/// empty tree: an empty manifest is a valid instruction to delete everything,
/// and sending one because there is no `driver` directory would be a poor way
/// to find that out.
pub fn targets(workspace: &Utf8Path) -> Vec<Target> {
    // Vivado takes the workspace as a whole, less the driver.
    let mut targets = vec![Target {
        kind: types::TargetKind::Vivado,
        root: workspace.to_owned(),
        excluded: vec![DRIVER.to_owned()],
    }];

    let driver = workspace.join(DRIVER);
    if driver.is_dir() {
        targets.push(Target {
            kind: types::TargetKind::Helios,
            root: driver,
            excluded: Vec::new(),
        });
    }

    targets
}

/// Scan a target's tree, dropping anything claimed by another target.
pub fn scan(
    target: &Target,
) -> Result<types::TreeManifest, vw_sync::ScanError> {
    let manifest = vw_sync::scan(&target.root)?;

    let entries = manifest
        .entries
        .into_iter()
        .filter(|entry| {
            !target.excluded.iter().any(|excluded| {
                entry.path == *excluded
                    || entry.path.starts_with(&format!("{excluded}/"))
            })
        })
        .collect();

    Ok(types::TreeManifest { entries })
}

impl Target {
    /// Read one of this target's files, for delivery.
    pub fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.root.join(path))
    }

    /// Find the file in `manifest` with this digest.
    ///
    /// A plan asks for content by digest, and the sender has it by path, so
    /// something has to bridge the two. Any path with the right digest will
    /// do — that is the whole point of naming content by what it is.
    pub fn path_for<'a>(
        &self,
        manifest: &'a types::TreeManifest,
        digest: &types::Digest,
    ) -> Option<&'a str> {
        manifest
            .entries
            .iter()
            .find(|entry| entry.digest == *digest)
            .map(|entry| entry.path.as_str())
    }
}

/// Push the workspace to an environment, once or continuously.
pub async fn run(
    session: &crate::cloud::Session,
    environment: &str,
    force: bool,
    watch: bool,
    debounce: std::time::Duration,
) -> Result<(), CloudError> {
    let workspace = workspace_root()?;
    let targets = targets(&workspace);
    announce(&targets);

    // Only ever the first pass. Forcing is an answer to a doubt about what is
    // on the instance, and once the sync below has settled it there is nothing
    // left to doubt — re-clearing on every file save would mean re-uploading
    // the whole workspace to see a one line edit.
    if force {
        clear(session, environment, &targets).await?;
    }

    sync_once(session, environment, &targets).await?;

    if !watch {
        return Ok(());
    }

    println!();
    println!(
        "{} {}",
        "watching".bright_black(),
        workspace.as_str().bright_black(),
    );

    let mut changes = watcher(&workspace)?;
    loop {
        // Wait for something to happen, then let the rest of the burst land
        // before scanning. An editor writing a file is several events, and a
        // branch switch is thousands; syncing on the first one would mean
        // syncing a tree that is still moving.
        if changes.recv().await.is_none() {
            return Ok(());
        }
        while tokio::time::timeout(debounce, changes.recv()).await.is_ok() {}

        if let Err(e) = sync_once(session, environment, &targets).await {
            // A failed sync is not a reason to stop watching. The next save
            // tries again, and an instance that is still coming up will be
            // there shortly.
            eprintln!("{} {e}", "error:".bright_red());
        }
    }
}

/// Say which instance is getting which directory.
///
/// Worth the two lines because a target with nothing to do prints nothing, so
/// a workspace with no driver and one whose helios instance is merely up to
/// date look identical from the outside. Said once, before the first pass,
/// because it describes the workspace rather than anything a sync does.
fn announce(targets: &[Target]) {
    for target in targets {
        println!(
            "{} {} {}",
            "\u{2192}".bright_black(),
            target.kind.to_string().cyan(),
            target.root.as_str().bright_black(),
        );
    }
}

/// Throw away what every target's instance has.
///
/// Leaves each instance as though it had never been synchronized, so the pass
/// that follows finds nothing there and sends the whole tree. This is the
/// entire implementation of forcing: the ordinary path already sends whatever
/// the instance is missing, so making the instance miss everything is all
/// there is to do.
async fn clear(
    session: &crate::cloud::Session,
    environment: &str,
    targets: &[Target],
) -> Result<(), CloudError> {
    for target in targets {
        let result = session
            .client
            .sync_clear(environment, &target.kind)
            .await
            .map_err(|e| session.error(e))?
            .into_inner();

        println!(
            "{} {} cleared ({} removed)",
            "\u{2717}".bright_black(),
            target.kind.to_string().cyan(),
            result.deleted,
        );
    }

    Ok(())
}

/// One pass over every target.
async fn sync_once(
    session: &crate::cloud::Session,
    environment: &str,
    targets: &[Target],
) -> Result<(), CloudError> {
    for target in targets {
        let manifest = scan(target)
            .map_err(|e| CloudError::Scan(target.root.clone(), e))?;

        let plan = session
            .client
            .sync_plan(environment, &target.kind, &manifest)
            .await
            .map_err(|e| session.error(e))?
            .into_inner();

        // Uploaded several at a time. Each one is a whole round trip to the
        // service, and source files are small enough that the time is almost
        // entirely waiting rather than transferring — sending them one after
        // another means the link sits idle for all of it. A first sync of a
        // few hundred files is the case that makes this obvious.
        //
        // Blobs are named by their content and land in a store, so they may
        // arrive in any order and are safe to send at once. The commit that
        // follows is what imposes an order, and it happens after all of this.
        let uploads = plan.missing.iter().map(|digest| {
            let path = target
                .path_for(&manifest, digest)
                .expect("the plan only asks for content the manifest names");

            async move {
                let contents = target
                    .read(path)
                    .map_err(|e| CloudError::ReadSource(path.to_owned(), e))?;

                session
                    .client
                    .sync_blob(
                        environment,
                        &target.kind,
                        digest.0.as_str(),
                        contents,
                    )
                    .await
                    .map_err(|e| session.error(e))?;

                Ok::<(), CloudError>(())
            }
        });

        futures::stream::iter(uploads)
            .buffer_unordered(UPLOAD_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;

        let result = session
            .client
            .sync_commit(environment, &target.kind, &manifest)
            .await
            .map_err(|e| session.error(e))?
            .into_inner();

        report(target, &manifest, plan.missing.len(), &result);
    }

    Ok(())
}

/// Say what a target's sync did, and stay quiet when it did nothing.
fn report(
    target: &Target,
    manifest: &types::TreeManifest,
    uploaded: usize,
    result: &types::CommitResult,
) {
    let changed = result.created + result.updated + result.deleted;
    if changed == 0 {
        return;
    }

    println!(
        "{} {} +{} ~{} -{} ({} sent, {} files)",
        "\u{2713}".bright_green(),
        target.kind.to_string().cyan(),
        result.created,
        result.updated,
        result.deleted,
        uploaded,
        manifest.entries.len(),
    );
}

/// The workspace this command was run from.
///
/// Walks up from the current directory, the way cargo and git do, so it can be
/// run from anywhere inside a workspace rather than only at its root.
fn workspace_root() -> Result<Utf8PathBuf, CloudError> {
    let cwd = std::env::current_dir().map_err(|_| CloudError::NoWorkspace)?;
    let mut dir =
        Utf8PathBuf::from_path_buf(cwd).map_err(|_| CloudError::NoWorkspace)?;

    loop {
        if dir.join("vw.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(CloudError::NoWorkspace);
        }
    }
}

/// A stream of "something under `root` changed".
///
/// The events themselves are discarded. Working out what changed from them is
/// a great deal of bookkeeping that a scan answers directly and correctly,
/// including for the cases events are worst at — a branch switch, a file
/// replaced by a directory, an editor writing through a temporary file.
fn watcher(
    root: &Utf8Path,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<()>, CloudError> {
    use notify::Watcher;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        if let Ok(notify::Event { kind, .. }) = event {
            // Access events fire when a build merely reads the tree, which is
            // constant and means nothing here.
            if !matches!(kind, notify::EventKind::Access(_)) {
                let _ = tx.send(());
            }
        }
    })
    .map_err(CloudError::Watch)?;

    watcher
        .watch(root.as_std_path(), notify::RecursiveMode::Recursive)
        .map_err(CloudError::Watch)?;

    // The watcher stops when it is dropped, and nothing else owns it.
    std::mem::forget(watcher);

    Ok(rx)
}

#[cfg(test)]
mod test {
    use super::*;

    fn workspace(files: &[&str]) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).expect("parent");
            std::fs::write(&path, "contents").expect("write");
        }
        (dir, root)
    }

    #[test]
    fn a_workspace_with_no_driver_is_all_vivados() {
        let (_dir, root) = workspace(&["hdl/top.vhd", "vw.toml"]);
        let targets = targets(&root);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, types::TargetKind::Vivado);

        let manifest = scan(&targets[0]).expect("scan");
        let paths: Vec<&str> =
            manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["hdl/top.vhd", "vw.toml"]);
    }

    #[test]
    fn the_driver_goes_to_helios_and_nowhere_else() {
        // The metroid shape: everything is vivado's except `driver`.
        let (_dir, root) = workspace(&[
            "hdl/top.vhd",
            "vw.toml",
            "driver/Cargo.toml",
            "driver/src/lib.rs",
        ]);
        let targets = targets(&root);

        assert_eq!(targets.len(), 2);

        let vivado = scan(&targets[0]).expect("scan vivado");
        let paths: Vec<&str> =
            vivado.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["hdl/top.vhd", "vw.toml"],
            "vivado should not be carrying the driver",
        );

        let helios = scan(&targets[1]).expect("scan helios");
        let paths: Vec<&str> =
            helios.entries.iter().map(|e| e.path.as_str()).collect();
        // Rooted at `driver`, so cargo finds its manifest where it expects to.
        assert_eq!(paths, ["Cargo.toml", "src/lib.rs"]);
    }

    #[test]
    fn a_name_that_merely_starts_with_driver_is_not_the_driver() {
        // `driver` must not swallow `drivers-notes.md`, which shares its
        // first six characters and nothing else.
        let (_dir, root) =
            workspace(&["driver/Cargo.toml", "drivers-notes.md"]);
        let targets = targets(&root);

        let vivado = scan(&targets[0]).expect("scan");
        let paths: Vec<&str> =
            vivado.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["drivers-notes.md"]);
    }

    #[test]
    fn a_workspace_without_a_driver_syncs_its_hardware_anyway() {
        // Plenty of workspaces are hardware only. That should sync what there
        // is rather than fail — and certainly not send helios an empty
        // manifest, which would mean "delete everything".
        let (_dir, root) = workspace(&["hdl/top.vhd"]);
        let targets = targets(&root);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, types::TargetKind::Vivado);
    }

    #[test]
    fn build_output_is_not_in_any_target() {
        let (_dir, root) = workspace(&[
            "hdl/top.vhd",
            "target/synth/top.dcp",
            "driver/Cargo.toml",
            "driver/target/debug/thing",
        ]);
        let targets = targets(&root);

        for target in &targets {
            let manifest = scan(target).expect("scan");
            assert!(
                !manifest
                    .entries
                    .iter()
                    .any(|entry| entry.path.contains("target/")),
                "{:?} is carrying build output",
                target.kind,
            );
        }
    }
}
