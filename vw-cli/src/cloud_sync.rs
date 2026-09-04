// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pushing a workspace to the instances that build it.
//!
//! Every instance gets the whole workspace.
//!
//! An environment's two halves do build different things — vivado builds the
//! hardware, helios builds what drives it — and it is tempting to send each of
//! them only what it builds from. That was how this worked, and the line
//! turned out not to stay put: a build script that reads `vw.toml`, a register
//! map the driver and the design both generate from, a header shared between a
//! testbench and a kernel module. Every one of those is a file on the wrong
//! side of the line, discovered as a build failure on an instance rather than
//! as anything a developer did wrong.
//!
//! Sending all of it to both is what removes that failure mode rather than
//! moving it, and it costs very little. The scan already leaves out build
//! output and anything a `.gitignore` covers; content is addressed by digest,
//! so a file the second instance already holds is named in a manifest and not
//! sent; and a workspace's sources are megabytes, against the gigabytes of
//! build output neither instance ever sends at all.
//!
//! Each instance is still synchronized on its own — its own plan, its own
//! content, its own commit — so there is no ordering to get right and a
//! failure to reach one does not hold up the other.

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

/// The instances a workspace is synchronized to.
///
/// Both of them, whatever is in the workspace. There is nothing to derive and
/// nothing to declare, so there is no way for a workspace's configuration to
/// disagree with where its files actually are.
pub const TARGETS: [types::TargetKind; 2] =
    [types::TargetKind::Vivado, types::TargetKind::Helios];

/// Push the workspace to an environment, once or continuously.
pub async fn run(
    session: &crate::cloud::Session,
    environment: &str,
    force: bool,
    watch: bool,
    debounce: std::time::Duration,
    only: Option<types::TargetKind>,
) -> Result<(), CloudError> {
    let workspace = workspace_root()?;
    // A command that only drives one instance should not fail because the
    // other one is down. `vw driver build` needs helios and nothing else; a
    // vivado instance rebooting is not its problem.
    let targets: Vec<types::TargetKind> = TARGETS
        .into_iter()
        .filter(|kind| only.is_none_or(|only| only == *kind))
        .collect();
    announce(&workspace);

    // Only ever the first pass. Forcing is an answer to a doubt about what is
    // on the instance, and once the sync below has settled it there is nothing
    // left to doubt — re-clearing on every file save would mean re-uploading
    // the whole workspace to see a one line edit.
    if force {
        clear(session, environment, &targets).await?;
    }

    sync_once(session, environment, &workspace, &targets, true).await?;

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

        if let Err(e) =
            sync_once(session, environment, &workspace, &targets, false).await
        {
            // A failed sync is not a reason to stop watching. The next save
            // tries again, and an instance that is still coming up will be
            // there shortly.
            crate::report(&e);
        }
    }
}

/// Say which workspace is being pushed.
///
/// Worth the line because a sync can be run from anywhere inside a workspace,
/// and "which one did that just send" is not a question anyone should have to
/// answer by remembering where they were standing. Which instances are getting
/// it is left to the reports below, which name them either way.
fn announce(workspace: &Utf8Path) {
    println!(
        "{} {}",
        "\u{2192}".bright_black(),
        workspace.as_str().bright_black(),
    );
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
    targets: &[types::TargetKind],
) -> Result<(), CloudError> {
    for kind in targets {
        let result = vw_api_client::retrying(|| {
            session.client.sync_clear(environment, kind)
        })
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

        println!(
            "{} {} cleared ({} removed)",
            "\u{2717}".bright_black(),
            kind.to_string().cyan(),
            result.deleted,
        );
    }

    Ok(())
}

/// One pass over every target.
async fn sync_once(
    session: &crate::cloud::Session,
    environment: &str,
    workspace: &Utf8Path,
    targets: &[types::TargetKind],
    announce: bool,
) -> Result<(), CloudError> {
    // Scanned once for all of them. Every instance is being told about the
    // same tree, and the scan is the expensive half of a pass — every source
    // file read and hashed — so scanning per instance would double the cost of
    // saying the same thing twice.
    let manifest = vw_sync::scan(workspace)
        .map_err(|e| CloudError::Scan(workspace.to_owned(), e))?;

    for kind in targets {
        let plan = vw_api_client::retrying(|| {
            session.client.sync_plan(environment, kind, &manifest)
        })
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
            let path = path_for(&manifest, digest)
                .expect("the plan only asks for content the manifest names");

            async move {
                let contents = std::fs::read(workspace.join(path))
                    .map_err(|e| CloudError::ReadSource(path.to_owned(), e))?;

                // Cloned per attempt because the body is consumed by the
                // request. That costs a copy of one file on the happy path,
                // which is the price of an upload that survives a dropped
                // connection — and these are workspace sources, not the
                // hundreds of megabytes an artifact runs to.
                vw_api_client::retrying(|| {
                    session.client.sync_blob(
                        environment,
                        kind,
                        digest.0.as_str(),
                        contents.clone(),
                    )
                })
                .await
                .map_err(|e| session.error(e))?;

                Ok::<(), CloudError>(())
            }
        });

        futures::stream::iter(uploads)
            .buffer_unordered(UPLOAD_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;

        let result = vw_api_client::retrying(|| {
            session.client.sync_commit(environment, kind, &manifest)
        })
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

        report(*kind, &manifest, plan.missing.len(), &result, announce);
    }

    Ok(())
}

/// Find the file in `manifest` with this digest.
///
/// A plan asks for content by digest, and the sender has it by path, so
/// something has to bridge the two. Any path with the right digest will do —
/// that is the whole point of naming content by what it is.
fn path_for<'a>(
    manifest: &'a types::TreeManifest,
    digest: &types::Digest,
) -> Option<&'a str> {
    manifest
        .entries
        .iter()
        .find(|entry| entry.digest == *digest)
        .map(|entry| entry.path.as_str())
}

/// Say what a target's sync did.
///
/// A sync that changed nothing says so when `always` is set, and says nothing
/// otherwise. Both are wanted, in different places. Watching a workspace means
/// a sync per keystroke-ish burst, and a line each time that nothing happened
/// would bury the ones where something did. But a sync run once — before a
/// build, or because the developer asked for one — that printed nothing is
/// indistinguishable from a sync that did not run, and "did my sources
/// actually get there" is not a question anyone should have to answer by
/// reading the source.
fn report(
    kind: types::TargetKind,
    manifest: &types::TreeManifest,
    uploaded: usize,
    result: &types::CommitResult,
    always: bool,
) {
    let changed = result.created + result.updated + result.deleted;
    if changed == 0 {
        if always {
            println!(
                "{} {} up to date ({} files)",
                "\u{2713}".bright_green(),
                kind.to_string().cyan(),
                manifest.entries.len(),
            );
        }
        return;
    }

    println!(
        "{} {} +{} ~{} -{} ({} sent, {} files)",
        "\u{2713}".bright_green(),
        kind.to_string().cyan(),
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

/// Remove the build output on every one of an environment's instances.
///
/// Both halves, because both build: vivado writes under the workspace root and
/// the driver's cargo writes under `driver/`. Cleaning one and leaving the
/// other would be a surprising thing for one command to do.
pub async fn clean(
    session: &crate::cloud::Session,
    environment: &str,
) -> Result<(), CloudError> {
    for kind in TARGETS {
        let result = vw_api_client::retrying(|| {
            session.client.clean_build_output(environment, &kind)
        })
        .await
        .map_err(|e| session.error(e))?
        .into_inner();

        if result.existed {
            println!(
                "{} {} removed {}",
                "\u{2713}".bright_green(),
                kind.to_string().cyan(),
                human_bytes(result.bytes),
            );
        } else {
            println!(
                "{} {} nothing to remove",
                "\u{2713}".bright_green(),
                kind.to_string().cyan(),
            );
        }
    }

    Ok(())
}

/// A byte count as a person would say it.
///
/// Build output runs to gigabytes, and "12093847552" is a number nobody reads.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
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

    fn paths(manifest: &types::TreeManifest) -> Vec<&str> {
        manifest.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn both_instances_are_synchronized() {
        assert_eq!(
            TARGETS,
            [types::TargetKind::Vivado, types::TargetKind::Helios],
        );
    }

    #[test]
    fn the_whole_workspace_goes_to_both() {
        // The thing that used to be split: `driver` went to helios and the
        // rest to vivado. Both of them now get all of it, driver included,
        // and at the paths it has in the workspace.
        let (_dir, root) = workspace(&[
            "hdl/top.vhd",
            "vw.toml",
            "driver/Cargo.toml",
            "driver/src/lib.rs",
        ]);

        let manifest = vw_sync::scan(&root).expect("scan");

        assert_eq!(
            paths(&manifest),
            [
                "driver/Cargo.toml",
                "driver/src/lib.rs",
                "hdl/top.vhd",
                "vw.toml",
            ],
        );
    }

    #[test]
    fn a_workspace_with_no_driver_still_syncs_to_both() {
        // Plenty of workspaces are hardware only. Nothing about that is a
        // reason to leave an instance out — it is a workspace with no driver,
        // not a workspace with no helios half.
        let (_dir, root) = workspace(&["hdl/top.vhd", "vw.toml"]);

        let manifest = vw_sync::scan(&root).expect("scan");

        assert_eq!(paths(&manifest), ["hdl/top.vhd", "vw.toml"]);
        assert_eq!(TARGETS.len(), 2);
    }

    #[test]
    fn build_output_is_never_sent() {
        let (_dir, root) = workspace(&[
            "hdl/top.vhd",
            "target/synth/top.dcp",
            "driver/Cargo.toml",
            "driver/target/debug/thing",
        ]);

        let manifest = vw_sync::scan(&root).expect("scan");

        assert!(
            !manifest
                .entries
                .iter()
                .any(|entry| entry.path.contains("target/")),
            "the manifest is carrying build output: {:?}",
            paths(&manifest),
        );
    }

    #[test]
    fn content_is_found_by_digest() {
        // What bridges a plan, which asks by digest, and a sender, which has
        // files by path.
        let (_dir, root) = workspace(&["hdl/top.vhd", "vw.toml"]);
        let manifest = vw_sync::scan(&root).expect("scan");

        for entry in &manifest.entries {
            // Every file here has the same contents, so any of the paths is a
            // correct answer — which is the property being relied on.
            let found = path_for(&manifest, &entry.digest).expect("a path");
            assert_eq!(
                vw_sync::digest_bytes(
                    &std::fs::read(root.join(found)).expect("read")
                ),
                entry.digest,
            );
        }

        assert!(
            path_for(&manifest, &types::Digest("nothing".to_owned())).is_none(),
        );
    }
}
