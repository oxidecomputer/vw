// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The cargo workspace under `bench/`.
//!
//! Every Rust-driven testbench — cosim and mixed-signal alike — is a crate,
//! and they all live in one cargo workspace rooted at `bench/Cargo.toml`. A
//! crate that exists on disk but is not a member of it does not build, and a
//! member that does not exist on disk stops the workspace manifest loading at
//! all, which breaks *every* bench rather than one. So creating a bench crate
//! and registering it are the same operation, done here.

use camino::{Utf8Path, Utf8PathBuf};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::{Result, VwError};

/// Where the cosim framework comes from.
///
/// Pinned to a branch rather than a tag because that is how every workspace
/// using it declares it today; a workspace that wants otherwise edits the one
/// line in `bench/Cargo.toml` afterwards.
const RUST_COSIM_GIT: &str = "https://github.com/oxidecomputer/rust-cosim";
const RUST_COSIM_BRANCH: &str = "main";

/// A fresh `bench/Cargo.toml`.
///
/// `[workspace.dependencies]` carries the handful every testbench reaches
/// for, so an individual bench's manifest is `rust-cosim.workspace = true`
/// and not a URL. `members` starts empty and is filled in by
/// [`register_member`].
fn workspace_manifest() -> String {
    format!(
        r#"# The cargo workspace holding this design's Rust testbenches.
#
# `vw cosim init` and `vw mist init` add their crate to `members`. A member
# listed here must exist on disk: cargo refuses to load the workspace at all
# otherwise, which would break every bench rather than one.
[workspace]
members = []
resolver = "2"

[workspace.dependencies]
rust-cosim = {{ git = "{RUST_COSIM_GIT}", branch = "{RUST_COSIM_BRANCH}" }}
futures = "0.3"
log = "0.4"
env_logger = "0.11"
"#
    )
}

/// `bench/`'s own ignores.
///
/// `target` is this cargo workspace's build output. `generated_structs.rs` is
/// what anodizer writes for `serialize_rust`-tagged records — generated on
/// every run from the design sources, so a checked-in copy could only ever be
/// stale.
const BENCH_GITIGNORE: &str = "\
target
generated_structs.rs
";

/// Cargo's git CLI is used rather than its built-in fetcher so that a
/// private dependency resolves with whatever credentials git already has —
/// ssh agent, netrc, a credential helper — instead of needing them restated
/// for cargo.
const BENCH_CARGO_CONFIG: &str = "\
[net]
git-fetch-with-cli = true
";

/// Create `bench/` and its cargo workspace if they are not there yet.
///
/// Returns the files it wrote. Existing files are left exactly as they are:
/// this runs before every `init`, and a workspace that has been edited is the
/// developer's, not ours to normalize.
pub fn ensure_bench_workspace(
    workspace_dir: &Utf8Path,
) -> Result<Vec<Utf8PathBuf>> {
    let bench_dir = workspace_dir.join("bench");
    std::fs::create_dir_all(bench_dir.as_std_path())?;

    let mut written = Vec::new();
    for (path, contents) in [
        (bench_dir.join("Cargo.toml"), workspace_manifest()),
        (bench_dir.join(".gitignore"), BENCH_GITIGNORE.to_string()),
        (
            bench_dir.join(".cargo").join("config.toml"),
            BENCH_CARGO_CONFIG.to_string(),
        ),
    ] {
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent.as_std_path())?;
        }
        std::fs::write(path.as_std_path(), contents)?;
        written.push(path);
    }
    Ok(written)
}

/// Add `member` to `bench/Cargo.toml`'s `members`, if it is not already
/// there. Returns whether the manifest changed.
///
/// Edited with `toml_edit` rather than parsed and re-serialized so that a
/// workspace whose manifest carries comments, a hand-grouped dependency list
/// or a particular layout gets one line added and nothing else touched.
pub fn register_member(workspace_dir: &Utf8Path, member: &str) -> Result<bool> {
    let manifest = workspace_dir.join("bench").join("Cargo.toml");
    let text =
        std::fs::read_to_string(manifest.as_std_path()).map_err(|e| {
            VwError::FileSystem {
                message: format!("reading {manifest}: {e}"),
            }
        })?;
    let mut doc = text.parse::<DocumentMut>().map_err(|e| VwError::Config {
        message: format!("parsing {manifest}: {e}"),
    })?;

    let workspace = doc
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| VwError::Config {
            message: format!("{manifest} has no [workspace] table"),
        })?;

    let members = match workspace.get_mut("members") {
        Some(item) => item.as_array_mut().ok_or_else(|| VwError::Config {
            message: format!("{manifest}: workspace.members is not an array"),
        })?,
        None => {
            workspace
                .insert("members", Item::Value(Value::Array(Array::new())));
            workspace
                .get_mut("members")
                .and_then(Item::as_array_mut)
                .expect("just inserted an array")
        }
    };

    if members.iter().any(|v| v.as_str() == Some(member)) {
        return Ok(false);
    }
    members.push(member);
    // Cargo does not care about the order, but a developer reading a diff
    // does — an appended-to list that stays sorted shows one added line
    // rather than a reshuffle.
    let mut sorted: Vec<String> = members
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    sorted.sort();
    let mut rebuilt = Array::new();
    for entry in sorted {
        rebuilt.push(entry);
    }
    rebuilt.set_trailing_comma(true);
    rebuilt.iter_mut().for_each(|v| {
        v.decor_mut().set_prefix("\n    ");
    });
    rebuilt.set_trailing("\n");
    workspace.insert("members", Item::Value(Value::Array(rebuilt)));

    std::fs::write(manifest.as_std_path(), doc.to_string())?;
    Ok(true)
}

/// Drop `member` from `bench/Cargo.toml`'s `members`, if it is there.
/// Returns whether the manifest changed.
///
/// The counterpart to [`register_member`], and just as necessary: a member
/// cargo cannot find on disk stops the whole bench workspace loading, so
/// deleting a bench crate without this breaks every other bench.
pub fn unregister_member(
    workspace_dir: &Utf8Path,
    member: &str,
) -> Result<bool> {
    let manifest = workspace_dir.join("bench").join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(manifest.as_std_path()) else {
        // No bench workspace to keep tidy.
        return Ok(false);
    };
    let mut doc = text.parse::<DocumentMut>().map_err(|e| VwError::Config {
        message: format!("parsing {manifest}: {e}"),
    })?;

    let Some(members) = doc
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .and_then(|w| w.get_mut("members"))
        .and_then(Item::as_array_mut)
    else {
        return Ok(false);
    };

    let before = members.len();
    members.retain(|v| v.as_str() != Some(member));
    if members.len() == before {
        return Ok(false);
    }

    std::fs::write(manifest.as_std_path(), doc.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path =
            Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    /// A workspace with no `bench/` at all gets a complete, loadable cargo
    /// workspace — the case a brand new design is in the first time anyone
    /// writes a testbench for it.
    #[test]
    fn a_missing_bench_workspace_is_created_whole() {
        let (_guard, ws) = scratch();
        let written = ensure_bench_workspace(&ws).unwrap();

        assert_eq!(written.len(), 3);
        assert!(ws.join("bench/Cargo.toml").exists());
        assert!(ws.join("bench/.gitignore").exists());
        assert!(ws.join("bench/.cargo/config.toml").exists());
    }

    /// Run twice, nothing is rewritten. `init` calls this every time, and a
    /// second bench must not clobber the first one's registrations or a
    /// hand-edited manifest.
    #[test]
    fn an_existing_bench_workspace_is_left_alone() {
        let (_guard, ws) = scratch();
        ensure_bench_workspace(&ws).unwrap();
        std::fs::write(
            ws.join("bench/Cargo.toml").as_std_path(),
            "[workspace]\nmembers = [\"kept\"]\n",
        )
        .unwrap();

        assert!(ensure_bench_workspace(&ws).unwrap().is_empty());
        let text =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        assert!(text.contains("kept"));
    }

    /// Registering is idempotent and keeps the list sorted, so `init` can be
    /// re-run and a diff shows one line.
    #[test]
    fn members_are_registered_once_and_stay_sorted() {
        let (_guard, ws) = scratch();
        ensure_bench_workspace(&ws).unwrap();

        assert!(register_member(&ws, "zed").unwrap());
        assert!(register_member(&ws, "alpha").unwrap());
        // Already there — no second entry, and the manifest is untouched.
        assert!(!register_member(&ws, "alpha").unwrap());

        let text =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        let members: Vec<&str> = doc["workspace"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(members, ["alpha", "zed"]);
    }

    /// Removing a bench takes its member with it. A member cargo cannot find
    /// on disk stops the workspace loading, which would break every other
    /// bench rather than the one that was deleted.
    #[test]
    fn members_are_unregistered_when_a_bench_goes() {
        let (_guard, ws) = scratch();
        ensure_bench_workspace(&ws).unwrap();
        register_member(&ws, "fifo").unwrap();
        register_member(&ws, "other").unwrap();

        assert!(unregister_member(&ws, "fifo").unwrap());
        // Gone already — nothing to do, and no error.
        assert!(!unregister_member(&ws, "fifo").unwrap());

        let text =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        let members: Vec<&str> = doc["workspace"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(members, ["other"]);
    }

    /// A manifest carrying comments and other tables keeps them. The whole
    /// reason for `toml_edit` here.
    #[test]
    fn registering_preserves_the_rest_of_the_manifest() {
        let (_guard, ws) = scratch();
        std::fs::create_dir_all(ws.join("bench").as_std_path()).unwrap();
        std::fs::write(
            ws.join("bench/Cargo.toml").as_std_path(),
            "# hand written\n[workspace]\nmembers = []\nresolver = \"2\"\n\n\
             [workspace.dependencies]\nrust-cosim = { path = \"../../cosim\" }\n",
        )
        .unwrap();

        register_member(&ws, "fifo").unwrap();

        let text =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        assert!(text.contains("# hand written"));
        assert!(text.contains(r#"rust-cosim = { path = "../../cosim" }"#));
        assert!(text.contains("fifo"));
    }
}
