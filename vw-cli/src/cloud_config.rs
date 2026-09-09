// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw-cloud.toml` — what this particular checkout does in the cloud.
//!
//! Two facts live here: which environment this checkout builds in, and
//! which slot on that environment it occupies. Both are properties of
//! the checkout rather than of the project, which is why they are not
//! in `vw.toml`.
//!
//! That distinction is the whole point. A developer comparing `main`
//! against a feature branch wants two trees on one environment, which
//! means the two checkouts have to disagree about their slot — and a
//! disagreement recorded in `vw.toml` would be a modification to a
//! tracked file, waiting to be committed by accident and carried to
//! everybody else.
//!
//! ## The workspace here is not the design's identity
//!
//! `[workspace] name` in `vw.toml` is what a workspace's own imports
//! resolve through (`src @foo/bar`), and what `vw::project_name`
//! hands a design at build time. Override *that* and the feature
//! checkout stops resolving its own imports — the two things being
//! compared would differ in a way that has nothing to do with the
//! change under test.
//!
//! So what this overrides is only the key: which directory on an
//! instance the tree goes in, which content store stands behind it,
//! and which bucket its artifacts land in. The build sees exactly
//! what it would have seen.

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

/// What the file is called, at the root of a workspace.
pub const FILE: &str = "vw-cloud.toml";

/// What is written into `.gitignore` when this file is first saved.
///
/// It has to be ignored or the mechanism defeats itself: a slot name
/// committed to a branch travels to everyone who checks that branch
/// out, and every one of them lands in the same slot again. Being
/// ignored also keeps it out of the synchronized tree, since the scan
/// honours `.gitignore` — which is right, because the instance has no
/// use for a file describing how to reach it.
const IGNORE_LINE: &str = FILE;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct CloudConfig {
    /// The environment this checkout builds in.
    ///
    /// Saves saying so on every command. `$VW_ENV` and an explicit
    /// argument still win — this is the standing answer, not an
    /// override of what somebody just typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,

    /// The slot on that environment this checkout occupies.
    ///
    /// Absent means `[workspace] name` from `vw.toml`, which is what
    /// a single checkout of a project wants. Set it when a second
    /// checkout of the same project needs a slot of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}")]
    Read(Utf8PathBuf, #[source] std::io::Error),
    #[error("{0} is not valid toml: {1}")]
    Parse(Utf8PathBuf, String),
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("encoding {0}: {1}")]
    Encode(Utf8PathBuf, String),
}

/// The path to a workspace's cloud settings.
pub fn path(workspace_dir: &Utf8Path) -> Utf8PathBuf {
    workspace_dir.join(FILE)
}

/// What this checkout has been told, if anything.
pub fn load(workspace_dir: &Utf8Path) -> Result<CloudConfig, ConfigError> {
    let path = path(workspace_dir);
    if !path.is_file() {
        return Ok(CloudConfig::default());
    }

    let contents = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Read(path.clone(), e))?;
    toml::from_str(&contents)
        .map_err(|e| ConfigError::Parse(path, e.to_string()))
}

/// Write this checkout's cloud settings, and make sure git ignores them.
pub fn save(
    workspace_dir: &Utf8Path,
    config: &CloudConfig,
) -> Result<(), ConfigError> {
    let path = path(workspace_dir);
    let encoded = toml::to_string_pretty(config)
        .map_err(|e| ConfigError::Encode(path.clone(), e.to_string()))?;

    std::fs::write(&path, encoded)
        .map_err(|e| ConfigError::Write(path.clone(), e))?;

    // Here rather than at `vw init`, because here is the moment the
    // file first exists — an ignore rule written earlier would be for
    // a file that may never appear, and one written later is already
    // too late for whoever ran `git add -A` in between.
    ignore(workspace_dir);

    Ok(())
}

/// Add `vw-cloud.toml` to the workspace's `.gitignore` if it is not
/// already covered.
///
/// Best effort, and deliberately so. A workspace that is not a git
/// checkout at all, or one whose `.gitignore` cannot be written, is
/// not a reason to refuse to record a setting the developer asked for.
fn ignore(workspace_dir: &Utf8Path) {
    let path = workspace_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == IGNORE_LINE) {
        return;
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(IGNORE_LINE);
    updated.push('\n');

    let _ = std::fs::write(&path, updated);
}

#[cfg(test)]
mod test {
    use super::*;

    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        (dir, path)
    }

    #[test]
    fn a_workspace_with_no_settings_has_none() {
        let (_dir, root) = scratch();
        let config = load(&root).unwrap();
        assert!(config.environment.is_none());
        assert!(config.workspace.is_none());
    }

    #[test]
    fn what_is_saved_is_what_is_read_back() {
        let (_dir, root) = scratch();
        save(
            &root,
            &CloudConfig {
                environment: Some("darmok".to_owned()),
                workspace: Some("redhawk-feat".to_owned()),
            },
        )
        .unwrap();

        let config = load(&root).unwrap();
        assert_eq!(config.environment.as_deref(), Some("darmok"));
        assert_eq!(config.workspace.as_deref(), Some("redhawk-feat"));
    }

    /// Committing a slot name would send every checkout of that branch
    /// to the same slot, which is the thing this file exists to avoid.
    #[test]
    fn saving_makes_git_ignore_the_file() {
        let (_dir, root) = scratch();
        save(&root, &CloudConfig::default()).unwrap();

        let ignored = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(ignored.lines().any(|line| line == FILE), "{ignored}");
    }

    #[test]
    fn an_existing_gitignore_is_added_to_rather_than_replaced() {
        let (_dir, root) = scratch();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();

        save(&root, &CloudConfig::default()).unwrap();

        let ignored = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(ignored.contains("target/"));
        assert!(ignored.contains("*.log"));
        assert!(ignored.lines().any(|line| line == FILE));
    }

    #[test]
    fn a_gitignore_that_already_covers_it_is_left_alone() {
        let (_dir, root) = scratch();
        let original = format!("target/\n{FILE}\n");
        std::fs::write(root.join(".gitignore"), &original).unwrap();

        save(&root, &CloudConfig::default()).unwrap();

        let ignored = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(ignored, original);
    }

    /// A file with no trailing newline is common enough, and appending
    /// to one blindly would comment the last line out by joining it.
    #[test]
    fn a_gitignore_without_a_trailing_newline_is_not_mangled() {
        let (_dir, root) = scratch();
        std::fs::write(root.join(".gitignore"), "target/").unwrap();

        save(&root, &CloudConfig::default()).unwrap();

        let ignored = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(ignored, format!("target/\n{FILE}\n"));
    }
}
