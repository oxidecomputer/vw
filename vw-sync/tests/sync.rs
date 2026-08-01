// Behaviour of a full synchronization round trip, exercised the way the two
// ends will use it: scan a sender's tree, ask a receiver what it needs, hand
// over exactly that, and make the receiver match.
//
// Both ends are real directories here. The engine is where the correctness
// lives — the HTTP layer above it only moves bytes — so it is worth testing
// against a filesystem rather than against a mock of one.

use camino::{Utf8Path, Utf8PathBuf};
use tempfile::TempDir;
use vw_api_types_versions::latest::{CommitResult, Digest, TreeManifest};
use vw_sync::{apply, missing, scan, Store};

/// A sender and a receiver, with somewhere to stage delivered content.
struct Pair {
    _dir: TempDir,
    sender: Utf8PathBuf,
    receiver: Utf8PathBuf,
    store: Store,
}

impl Pair {
    fn new() -> Pair {
        let dir = TempDir::new().expect("scratch directory");
        let root = Utf8Path::from_path(dir.path()).expect("utf8 temp dir");
        let (sender, receiver, store) = (
            root.join("sender"),
            root.join("receiver"),
            Store::new(root.join("store")),
        );
        std::fs::create_dir_all(&sender).expect("sender root");
        std::fs::create_dir_all(&receiver).expect("receiver root");

        Pair {
            _dir: dir,
            sender,
            receiver,
            store,
        }
    }

    fn write(&self, path: &str, contents: &str) {
        let full = self.sender.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).expect("parent");
        std::fs::write(&full, contents).expect("write");
    }

    fn write_receiver(&self, path: &str, contents: &str) {
        let full = self.receiver.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).expect("parent");
        std::fs::write(&full, contents).expect("write");
    }

    /// One complete synchronization: plan, deliver what is missing, commit.
    ///
    /// Returns what the commit did and how many blobs went over the wire,
    /// which is the number the whole design exists to keep small.
    fn sync(&self) -> (CommitResult, usize) {
        let manifest = scan(&self.sender).expect("scan sender");
        let plan =
            missing(&self.receiver, &self.store, &manifest).expect("plan");

        for digest in &plan.missing {
            self.store
                .put(digest, &self.content_for(&manifest, digest))
                .expect("deliver");
        }

        let result =
            apply(&self.receiver, &self.store, &manifest).expect("apply");
        (result, plan.missing.len())
    }

    /// The sender's copy of some content, found by digest.
    fn content_for(&self, manifest: &TreeManifest, digest: &Digest) -> Vec<u8> {
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.digest == *digest)
            .expect("the plan asked for something the manifest names");
        std::fs::read(self.sender.join(&entry.path)).expect("read sender file")
    }

    /// Every path on the receiver, so a test can say exactly what is there.
    fn receiver_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = scan(&self.receiver)
            .expect("scan receiver")
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        paths.sort();
        paths
    }

    fn receiver_contents(&self, path: &str) -> String {
        std::fs::read_to_string(self.receiver.join(path))
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }
}

#[test]
fn a_tree_arrives_intact() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("ip/core.xci", "<ip/>");
    pair.write("vw.toml", "[workspace]");

    let (result, uploaded) = pair.sync();

    assert_eq!(uploaded, 3);
    assert_eq!(
        result,
        CommitResult {
            created: 3,
            updated: 0,
            deleted: 0,
            unchanged: 0
        }
    );
    assert_eq!(
        pair.receiver_paths(),
        ["hdl/top.vhd", "ip/core.xci", "vw.toml"]
    );
    assert_eq!(pair.receiver_contents("hdl/top.vhd"), "entity top is end;");
}

#[test]
fn syncing_an_unchanged_tree_sends_nothing() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.sync();

    // The point of content addressing: a second sync of the same tree is
    // entirely talk and no bytes.
    let (result, uploaded) = pair.sync();

    assert_eq!(uploaded, 0);
    assert_eq!(result.unchanged, 1);
    assert_eq!(result.created + result.updated + result.deleted, 0);
}

#[test]
fn only_the_edited_file_goes_over_the_wire() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("hdl/other.vhd", "entity other is end;");
    pair.write("vw.toml", "[workspace]");
    pair.sync();

    pair.write("hdl/top.vhd", "entity top is end; -- edited");
    let (result, uploaded) = pair.sync();

    assert_eq!(uploaded, 1, "only the edited file should be delivered");
    assert_eq!(result.updated, 1);
    assert_eq!(result.unchanged, 2);
    assert_eq!(
        pair.receiver_contents("hdl/top.vhd"),
        "entity top is end; -- edited"
    );
}

#[test]
fn a_deleted_file_is_deleted_on_the_receiver() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("hdl/gone.vhd", "entity gone is end;");
    pair.sync();

    std::fs::remove_file(pair.sender.join("hdl/gone.vhd")).expect("remove");
    let (result, _) = pair.sync();

    // A stale source file is not harmless: it still compiles, and it is a
    // baffling way to spend an afternoon.
    assert_eq!(result.deleted, 1);
    assert_eq!(pair.receiver_paths(), ["hdl/top.vhd"]);
}

#[test]
fn a_rename_costs_nothing() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("hdl/big.vhd", &"x".repeat(100_000));
    pair.sync();

    std::fs::rename(
        pair.sender.join("hdl/big.vhd"),
        pair.sender.join("hdl/renamed.vhd"),
    )
    .expect("rename");
    let (result, uploaded) = pair.sync();

    // The bytes are already on the receiver under the old name, so nothing
    // needs to cross the wire — the receiver copies them into place itself.
    assert_eq!(uploaded, 0, "a rename should not re-send the content");
    assert_eq!(result.created, 1);
    assert_eq!(result.deleted, 1);
    assert_eq!(pair.receiver_paths(), ["hdl/renamed.vhd", "hdl/top.vhd"]);
    assert_eq!(pair.receiver_contents("hdl/renamed.vhd").len(), 100_000);
}

#[test]
fn a_whole_directory_can_move_without_resending_it() {
    let pair = Pair::new();
    for i in 0..5 {
        pair.write(&format!("hdl/old/f{i}.vhd"), &format!("entity f{i};"));
    }
    pair.sync();

    std::fs::rename(pair.sender.join("hdl/old"), pair.sender.join("hdl/new"))
        .expect("rename directory");
    let (_, uploaded) = pair.sync();

    assert_eq!(uploaded, 0);
    assert_eq!(
        pair.receiver_paths(),
        [
            "hdl/new/f0.vhd",
            "hdl/new/f1.vhd",
            "hdl/new/f2.vhd",
            "hdl/new/f3.vhd",
            "hdl/new/f4.vhd",
        ]
    );
}

#[test]
fn build_output_is_never_sent_or_deleted() {
    let pair = Pair::new();
    pair.write("vw.toml", "[workspace]");
    pair.write("hdl/top.vhd", "entity top is end;");
    // What a synthesis run leaves behind on the sender.
    pair.write("target/synth/top.dcp", "checkpoint");
    pair.write("driver/target/debug/thing", "binary");

    let (_, uploaded) = pair.sync();
    assert_eq!(uploaded, 2, "only the two source files");

    // And what a build on the receiver produces afterwards. A second sync must
    // leave it alone: deleting a synthesis run on every keystroke would be a
    // remarkable way to lose an afternoon.
    pair.write_receiver("target/synth/top.dcp", "receiver checkpoint");
    pair.write_receiver("driver/target/debug/thing", "receiver binary");

    pair.write("hdl/top.vhd", "entity top is end; -- edited");
    pair.sync();

    assert_eq!(
        pair.receiver_contents("target/synth/top.dcp"),
        "receiver checkpoint",
    );
    assert_eq!(
        pair.receiver_contents("driver/target/debug/thing"),
        "receiver binary",
    );
}

#[test]
fn gitignored_files_are_invisible_at_both_ends() {
    let pair = Pair::new();
    pair.write(".gitignore", "*.log\n*.fst\n");
    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("vivado.log", "noise");
    pair.write("bench/wave.fst", "waveform");

    let (_, uploaded) = pair.sync();
    assert_eq!(uploaded, 2, "the ignore file and the source file");
    assert_eq!(pair.receiver_paths(), [".gitignore", "hdl/top.vhd"]);

    // The receiver reads the same rules out of the tree it was handed, so its
    // own logs survive a sync rather than being treated as strays.
    pair.write_receiver("vivado.log", "receiver noise");
    pair.sync();
    assert_eq!(pair.receiver_contents("vivado.log"), "receiver noise");
}

#[test]
fn a_nested_ignore_file_applies_to_its_own_subtree() {
    let pair = Pair::new();
    pair.write("driver/.gitignore", "generated\n");
    pair.write("driver/Cargo.toml", "[package]");
    pair.write("driver/generated/bindings.rs", "// generated");
    pair.write("hdl/generated/keep.vhd", "not covered by driver's rules");

    pair.sync();

    // git reads nested ignore files as scoped to their directory, and so must
    // this — otherwise `driver`'s rules would quietly eat `hdl`'s files.
    assert_eq!(
        pair.receiver_paths(),
        [
            "driver/.gitignore",
            "driver/Cargo.toml",
            "hdl/generated/keep.vhd",
        ]
    );
}

#[test]
fn the_executable_bit_survives() {
    let pair = Pair::new();
    pair.write("tools/build.sh", "#!/bin/sh\necho hi\n");
    pair.write("hdl/top.vhd", "entity top is end;");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            pair.sender.join("tools/build.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }

    pair.sync();

    let manifest = scan(&pair.receiver).expect("scan receiver");
    let script = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "tools/build.sh")
        .expect("the script arrived");
    assert!(
        script.executable,
        "a build script that cannot run is no use"
    );

    let source = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "hdl/top.vhd")
        .expect("the source arrived");
    assert!(!source.executable);
}

#[test]
fn flipping_the_executable_bit_is_a_change() {
    let pair = Pair::new();
    pair.write("tools/build.sh", "#!/bin/sh\n");
    pair.sync();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            pair.sender.join("tools/build.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let (result, uploaded) = pair.sync();

        // The content did not change, so nothing needs sending — but the mode
        // did, so the file is still rewritten.
        assert_eq!(uploaded, 0);
        assert_eq!(result.updated, 1);
        assert!(scan(&pair.receiver)
            .expect("scan")
            .entries
            .iter()
            .any(|entry| entry.path == "tools/build.sh" && entry.executable));
    }
}

#[test]
fn emptied_directories_do_not_linger() {
    let pair = Pair::new();
    pair.write("hdl/keep.vhd", "entity keep is end;");
    pair.write("hdl/doomed/a.vhd", "entity a is end;");
    pair.write("hdl/doomed/b.vhd", "entity b is end;");
    pair.sync();

    std::fs::remove_dir_all(pair.sender.join("hdl/doomed")).expect("remove");
    pair.sync();

    assert!(
        !pair.receiver.join("hdl/doomed").exists(),
        "a directory with no sources left in it should go too",
    );
    assert!(pair.receiver.join("hdl").is_dir());
}

#[test]
fn a_receiver_with_a_different_tree_is_brought_into_line() {
    let pair = Pair::new();

    // What another machine left behind: some of it right, some stale, some
    // never on this sender at all.
    pair.write_receiver("hdl/top.vhd", "entity top is end;");
    pair.write_receiver("hdl/stale.vhd", "entity stale is end;");
    pair.write_receiver("old/thing.vhd", "entity thing is end;");

    pair.write("hdl/top.vhd", "entity top is end;");
    pair.write("hdl/new.vhd", "entity new is end;");

    let (result, uploaded) = pair.sync();

    // Working from a second machine is not a conflict to resolve — the
    // manifest says what should exist, and the receiver ends up saying it.
    assert_eq!(uploaded, 1, "only the file the receiver has never seen");
    assert_eq!(result.unchanged, 1);
    assert_eq!(result.created, 1);
    assert_eq!(result.deleted, 2);
    assert_eq!(pair.receiver_paths(), ["hdl/new.vhd", "hdl/top.vhd"]);
}

#[test]
fn a_manifest_cannot_write_outside_the_tree() {
    let pair = Pair::new();
    let store = &pair.store;

    // A manifest arrives over the wire and its paths become filesystem paths.
    for path in [
        "../escaped.vhd",
        "hdl/../../escaped.vhd",
        "/etc/passwd",
        "hdl/./top.vhd",
        "",
        "..",
        "C:\\windows\\system32",
    ] {
        let manifest = TreeManifest {
            entries: vec![vw_api_types_versions::latest::FileEntry {
                path: path.to_owned(),
                digest: vw_sync::digest_bytes(b"payload"),
                executable: false,
            }],
        };
        assert!(
            apply(&pair.receiver, store, &manifest).is_err(),
            "'{path}' should be refused",
        );
    }

    assert!(!pair.receiver.join("../escaped.vhd").exists());
}

#[test]
fn content_that_does_not_match_its_digest_is_refused() {
    let dir = TempDir::new().expect("scratch directory");
    let store = Store::new(
        Utf8Path::from_path(dir.path()).expect("utf8").join("store"),
    );

    let honest = vw_sync::digest_bytes(b"the real thing");
    assert!(store.put(&honest, b"the real thing").is_ok());

    // Every later lookup goes by digest, so storing content under a digest it
    // does not have would poison the store with something wrong under a name
    // that looks right.
    assert!(store.put(&honest, b"something else entirely").is_err());
    assert_eq!(store.get(&honest).expect("still intact"), b"the real thing");
}

#[test]
fn a_digest_cannot_escape_the_store() {
    let dir = TempDir::new().expect("scratch directory");
    let root = Utf8Path::from_path(dir.path()).expect("utf8");
    let store = Store::new(root.join("store"));

    // A digest names a file in the store, so it reaches the filesystem.
    for hostile in [
        "../../../../etc/passwd",
        "..",
        "",
        "not-hex-at-all",
        &"f".repeat(63),
        &"F".repeat(64),
    ] {
        let digest = Digest(hostile.to_owned());
        assert!(
            store.put(&digest, b"payload").is_err(),
            "'{hostile}' should be refused",
        );
        assert!(!store.has(&digest));
    }
}

#[test]
fn a_commit_with_content_still_undelivered_is_refused() {
    let pair = Pair::new();
    pair.write("hdl/top.vhd", "entity top is end;");

    // Committing without having uploaded what the plan asked for should fail
    // rather than write a truncated tree.
    let manifest = scan(&pair.sender).expect("scan");
    let result = apply(&pair.receiver, &pair.store, &manifest);

    assert!(result.is_err());
    assert!(pair.receiver_paths().is_empty());
}
