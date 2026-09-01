// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Getting finished artifacts off the instance that built them.
//!
//! A build leaves its images in `target/image`, which synchronization never
//! touches in either direction — that is the whole point of `target`. Without
//! something like this an artifact would exist only on an instance that is, by
//! design, disposable.
//!
//! Polled rather than watched. A `.pdi` is written once at the end of a run
//! that took hours, so noticing it a few seconds late costs nothing, and a
//! poll has none of a watcher's trouble with a directory that does not exist
//! yet, is deleted by `vw clean`, and then reappears.

use std::collections::HashMap;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use slog::{info, warn, Logger};
use vw_api_types_versions::latest::S3Credentials;

/// Where a build leaves things worth keeping, and what to keep from each.
///
/// Directories under the workspace's `target`, paired with the extension that
/// matters there. Everything else a build writes — checkpoints, logs, journal
/// files, the vivado project — is either enormous, only meaningful on the
/// machine that made it, or both.
///
/// `synth`, `place` and `route` each produce an `edif` and they are different
/// netlists, which is why what goes in the bucket is keyed by the directory
/// too rather than by file name alone.
const GATHERED: [(&str, &str); 6] = [
    ("image", "pdi"),
    ("reports", "rpt"),
    ("reports", "csv"),
    ("synth", "edif"),
    ("place", "edif"),
    ("route", "edif"),
];

/// Where a testbench leaves its results, and what to keep.
///
/// Kept apart from [`GATHERED`] because the shape is different: each bench
/// writes into a directory of its own under `target/bench`, so what matters is
/// a level deeper than everything else and the flat walk above cannot see it.
///
/// The `fst` is the waveform, and it is the first thing anybody opens when a
/// simulation did not do what they expected — so it is worth keeping even
/// though it is the largest thing here. A mixed-signal run additionally leaves
/// the Xyce `prn` and the plots rendered from it.
///
/// Not what nvc built to get there: that lives under `target/sim` and is a
/// work library and object files, meaningless anywhere but the machine that
/// made them.
const GATHERED_PER_BENCH: [&str; 3] = ["fst", "prn", "png"];

/// The directory holding one subdirectory per bench that has run.
const BENCH_OUTPUT: &str = "bench";

/// The directory a build writes everything to, under the workspace.
const BUILD_OUTPUT: &str = "target";

/// How often to look.
///
/// Often enough that an artifact is on its way before anyone thinks to ask
/// for it. A scan of one directory costs a `readdir`, and the digest check
/// only reads a file that has actually changed.
const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub(crate) enum ArtifactError {
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("the stored artifact target is not readable")]
    Corrupt,
}

/// Remember where artifacts go, so a restart does not have to be told again.
///
/// The instance can reboot between one build and the next, and whoever told it
/// where to put things may not think to say so a second time.
pub(crate) fn remember(
    path: &Utf8Path,
    credentials: &S3Credentials,
) -> Result<(), ArtifactError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ArtifactError::Write(parent.to_owned(), e))?;
    }

    let encoded = serde_json::to_string_pretty(credentials)
        .map_err(|_| ArtifactError::Corrupt)?;
    std::fs::write(path, encoded)
        .map_err(|e| ArtifactError::Write(path.to_owned(), e))?;

    // It holds a secret key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ArtifactError::Write(path.to_owned(), e))?;
    }

    Ok(())
}

/// What was remembered, if anything.
pub(crate) fn recall(
    path: &Utf8Path,
) -> Result<Option<S3Credentials>, ArtifactError> {
    if !path.is_file() {
        return Ok(None);
    }

    let stored = std::fs::read_to_string(path)
        .map_err(|e| ArtifactError::Read(path.to_owned(), e))?;
    serde_json::from_str(&stored)
        .map(Some)
        .map_err(|_| ArtifactError::Corrupt)
}

/// Watch `root`'s image directory and upload whatever appears in it.
///
/// Runs until the agent stops. Takes its target from `target`, which is
/// updated whenever the service tells us where artifacts go — so an agent that
/// started before anyone said simply uploads nothing until someone does.
pub(crate) async fn synchronize(
    root: Utf8PathBuf,
    mut target: tokio::sync::watch::Receiver<Option<S3Credentials>>,
    log: Logger,
) {
    // What has already gone, by path, with the digest that went. Keyed by
    // digest rather than a timestamp so a rebuild that produces byte-identical
    // output is not uploaded twice, and one that produces different bytes
    // under the same name is.
    let mut uploaded: HashMap<Utf8PathBuf, String> = HashMap::new();
    // What each file looked like last time round, to tell a finished artifact
    // from one still being written.
    let mut previously: HashMap<Utf8PathBuf, Stamp> = HashMap::new();

    loop {
        tokio::select! {
            () = tokio::time::sleep(INTERVAL) => {}
            // A new target means the old record of what has been sent is
            // about somewhere else.
            changed = target.changed() => {
                if changed.is_err() {
                    return;
                }
                uploaded.clear();
            }
        }

        let credentials = target.borrow().clone();
        let Some(credentials) = credentials else {
            continue;
        };

        let mut currently = HashMap::new();
        for Found {
            path: artifact,
            key,
        } in artifacts(&root)
        {
            // An image is written over seconds, and this looks every second.
            // A file whose size or timestamp moved since the last pass is
            // still being written, and uploading it now would put a truncated
            // artifact in the bucket under the name of a finished one — which
            // is worse than not having it yet, because it looks like success.
            let Some(stamp) = stamp(&artifact) else {
                continue;
            };
            let settled = previously.get(&artifact) == Some(&stamp);
            currently.insert(artifact.clone(), stamp);
            if !settled {
                continue;
            }

            let digest = match digest_of(&artifact) {
                Ok(digest) => digest,
                Err(e) => {
                    warn!(log, "cannot read an artifact";
                        "path" => %artifact,
                        "error" => %e,
                    );
                    continue;
                }
            };
            if uploaded.get(&artifact) == Some(&digest) {
                continue;
            }

            match upload(&credentials, &key, &artifact).await {
                Ok(()) => {
                    info!(log, "uploaded an artifact";
                        "path" => %artifact,
                        "bucket" => &credentials.bucket,
                        "key" => &key,
                    );
                    uploaded.insert(artifact, digest);
                }
                Err(e) => {
                    // Left out of `uploaded`, so the next pass tries again.
                    // A store that is briefly unreachable should not cost an
                    // artifact.
                    warn!(log, "cannot upload an artifact";
                        "path" => %artifact,
                        "error" => %e,
                    );
                }
            }
        }
        previously = currently;
    }
}

/// What a file looks like from the outside, cheaply.
///
/// Size and modification time together are enough to notice a file that is
/// still growing, without reading it — which matters when this runs every
/// second and the file is hundreds of megabytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp(path: &Utf8Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// One thing worth keeping, and the name it goes under.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Found {
    /// Where it is on this instance.
    path: Utf8PathBuf,
    /// What it is called in the bucket: the path under `target`, so a netlist
    /// from `synth` and one from `route` stay two different objects rather
    /// than one overwriting the other.
    key: String,
}

/// Everything currently sitting in the directories a build fills.
///
/// Not recursive: each of these is written flat, and descending would pick up
/// the working state of whatever wrote them — vivado's `.runs` scratch under a
/// stage directory is neither small nor meaningful anywhere else.
fn artifacts(root: &Utf8Path) -> Vec<Found> {
    let mut found = Vec::new();

    for (directory, extension) in GATHERED {
        let source = root.join(BUILD_OUTPUT).join(directory);
        let Ok(entries) = std::fs::read_dir(&source) else {
            // A build that has not reached this stage yet, which is the
            // ordinary case for most of them most of the time.
            continue;
        };

        for path in entries
            .flatten()
            .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
            .filter(|path| path.is_file())
            .filter(|path| path.extension() == Some(extension))
        {
            let Some(name) = path.file_name() else {
                continue;
            };
            found.push(Found {
                key: format!("{directory}/{name}"),
                path,
            });
        }
    }

    found.extend(bench_artifacts(root));

    found.sort();
    found
}

/// The results of every mixed-signal bench that has run here.
///
/// One directory deeper than everything else, so it gets its own walk. The
/// bench's name stays in the key — two benches both producing an `eye.png`
/// would otherwise be one object in the bucket, each overwriting the other.
fn bench_artifacts(root: &Utf8Path) -> Vec<Found> {
    let mut found = Vec::new();

    let source = root.join(BUILD_OUTPUT).join(BENCH_OUTPUT);
    let Ok(benches) = std::fs::read_dir(&source) else {
        // No mixed-signal bench has run here, which is most workspaces.
        return found;
    };

    for bench in benches
        .flatten()
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| path.is_dir())
    {
        let Some(bench_name) = bench.file_name().map(str::to_owned) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&bench) else {
            continue;
        };

        for path in entries
            .flatten()
            .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
            .filter(|path| path.is_file())
            .filter(|path| {
                path.extension()
                    .is_some_and(|e| GATHERED_PER_BENCH.contains(&e))
            })
        {
            let Some(name) = path.file_name() else {
                continue;
            };
            found.push(Found {
                key: format!("{BENCH_OUTPUT}/{bench_name}/{name}"),
                path,
            });
        }
    }

    found
}

/// What an artifact currently hashes to, without holding it in memory.
///
/// An image runs to hundreds of megabytes and this runs every second; reading
/// one into a buffer to hash it would make an idle agent's memory use track
/// the size of the last build.
fn digest_of(path: &Utf8Path) -> std::io::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Send a specific set of files to the store, keyed by where they sit under
/// the build output directory.
///
/// The other path here polls, because vivado writes when it likes and nothing
/// announces it. A cargo build does announce it — cargo names every file it
/// produced — so there is nothing to discover and nothing to wait for.
///
/// Returns how many went. A failure is logged and skipped rather than
/// abandoning the rest: one unreachable moment should not cost the other
/// artifacts of the same build.
pub(crate) async fn upload_all(
    root: &Utf8Path,
    credentials: &S3Credentials,
    artifacts: &[Utf8PathBuf],
    log: &Logger,
) -> usize {
    let mut sent = 0;
    for artifact in artifacts {
        // Keyed by its path under `target`, so a debug and a release build of
        // the same name stay two objects, as do the same name built for two
        // different targets.
        let key = artifact
            .strip_prefix(root.join(BUILD_OUTPUT))
            .map(Utf8Path::to_string)
            .unwrap_or_else(|_| {
                artifact.file_name().unwrap_or("artifact").to_owned()
            });

        match upload(credentials, &key, artifact).await {
            Ok(()) => {
                info!(log, "uploaded a build artifact";
                    "path" => %artifact,
                    "bucket" => &credentials.bucket,
                    "key" => &key,
                );
                sent += 1;
            }
            Err(e) => warn!(log, "cannot upload a build artifact";
                "path" => %artifact,
                "error" => %e,
            ),
        }
    }
    sent
}

/// Put one artifact in the bucket.
///
/// Streamed from disk rather than read first, for the same reason the digest
/// is: the files this exists to move are large, and neither end of the wire
/// needs a copy of one in memory for it to get across.
async fn upload(
    credentials: &S3Credentials,
    key: &str,
    path: &Utf8Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let region = s3::Region::Custom {
        region: credentials.region.clone(),
        endpoint: credentials.endpoint.clone(),
    };
    let creds = s3::creds::Credentials::new(
        Some(&credentials.access_key_id),
        Some(&credentials.secret_access_key),
        None,
        None,
        None,
    )?;

    // Path style because the bucket is reached by address rather than by name:
    // there is no DNS inside the VPC that would resolve
    // `vivado-darmok.<instance>`.
    let bucket =
        s3::Bucket::new(&credentials.bucket, region, creds)?.with_path_style();

    let mut file = tokio::fs::File::open(path).await?;
    let status = bucket
        .put_object_stream(&mut file, format!("/{key}"))
        .await?
        .status_code();
    if status >= 300 {
        return Err(format!("the object store answered {status}").into());
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        (dir, root)
    }

    fn credentials() -> S3Credentials {
        S3Credentials {
            endpoint: "http://127.0.0.1:3900".to_owned(),
            port: 3900,
            region: "garage".to_owned(),
            bucket: "vivado-darmok".to_owned(),
            access_key_id: "GK00000000000000000000000".to_owned(),
            secret_access_key: "shhh".to_owned(),
        }
    }

    /// Put a file where a build would leave it.
    fn build_output(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(BUILD_OUTPUT).join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    /// A mixed-signal bench writes a directory of its own, one level below
    /// everything else. The flat walk that finds images and netlists cannot
    /// see into it, so this is the thing that would silently ship nothing.
    #[test]
    fn a_mixed_signal_benchs_results_are_gathered() {
        let (_dir, root) = scratch();
        build_output(&root, "bench/tx-eq/model.cir.prn", "xyce output");
        build_output(&root, "bench/tx-eq/eye.png", "a plot");
        build_output(&root, "bench/tx-eq/timeseries.png", "another plot");
        build_output(&root, "bench/tx-eq/tx-eq.fst", "a waveform");

        let keys: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(
            keys,
            [
                "bench/tx-eq/eye.png",
                "bench/tx-eq/model.cir.prn",
                "bench/tx-eq/timeseries.png",
                "bench/tx-eq/tx-eq.fst",
            ],
        );
    }

    /// The bench's name has to survive into the key. Two benches each
    /// producing an `eye.png` would otherwise be one object, each overwriting
    /// the other, and only the last one run would exist.
    #[test]
    fn two_benches_do_not_collide_in_the_bucket() {
        let (_dir, root) = scratch();
        build_output(&root, "bench/tx-eq/eye.png", "one");
        build_output(&root, "bench/rx-eq/eye.png", "another");

        let keys: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(keys, ["bench/rx-eq/eye.png", "bench/tx-eq/eye.png"]);
    }

    /// The waveform is the point. When a simulation does not do what it was
    /// supposed to, the `fst` is the first thing anybody opens — so it ships
    /// even though it is the biggest thing a bench leaves behind.
    #[test]
    fn a_waveform_is_gathered() {
        let (_dir, root) = scratch();
        build_output(&root, "bench/dma_tb/dma_tb.fst", "a waveform");

        let keys: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(keys, ["bench/dma_tb/dma_tb.fst"]);
    }

    /// What nvc built on the way there is not a result. `target/sim` holds a
    /// work library and object files, which mean nothing off the machine that
    /// made them.
    #[test]
    fn a_benchs_build_directory_is_not_shipped() {
        let (_dir, root) = scratch();
        build_output(&root, "sim/dma_tb/work/WORK.DMA_TB", "nvc library");
        build_output(&root, "sim/dma_tb/_NVC_LIB", "more of it");
        build_output(&root, "bench/dma_tb/dma_tb.fst", "a waveform");

        let keys: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(keys, ["bench/dma_tb/dma_tb.fst"]);
    }

    #[test]
    fn only_what_a_build_produced_counts_as_an_artifact() {
        let (_dir, root) = scratch();
        build_output(&root, "image/top.pdi", "image");
        build_output(&root, "reports/timing.rpt", "report");
        build_output(&root, "synth/design.edif", "netlist");
        // Neither small nor meaningful anywhere else.
        build_output(&root, "image/top.bit", "bitstream");
        build_output(&root, "synth/design.dcp", "checkpoint");
        build_output(&root, "vw-project/project.xpr", "project");
        build_output(&root, "logs/vivado.log", "log");
        // A stage's working directory is not ours to send either.
        build_output(&root, "synth/runs/inner.edif", "scratch");

        let found: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(
            found,
            ["image/top.pdi", "reports/timing.rpt", "synth/design.edif",],
        );
    }

    #[test]
    fn a_netlist_from_each_stage_is_its_own_artifact() {
        // `synth`, `place` and `route` all produce `design.edif` and they are
        // three different netlists. Keyed by file name alone, whichever
        // uploaded last would be the only one anybody could ever download.
        let (_dir, root) = scratch();
        for stage in ["synth", "place", "route"] {
            build_output(&root, &format!("{stage}/design.edif"), stage);
        }

        let found: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(
            found,
            [
                "place/design.edif",
                "route/design.edif",
                "synth/design.edif"
            ],
        );
    }

    #[test]
    fn an_instance_with_no_build_output_has_nothing_to_send() {
        let (_dir, root) = scratch();
        assert!(artifacts(&root).is_empty());
    }

    #[test]
    fn a_build_that_has_only_reached_synthesis_sends_what_it_has() {
        // Most of the time most stages do not exist yet, and that is not a
        // condition worth reporting — it is just a build in progress.
        let (_dir, root) = scratch();
        build_output(&root, "synth/design.edif", "netlist");

        let found: Vec<String> =
            artifacts(&root).into_iter().map(|f| f.key).collect();

        assert_eq!(found, ["synth/design.edif"]);
    }

    #[test]
    fn where_artifacts_go_survives_a_restart() {
        let (_dir, root) = scratch();
        let path = root.join("artifact-target.json");

        remember(&path, &credentials()).expect("remember");
        let recalled = recall(&path).expect("recall").expect("something");

        assert_eq!(recalled.bucket, "vivado-darmok");
        assert_eq!(recalled.access_key_id, "GK00000000000000000000000");
    }

    #[test]
    fn nothing_remembered_is_not_an_error() {
        let (_dir, root) = scratch();
        assert!(recall(&root.join("nothing.json"))
            .expect("recall")
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_key_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, root) = scratch();
        let path = root.join("artifact-target.json");

        remember(&path, &credentials()).expect("remember");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn a_file_still_being_written_is_not_yet_an_artifact() {
        // An image is written over seconds and this looks every second, so the
        // difference between "finished" and "half there" is the only thing
        // standing between a consumer and a truncated build.
        let (_dir, root) = scratch();
        let path = root.join(BUILD_OUTPUT).join("image/top.pdi");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

        std::fs::write(&path, "half").expect("write");
        let half = stamp(&path).expect("stamp");

        // Still growing: this pass looks different from the last.
        std::fs::write(&path, "half and then some").expect("write");
        let grown = stamp(&path).expect("stamp");
        assert_ne!(half, grown, "a file that grew should look different");

        // Unchanged since the last pass, so it is done.
        assert_eq!(stamp(&path).expect("stamp"), grown);
    }

    #[test]
    fn a_digest_does_not_depend_on_reading_the_whole_file_at_once() {
        // The point is constant memory, but what has to be true for the change
        // check to work is that the digest tracks the contents.
        let (_dir, root) = scratch();
        let path = root.join("artifact.pdi");

        std::fs::write(&path, vec![7u8; 300_000]).expect("write");
        let first = digest_of(&path).expect("digest");
        assert_eq!(digest_of(&path).expect("digest"), first, "stable");

        std::fs::write(&path, vec![9u8; 300_000]).expect("write");
        assert_ne!(digest_of(&path).expect("digest"), first, "changed");
    }

    #[test]
    fn a_secret_is_not_in_the_debug_output() {
        let printed = format!("{:?}", credentials());
        assert!(!printed.contains("shhh"), "{printed}");
        assert!(printed.contains("vivado-darmok"), "{printed}");
    }
}
