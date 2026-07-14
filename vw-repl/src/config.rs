// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Workspace-local REPL configuration loaded from `<ws>/.vw/repl.toml`.
//!
//! Optional file — the REPL runs fine without it. Present so a
//! project can pin per-workspace UI preferences (currently just the
//! auto-collapse policy for scrollback entries) without touching
//! `vw.toml`, which is the *build* manifest and shouldn't be
//! littered with editor-UX knobs.

use std::path::Path;

use serde::Deserialize;

/// How aggressively the REPL auto-collapses multi-line scrollback
/// entries when they land. Single-line entries are never
/// collapsible regardless of mode — a `▶` around one row of text
/// is worse UX than the row itself.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollapseMode {
    /// Auto-collapse only past [`crate::app::COLLAPSE_AUTO_THRESHOLD`]
    /// lines. Smaller multi-line entries land expanded but stay
    /// toggleable via Shift+click. The default — matches the
    /// out-of-the-box behavior users see without a config file.
    #[default]
    Normal,
    /// Every collapsible entry (>=2 lines) starts collapsed.
    /// Turns the scrollback into a compact index of `▶`-marked
    /// placeholders that expand on demand — useful when running
    /// long batches where most output is chatter you scroll past.
    Aggressive,
}

/// Deserialized `<ws>/.vw/repl.toml`. All fields optional so a
/// stub `[ui]` section is legal — missing keys fall back to
/// [`Default`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ReplConfig {
    pub ui: UiConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub collapse: CollapseMode,
}

/// Load `<ws>/.vw/repl.toml` if it exists. Absent file → default
/// config. Malformed file → default config + a `tracing::warn!` so
/// the user notices in the verbose log but the REPL still starts;
/// a config error shouldn't be a fatal boot condition for an
/// interactive tool.
pub fn load(workspace_root: Option<&Path>) -> ReplConfig {
    let Some(ws) = workspace_root else {
        return ReplConfig::default();
    };
    let path = ws.join(".vw").join("repl.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ReplConfig::default();
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to read repl.toml — using defaults",
            );
            return ReplConfig::default();
        }
    };
    match toml::from_str::<ReplConfig>(&raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "malformed repl.toml — using defaults",
            );
            ReplConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load(Some(tmp.path()));
        assert_eq!(cfg.ui.collapse, CollapseMode::Normal);
    }

    #[test]
    fn no_workspace_root_is_default() {
        let cfg = load(None);
        assert_eq!(cfg.ui.collapse, CollapseMode::Normal);
    }

    #[test]
    fn parses_aggressive() {
        let tmp = tempfile::tempdir().unwrap();
        let vw_dir = tmp.path().join(".vw");
        std::fs::create_dir_all(&vw_dir).unwrap();
        std::fs::write(
            vw_dir.join("repl.toml"),
            "[ui]\ncollapse = \"aggressive\"\n",
        )
        .unwrap();
        let cfg = load(Some(tmp.path()));
        assert_eq!(cfg.ui.collapse, CollapseMode::Aggressive);
    }

    #[test]
    fn parses_normal_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let vw_dir = tmp.path().join(".vw");
        std::fs::create_dir_all(&vw_dir).unwrap();
        std::fs::write(
            vw_dir.join("repl.toml"),
            "[ui]\ncollapse = \"normal\"\n",
        )
        .unwrap();
        let cfg = load(Some(tmp.path()));
        assert_eq!(cfg.ui.collapse, CollapseMode::Normal);
    }

    #[test]
    fn empty_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let vw_dir = tmp.path().join(".vw");
        std::fs::create_dir_all(&vw_dir).unwrap();
        std::fs::write(vw_dir.join("repl.toml"), "").unwrap();
        let cfg = load(Some(tmp.path()));
        assert_eq!(cfg.ui.collapse, CollapseMode::Normal);
    }

    #[test]
    fn malformed_file_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let vw_dir = tmp.path().join(".vw");
        std::fs::create_dir_all(&vw_dir).unwrap();
        std::fs::write(
            vw_dir.join("repl.toml"),
            "[ui]\ncollapse = \"chaotic\"\n",
        )
        .unwrap();
        let cfg = load(Some(tmp.path()));
        assert_eq!(cfg.ui.collapse, CollapseMode::Normal);
    }
}
