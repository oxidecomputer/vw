// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The files vivado generates that a developer's own tools need.
//!
//! Not artifacts in the sense the object store holds — nobody wants to collect
//! these, and they are worthless a week later. They are the VHDL wrappers and
//! stubs that vivado writes for each configured IP, and without them a static
//! analysis of the design cannot resolve `entity ip.<name>_wrapper` or
//! `entity xil_defaultlib.<name>`. So they have to come back to the machine the
//! developer's language server is running on, at the same paths, or "go to
//! definition" lands nowhere.
//!
//! They are fetched rather than pushed, and by exact path rather than through
//! the object store, because a check is waiting on them. Somebody typing `vw
//! check` should not be waiting on a poll interval, an upload and a download of
//! something that is sitting in a directory two hops away.

use camino::{Utf8Path, Utf8PathBuf};
use vw_api_types_versions::latest::{FileEntry, TreeManifest};

/// Where vivado leaves generated VHDL within a build directory, and how to
/// recognise it.
///
/// Block design IP gets a wrapper per design under `ip`; standalone IP gets a
/// stub deep inside the vivado project's generated sources. The two live
/// nowhere near each other because vivado decides where they go, not us.
const GENERATED: [(&str, &str); 2] =
    [("ip", ".vhd"), ("vw-project", "_stub.vhdl")];

/// Every directory generated VHDL can be in, relative to the workspace, with
/// the suffix that marks it.
///
/// One set per build directory: a workspace with variants keeps each
/// variant's IP apart under `target/<variant>` (see `vw_lib::build_dir`), and
/// all of them are handed out — the client writes each file back to the same
/// path, so the variants stay apart on that side too.
fn generated_dirs(root: &Utf8Path) -> Vec<(Utf8PathBuf, &'static str)> {
    vw_lib::build_dirs(root)
        .into_iter()
        .filter_map(|build| {
            build.strip_prefix(root).ok().map(Utf8Path::to_owned)
        })
        .flat_map(|build| {
            GENERATED.iter().map(move |(directory, suffix)| {
                (build.join(directory), *suffix)
            })
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GeneratedError {
    #[error("'{0}' is not a path this instance will hand out")]
    UnsafePath(String),
    #[error("no generated file at '{0}'")]
    NotFound(String),
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
}

/// Write the stubs that only exist once something turns vivado's templates
/// into them.
///
/// Vivado writes an instantiation template per standalone IP; turning that into
/// a black-box entity is a mechanical splice done in Rust, and on a local run
/// it happens right after the vivado pass. On a remote one there is nobody on
/// this side to do it, so it happens here — where the templates are.
///
/// Idempotent and content-aware, so asking twice costs a directory walk.
pub(crate) fn prepare(root: &Utf8Path) -> usize {
    vw_lib::write_ip_stubs_from_templates(root).unwrap_or(0)
}

/// Every generated file, by the path it should have on the far end.
///
/// Paths are relative to the workspace, so a client can write each one exactly
/// where its own tools expect to find it.
pub(crate) fn manifest(root: &Utf8Path) -> TreeManifest {
    let mut entries = Vec::new();

    for (directory, suffix) in generated_dirs(root) {
        collect(root, &root.join(directory), suffix, &mut entries);
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    TreeManifest { entries }
}

/// Walk `directory`, adding every file whose name ends with `suffix`.
///
/// Recursive because vivado buries a stub five levels inside its project, and
/// the block-design wrappers sit one level down under a directory per design.
/// Nothing else in these trees matches, so the depth costs only the walk.
fn collect(
    root: &Utf8Path,
    directory: &Utf8Path,
    suffix: &str,
    into: &mut Vec<FileEntry>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };

        if path.is_dir() {
            collect(root, &path, suffix, into);
            continue;
        }
        if !path.as_str().ends_with(suffix) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };

        into.push(FileEntry {
            path: relative.to_string(),
            digest: vw_sync::digest_bytes(&contents),
            executable: false,
        });
    }
}

/// One generated file's contents.
///
/// The path arrives from a caller and is joined onto this instance's tree, so
/// it is checked before it is used — and checked again against what this
/// instance is actually willing to hand out, so a well formed path to
/// somewhere else on the filesystem is refused too.
pub(crate) fn read(
    root: &Utf8Path,
    path: &str,
) -> Result<Vec<u8>, GeneratedError> {
    let unsafe_path = || GeneratedError::UnsafePath(path.to_owned());

    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(unsafe_path());
    }
    for component in path.split('/') {
        if component.is_empty() || component == ".." || component == "." {
            return Err(unsafe_path());
        }
    }

    // Only the places generated VHDL lives, and only files that look like it.
    let allowed = generated_dirs(root).iter().any(|(directory, suffix)| {
        path.starts_with(&format!("{directory}/")) && path.ends_with(suffix)
    });
    if !allowed {
        return Err(unsafe_path());
    }

    let full = root.join(path);
    if !full.is_file() {
        return Err(GeneratedError::NotFound(path.to_owned()));
    }

    std::fs::read(&full).map_err(|e| GeneratedError::Read(full, e))
}

#[cfg(test)]
mod test {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        (dir, root)
    }

    fn write(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    /// A tree shaped the way vivado leaves one.
    fn generated_tree(root: &Utf8Path) {
        write(root, "target/ip/cips/wrapper.vhd", "-- cips");
        write(root, "target/ip/dcmac/wrapper.vhd", "-- dcmac");
        write(
            root,
            "target/vw-project/metroid/metroid.gen/sources_1/ip/\
             primary_clock/primary_clock_stub.vhdl",
            "-- primary_clock",
        );
        write(
            root,
            "target/vw-project/metroid/metroid.gen/sources_1/ip/clk_eth/\
             clk_eth_stub.vhdl",
            "-- clk_eth",
        );
    }

    #[test]
    fn both_kinds_of_generated_vhdl_are_found() {
        // Block design wrappers and standalone stubs live nowhere near each
        // other, and a check needs both to resolve the design.
        let (_dir, root) = scratch();
        generated_tree(&root);

        let paths: Vec<String> = manifest(&root)
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();

        assert_eq!(
            paths,
            [
                "target/ip/cips/wrapper.vhd",
                "target/ip/dcmac/wrapper.vhd",
                "target/vw-project/metroid/metroid.gen/sources_1/ip/clk_eth/\
                 clk_eth_stub.vhdl",
                "target/vw-project/metroid/metroid.gen/sources_1/ip/\
                 primary_clock/primary_clock_stub.vhdl",
            ],
        );
    }

    #[test]
    fn the_rest_of_a_vivado_project_is_not_generated_vhdl() {
        // The project directory is enormous and almost none of it means
        // anything on another machine.
        let (_dir, root) = scratch();
        generated_tree(&root);
        write(
            root.as_ref(),
            "target/vw-project/metroid/metroid.xpr",
            "proj",
        );
        write(
            root.as_ref(),
            "target/vw-project/metroid/metroid.gen/sources_1/ip/clk_eth/\
             clk_eth.vho",
            "template",
        );
        write(root.as_ref(), "target/ip/cips/wrapper.dcp", "checkpoint");
        write(root.as_ref(), "target/image/top.pdi", "image");

        let paths: Vec<String> = manifest(&root)
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();

        assert_eq!(paths.len(), 4, "only the wrappers and stubs: {paths:?}");
    }

    #[test]
    fn every_variants_generated_vhdl_is_handed_over() {
        // Each variant generates into `target/<variant>`, often with the
        // same IP names. The client writes each file back to the same
        // path, so both variants' IP arrive and stay apart.
        let (_dir, root) = scratch();
        write(
            &root,
            "vw.toml",
            "[workspace]\nname = \"twin\"\nversion = \"0.1.0\"\n\n\
             [[workspace.variants]]\nname = \"alpha\"\npart = \"p\"\n\
             default = true\n\n\
             [[workspace.variants]]\nname = \"beta\"\npart = \"p\"\n",
        );
        write(&root, "target/alpha/ip/txr0/wrapper.vhd", "-- dual quad");
        write(&root, "target/beta/ip/txr0/wrapper.vhd", "-- single quad");
        write(
            &root,
            "target/beta/vw-project/twin/twin.gen/sources_1/ip/clk_eth/\
             clk_eth_stub.vhdl",
            "-- clk_eth",
        );
        // Left over from the flat layout; not where this workspace's IP is.
        write(&root, "target/ip/txr0/wrapper.vhd", "-- stale");

        let paths: Vec<String> = manifest(&root)
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();

        assert_eq!(
            paths,
            [
                "target/alpha/ip/txr0/wrapper.vhd",
                "target/beta/ip/txr0/wrapper.vhd",
                "target/beta/vw-project/twin/twin.gen/sources_1/ip/clk_eth/\
                 clk_eth_stub.vhdl",
            ],
        );
        assert_eq!(
            read(&root, "target/beta/ip/txr0/wrapper.vhd").expect("served"),
            b"-- single quad",
        );
        assert!(matches!(
            read(&root, "target/ip/txr0/wrapper.vhd"),
            Err(GeneratedError::UnsafePath(_)),
        ));
        assert!(matches!(
            read(&root, "target/beta/image/beta.pdi"),
            Err(GeneratedError::UnsafePath(_)),
        ));
    }

    #[test]
    fn a_workspace_with_no_generated_ip_has_nothing_to_hand_over() {
        let (_dir, root) = scratch();
        assert!(manifest(&root).entries.is_empty());
    }

    #[test]
    fn a_generated_file_comes_back_by_path() {
        let (_dir, root) = scratch();
        generated_tree(&root);

        let contents = read(&root, "target/ip/cips/wrapper.vhd").expect("read");

        assert_eq!(contents, b"-- cips");
    }

    #[test]
    fn nothing_outside_the_generated_directories_is_handed_out() {
        let (_dir, root) = scratch();
        generated_tree(&root);
        write(root.as_ref(), "secrets.vhd", "-- not yours");
        write(root.as_ref(), "target/logs/vivado.log", "log");

        for path in [
            "../secrets.vhd",
            "/etc/passwd",
            "target/ip/../../secrets.vhd",
            // Well formed, inside the tree, and still not ours to serve.
            "secrets.vhd",
            "target/logs/vivado.log",
            // The right directory, the wrong kind of file.
            "target/vw-project/metroid/metroid.xpr",
        ] {
            assert!(
                read(&root, path).is_err(),
                "'{path}' should have been refused",
            );
        }
    }
}
