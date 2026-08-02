// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Resolving `--part` / `--variant` against a workspace.
//!
//! Lives here rather than in the CLI because whoever spawns Vivado has to do
//! this, and that is no longer only the machine the developer is sitting at.
//! An agent running a build on an instance resolves the same flags against the
//! same `vw.toml` — the copy in its own synced tree — and gets the same answer
//! for the same reasons.
//!
//! Nothing here prints. The CLI shows what it learns; an agent puts it in a
//! log and streams it back. Returning the notes instead of writing them lets
//! both be true of the same code.

use camino::Utf8Path;

use crate::AutoProject;

/// What a run's part and variant flags come to for one workspace.
pub struct Selection {
    /// The project to open, if the workspace names a part at all.
    pub auto_project: Option<AutoProject>,
    /// The variant in force, which `vw::vhdl_design_sources` filters on so a
    /// `design.htcl` does not have to name it.
    pub active_variant: Option<String>,
    /// Things worth telling the user that happened on the way — a legacy IP
    /// cache cleared, a stale project wiped, a fallback taken.
    pub notes: Vec<String>,
}

/// Resolve the part and variant flags against the workspace's declared parts
/// and variants.
///
/// Applies the mutual-exclusion rules: `--variant` against a part-mode
/// workspace is an error, `--part` against a variant-mode one is an error
/// (variants own their parts), and neither means the workspace's default.
pub fn resolve_workspace_selection(
    ws: &Utf8Path,
    part: Option<&str>,
    variant: Option<&str>,
) -> Result<Selection, String> {
    let Ok(cfg) = vw_lib::load_workspace_config(ws) else {
        return Ok(Selection {
            auto_project: None,
            active_variant: None,
            notes: Vec::new(),
        });
    };
    let ws_info = &cfg.workspace;

    if variant.is_some() && ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} has no `[[workspace.variants]]` block; \
             remove `--variant` or add variants to vw.toml",
        ));
    }
    if part.is_some() && !ws_info.variants.is_empty() {
        return Err(format!(
            "workspace at {ws} is variant-mode (has \
             `[[workspace.variants]]`); use `--variant <name>` instead of \
             `--part` — variants own their parts inline",
        ));
    }

    let mut notes = Vec::new();

    if !ws_info.variants.is_empty() {
        let selected =
            ws_info.select_variant(variant).map_err(|e| e.to_string())?;
        let Some(v) = selected else {
            return Ok(Selection {
                auto_project: None,
                active_variant: None,
                notes,
            });
        };
        let persist_dir = persist_dir(ws, &ws_info.name, &mut notes);
        Ok(Selection {
            auto_project: Some(AutoProject {
                name: ws_info.name.clone(),
                part: v.part.clone(),
                persist_dir,
            }),
            active_variant: Some(v.name.clone()),
            notes,
        })
    } else {
        let selected = ws_info
            .select_target_part(part)
            .map_err(|e| e.to_string())?;
        let persist_dir = persist_dir(ws, &ws_info.name, &mut notes);
        Ok(Selection {
            auto_project: selected.map(|p| AutoProject {
                name: ws_info.name.clone(),
                part: p.to_string(),
                persist_dir: persist_dir.clone(),
            }),
            active_variant: None,
            notes,
        })
    }
}

/// The on-disk Vivado project directory, after the one-shot legacy IP-cache
/// cleanup and staleness wipe.
///
/// A failure here is not fatal: it becomes a note and an in-memory project, so
/// the session still works. That covers a read-only workspace, or a `target/`
/// held by something else.
fn persist_dir(
    ws: &Utf8Path,
    name: &str,
    notes: &mut Vec<String>,
) -> Option<std::path::PathBuf> {
    match vw_lib::prepare_vw_project_dir(ws, name) {
        Ok(prep) => {
            if prep.legacy_cache_removed > 0 {
                notes.push(format!(
                    "removed {} legacy IP cache entr{y} under {ws}/target/ip \
                     — replaced by on-disk Vivado project",
                    prep.legacy_cache_removed,
                    y = if prep.legacy_cache_removed == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                ));
            }
            if let Some(wiped) = &prep.wiped_project {
                notes.push(format!(
                    "wiped stale Vivado project at {wiped} (source \
                     fingerprint changed or manifest missing)",
                ));
            }
            Some(prep.project_dir.into_std_path_buf())
        }
        Err(e) => {
            notes.push(format!(
                "failed to prepare on-disk Vivado project dir under \
                 {ws}/target/vw-project ({e}); falling back to in-memory \
                 project (state won't persist across sessions)",
            ));
            None
        }
    }
}
