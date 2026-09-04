// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Core library for VHDL workspace management.
//!
//! This library provides functionality for:
//! - Managing VHDL project dependencies from git repositories
//! - Running testbenches with the NVC simulator
//! - Generating vhdl_ls configuration files
//!
//! # Example
//!
//! ```no_run
//! use vw_lib::{init_workspace, update_workspace};
//! use camino::Utf8Path;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let workspace_dir = Utf8Path::new(".");
//!
//! // Initialize a new workspace
//! init_workspace(workspace_dir, "my_project".to_string(), None)?;
//!
//! // Update dependencies
//! update_workspace(workspace_dir).await?;
//! # Ok(())
//! # }
//! ```

use std::cell::RefCell;
use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use camino::{Utf8Path, Utf8PathBuf};
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};

use crate::nvc_helpers::{run_nvc_analysis, run_nvc_elab, run_nvc_sim};
use vw_core::parse_entities;

pub mod bench_init;
pub mod cosim;
pub mod parts;
pub mod sim;

// The low-level VHDL-analysis + nvc primitives now live in `vw-core`.
// Re-export them so existing `crate::…` / `vw_lib::…` paths (in this crate's
// remaining workflow code and in downstream crates) keep resolving.
pub use vw_core::{
    analyze_ext_libraries, find_referenced_files, load_existing_vhdl_ls_config,
    sort_files_by_dependencies, FileCache, RecordProcessor, Result,
    VhdlLsConfig, VhdlLsLibrary, VhdlStandard, VwError,
};
pub use vw_core::{mapping, nvc_helpers, visitor};

/// Start a `cargo` that resolves its own toolchain.
///
/// A bench workspace pins its toolchain — anodizer's output needs nightly —
/// and rustup resolves that from the directory being built in. But cargo
/// exports these to anything it spawns, and `RUSTUP_TOOLCHAIN` beats the
/// directory: inherited, a bench builds with whatever toolchain launched
/// `vw` instead of the one it asked for, and fails on a `#![feature]` that
/// is perfectly legal where it is written. Nothing sets them outside a cargo
/// invocation, so clearing them costs nothing and restores the pin when
/// something does.
pub fn cargo_command() -> std::process::Command {
    let mut command = std::process::Command::new("cargo");
    for leaked in ["RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC", "CARGO"] {
        command.env_remove(leaked);
    }
    command
}

/// Workspace-relative directory for vw's own testbench simulation build (the
/// nvc `work` + dependency libraries). Kept under `target/` so all generated
/// output lives there. (anodizer's separate build is `target/anodizer/build`.)
pub const BUILD_DIR: &str = "target/sim";

// ============================================================================
// Configuration Structures
// ============================================================================

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    #[allow(dead_code)]
    pub workspace: WorkspaceInfo,
    #[serde(default)]
    pub dependencies: HashMap<String, Dependency>,
    /// Test-only dependencies. Only the ENTRY workspace's
    /// `[test-dependencies]` are honored — a transitive dep's
    /// test-deps are private to itself. Cargo-parity semantic for
    /// `dev-dependencies`. Consumed by `vw test` via
    /// [`transitive_dep_cache_paths_with_test`].
    #[serde(default, rename = "test-dependencies")]
    pub test_dependencies: HashMap<String, Dependency>,
    /// Library-scope: the set of Vivado device families/parts the
    /// files in this workspace support. Populated by `vw ip
    /// generate` from the underlying IP's `<xilinx:supported
    /// Families>`, normalized so every entry has a brace-form
    /// regex against the raw part name.
    ///
    /// Project workspaces (those that declare `[workspace]
    /// target-part`) omit this — projects consume libraries, they
    /// don't publish their own supported-parts list.
    #[serde(default)]
    pub targets: Option<TargetsConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkspaceInfo {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    /// Project-scope: every Vivado device part this workspace can
    /// target. Each entry is a full Vivado specifier (e.g.
    /// `xcvp1202-vsva2785-2MHP-e-S`) — package and speed grade
    /// matter for downstream implementation / timing analysis
    /// even if the family alone is enough for IP-availability
    /// checks.
    ///
    /// One entry must be marked `default = true` when the list
    /// has more than one; the default drives the auto-project
    /// on `vw run` / `vw repl` / `vw test`. The CLI's
    /// `--part <id-or-substring>` flag selects a non-default
    /// entry.
    ///
    /// Empty for library workspaces (they publish `[targets]`
    /// instead — see [`TargetsConfig`]).
    ///
    /// Mutually exclusive with [`variants`](Self::variants) — a
    /// workspace declares one shape or the other, never both.
    /// Variants own their parts inline; `[[target-parts]]` is for
    /// projects whose parts are truly interchangeable and don't
    /// change what source files compile.
    #[serde(default, rename = "target-parts")]
    pub target_parts: Vec<TargetPart>,
    /// Project-scope: named feature-flag-style variants. Each
    /// variant declares its own part inline and an optional list
    /// of `exclusive` file paths (workspace-relative globs) that
    /// are ONLY compiled when that variant is active.
    ///
    /// Mutually exclusive with [`target_parts`](Self::target_parts)
    /// — see the docstring there. Empty for the common
    /// "no variants" case, in which case part selection is
    /// driven purely by `[[target-parts]]`.
    ///
    /// One entry must be marked `default = true` when the list
    /// has more than one; the default drives the auto-project on
    /// `vw run` / `vw repl` / `vw test`. The CLI's
    /// `--variant <name>` flag selects a non-default entry.
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// Project-scope: default top-entity name. Consumed by
    /// `vw::synth` (as the fallback when `-top` isn't passed) and
    /// by `vw::_resolve_top` (used by `vw::place` / `vw::route` /
    /// `vw::report` to derive the DCP / report paths).
    ///
    /// A variant with its own `top` overrides this — see
    /// [`Variant::top`]. When neither this nor the active
    /// variant's `top` is set, callers fall back to fileset TOP /
    /// current_design NAME / an explicit `-top` from the user.
    ///
    /// Typical usage: a workspace with a single top-level entity
    /// sets this once at project scope. Multi-variant workspaces
    /// with per-board tops (e.g. `top_vpk120` / `top_metro`) set
    /// it per-variant instead.
    #[serde(default)]
    pub top: Option<String>,
}

/// One entry in a workspace's `[[target-parts]]` list.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TargetPart {
    /// Full Vivado part identifier, e.g.
    /// `xcvp1202-vsva2785-2MHP-e-S`.
    pub part: String,
    /// `true` marks this entry as the default target part. Exactly
    /// one entry must set this when the list has more than one
    /// entry. A single-entry list may omit the flag — the sole
    /// entry is implicitly the default.
    #[serde(default)]
    pub default: bool,
}

/// Errors surfaced when validating or selecting a target part
/// from a workspace's `[[target-parts]]` list. Wrapped by the
/// `WorkspaceInfo` accessors so callers can render precise
/// diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum TargetSelectError {
    #[error(
        "workspace has {count} `[[target-parts]]` entries but none are marked \
         `default = true`; add `default = true` to exactly one entry"
    )]
    NoDefault { count: usize },
    #[error(
        "workspace has multiple `[[target-parts]]` entries marked \
         `default = true` ({defaults:?}); only one may be default"
    )]
    MultipleDefaults { defaults: Vec<String> },
    #[error("no `[[target-parts]]` entry matches `{query}`")]
    NoMatch { query: String },
    #[error(
        "`{query}` matches multiple `[[target-parts]]` entries ({matches:?}); \
         disambiguate with a longer substring or the full part ID"
    )]
    Ambiguous { query: String, matches: Vec<String> },
}

/// One entry in a workspace's `[[workspace.variants]]` list.
///
/// A variant is a feature-flag-style selection that:
/// - pins a specific Vivado part (inline, not via cross-reference)
/// - optionally names an `exclusive` list of source-file globs
///   (workspace-relative) that ONLY compile when this variant
///   is the active one.
///
/// Files NOT listed in any variant's `exclusive` set are shared —
/// they always contribute to `vhdl_design_sources` regardless
/// of the active variant.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Variant {
    /// Human-facing selector, e.g. `vpk120` or `metro`. Matched
    /// exactly by `--variant <name>`. Must be unique within a
    /// workspace's variants list.
    pub name: String,
    /// Full Vivado part identifier, e.g.
    /// `xcvp1202-vsva2785-2MHP-e-S`. Drives the auto-project
    /// when this variant is active.
    pub part: String,
    /// `true` marks this entry as the default variant. Exactly
    /// one entry must set this when the list has more than one.
    /// A single-entry list may omit the flag — the sole entry
    /// is implicitly the default.
    #[serde(default)]
    pub default: bool,
    /// Workspace-relative globs (e.g. `"hdl/ethernet-vpk120.vhd"`
    /// or `"hdl/board-vpk120/**/*.vhd"`) matching files that
    /// ONLY compile when this variant is active. Files that
    /// don't match any variant's exclusive set are always shared.
    #[serde(default)]
    pub exclusive: Vec<String>,
    /// Per-variant top-entity name. Overrides
    /// [`WorkspaceInfo::top`] when this variant is active.
    /// The typical multi-variant shape is
    /// `top_<variant-name>` — one wrapper per board.
    #[serde(default)]
    pub top: Option<String>,
}

/// Errors surfaced when validating or selecting a variant from a
/// workspace's `[[variants]]` list. Same shape as
/// [`TargetSelectError`] with variant-flavored messages.
#[derive(Debug, thiserror::Error)]
pub enum VariantSelectError {
    #[error(
        "workspace declares both `[[target-parts]]` and \
         `[[workspace.variants]]` — they're mutually exclusive; \
         variants own their parts inline, so remove `[[target-parts]]`"
    )]
    BothPartsAndVariants,
    #[error(
        "workspace has {count} `[[workspace.variants]]` entries but \
         none are marked `default = true`; add `default = true` to \
         exactly one entry"
    )]
    NoDefault { count: usize },
    #[error(
        "workspace has multiple `[[workspace.variants]]` entries \
         marked `default = true` ({defaults:?}); only one may be default"
    )]
    MultipleDefaults { defaults: Vec<String> },
    #[error("no `[[workspace.variants]]` entry named `{query}`")]
    NoMatch { query: String },
    #[error(
        "duplicate variant name `{name}` in \
         `[[workspace.variants]]` — variant names must be unique"
    )]
    DuplicateName { name: String },
}

impl WorkspaceInfo {
    /// Return the default target part, if any. Empty list yields
    /// `Ok(None)`. Single-entry list yields that entry as the
    /// implicit default (regardless of the `default` flag).
    /// Multi-entry list requires exactly one `default = true`.
    pub fn default_target_part(
        &self,
    ) -> std::result::Result<Option<&str>, TargetSelectError> {
        match self.target_parts.len() {
            0 => Ok(None),
            1 => Ok(Some(self.target_parts[0].part.as_str())),
            _ => {
                let defaults: Vec<&TargetPart> =
                    self.target_parts.iter().filter(|p| p.default).collect();
                match defaults.len() {
                    1 => Ok(Some(defaults[0].part.as_str())),
                    0 => Err(TargetSelectError::NoDefault {
                        count: self.target_parts.len(),
                    }),
                    _ => Err(TargetSelectError::MultipleDefaults {
                        defaults: defaults
                            .iter()
                            .map(|p| p.part.clone())
                            .collect(),
                    }),
                }
            }
        }
    }

    /// Resolve a CLI `--part <query>` selector against the
    /// workspace's target parts. `None` returns the default (via
    /// [`default_target_part`](Self::default_target_part)). `Some`
    /// matches by exact part ID first; failing that, by unique
    /// substring. Multiple substring matches, or zero matches,
    /// error out.
    pub fn select_target_part(
        &self,
        query: Option<&str>,
    ) -> std::result::Result<Option<&str>, TargetSelectError> {
        let Some(q) = query else {
            return self.default_target_part();
        };
        if let Some(exact) = self.target_parts.iter().find(|p| p.part == q) {
            return Ok(Some(exact.part.as_str()));
        }
        let matches: Vec<&TargetPart> = self
            .target_parts
            .iter()
            .filter(|p| p.part.contains(q))
            .collect();
        match matches.len() {
            1 => Ok(Some(matches[0].part.as_str())),
            0 => Err(TargetSelectError::NoMatch {
                query: q.to_string(),
            }),
            _ => Err(TargetSelectError::Ambiguous {
                query: q.to_string(),
                matches: matches.iter().map(|p| p.part.clone()).collect(),
            }),
        }
    }

    /// Return the default variant, if any. Empty list yields
    /// `Ok(None)` (workspaces without variants). Single-entry
    /// list yields that entry as the implicit default
    /// (regardless of the `default` flag). Multi-entry list
    /// requires exactly one `default = true`.
    pub fn default_variant(
        &self,
    ) -> std::result::Result<Option<&Variant>, VariantSelectError> {
        match self.variants.len() {
            0 => Ok(None),
            1 => Ok(Some(&self.variants[0])),
            _ => {
                let defaults: Vec<&Variant> =
                    self.variants.iter().filter(|v| v.default).collect();
                match defaults.len() {
                    1 => Ok(Some(defaults[0])),
                    0 => Err(VariantSelectError::NoDefault {
                        count: self.variants.len(),
                    }),
                    _ => Err(VariantSelectError::MultipleDefaults {
                        defaults: defaults
                            .iter()
                            .map(|v| v.name.clone())
                            .collect(),
                    }),
                }
            }
        }
    }

    /// Resolve a CLI `--variant <name>` selector against the
    /// workspace's variants. `None` returns the default (via
    /// [`default_variant`](Self::default_variant)). `Some`
    /// matches by exact name only — unlike `--part`, no
    /// substring fallback: variant names are short enough that
    /// substring matching would be more confusing than useful.
    pub fn select_variant(
        &self,
        query: Option<&str>,
    ) -> std::result::Result<Option<&Variant>, VariantSelectError> {
        let Some(q) = query else {
            return self.default_variant();
        };
        self.variants
            .iter()
            .find(|v| v.name == q)
            .map(Some)
            .ok_or_else(|| VariantSelectError::NoMatch {
                query: q.to_string(),
            })
    }

    /// Resolve the top-entity name for the given active variant.
    /// Precedence: variant's `top` (when a variant is active and
    /// sets one) wins over the workspace-level `top`. Returns
    /// `None` when neither is configured — callers (`vw::synth`
    /// via the `top` RPC + `vw::top` proc) then decide whether to
    /// error or fall back to other sources like the fileset TOP
    /// property.
    ///
    /// `active_variant`: the variant name the caller resolved
    /// (typically via `select_variant`). Pass `None` for
    /// no-variant workspaces or when the caller hasn't picked
    /// one yet.
    pub fn resolve_top(&self, active_variant: Option<&str>) -> Option<String> {
        if let Some(vname) = active_variant {
            if let Some(v) = self.variants.iter().find(|v| v.name == vname) {
                if let Some(t) = &v.top {
                    return Some(t.clone());
                }
            }
        }
        self.top.clone()
    }
}

/// Library-scope target metadata. Populated by `vw ip generate`
/// so downstream `vw check` can verify a consumer's target-part
/// is supported by every transitive dep without ever touching
/// Vivado at check time.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TargetsConfig {
    /// Blessed patterns — `<xilinx:family>` entries whose
    /// `lifeCycle` is `Production`, `Beta`, or `Pre-Production`.
    /// A target-part that matches any of these is a clean pass;
    /// the check emits no diagnostic for that dep.
    ///
    /// Every string is in the form `<family-name>{<part-name-regex>}`
    /// — see [`parse_target_pattern`] for the parse rules. Bare
    /// family names are rejected; `vw ip generate` normalizes them
    /// before writing.
    #[serde(default)]
    pub supported: Vec<String>,
    /// Explicitly-unsupported patterns — `<xilinx:family>` entries
    /// with `lifeCycle="Not-Supported"`. Xilinx has attested that
    /// the IP is NOT usable on parts matching these patterns; if
    /// the target-part matches one, `vw check` fires an ERROR.
    ///
    /// Note: an entire component.xml with only Not-Supported
    /// entries (e.g. an experimental IP still in incubation)
    /// leaves `supported = []` and populates only this list. In
    /// that case the check treats a NON-match here as "unblessed
    /// but not forbidden" — a warning, not an error.
    #[serde(default, rename = "not-supported")]
    pub not_supported: Vec<String>,
}

/// A parsed `family{regex}` target pattern. The regex is
/// pre-compiled at parse time so hot paths (validator, LSP
/// diagnostics) don't re-compile per-check.
#[derive(Debug, Clone)]
pub struct TargetPattern {
    /// The family word out front — `versal`, `artix7`, etc. Kept
    /// verbatim for diagnostics: "clk-wizard supports the versal
    /// family; your target `xcvm3358…` isn't in that family."
    pub family: String,
    /// The compiled regex from inside the braces. Applied against
    /// the consumer's target-part string; a match means the
    /// library supports the target.
    pub regex: regex::Regex,
    /// Original source text (`versal{xcvm3(.*)}`) — used to
    /// point diagnostics back at the exact vw.toml line.
    pub raw: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetParseError {
    #[error("target pattern `{raw}` is missing the `{{regex}}` part")]
    MissingBraces { raw: String },
    #[error("target pattern `{raw}`: {source}")]
    BadRegex {
        raw: String,
        #[source]
        source: regex::Error,
    },
}

/// Parse one entry of `[targets].supported` — the form
/// `<family>{<regex>}`. Bare-family entries (no braces) are
/// rejected with [`TargetParseError::MissingBraces`]; the
/// generator's job is to normalize them into brace form before
/// they reach downstream consumers.
/// Snapshot of per-dep target-support metadata used by the
/// project-vs-dep compatibility check. Populated from each dep's
/// `[targets] supported` list at the entry workspace's transitive
/// walk time; used later by [`check_target_compatibility`] to
/// verify a given target-part is supported.
///
/// Deps with no `[targets]` at all are `Vec::new()` — the check
/// treats an empty list as "universal / no constraint," so
/// non-IP libraries (@vw, @test) don't need to declare anything.
///
/// Parse errors during pattern compile are turned into
/// `(dep_name, error)` entries in the second field so callers
/// can surface them as diagnostics without dropping the whole
/// dep's info.
#[derive(Debug, Default)]
pub struct DepTargets {
    /// Blessed (`Production`/`Beta`/`Pre-Production`) patterns
    /// per dep. A target-part matching any of these clears the
    /// check clean.
    pub per_dep: HashMap<String, Vec<TargetPattern>>,
    /// Explicitly `Not-Supported` patterns per dep. A target-part
    /// matching any of these is an ERROR — Xilinx has attested
    /// the IP is not usable there.
    pub per_dep_not_supported: HashMap<String, Vec<TargetPattern>>,
    pub errors: Vec<(String, TargetParseError)>,
}

/// Walk the entry workspace's transitive deps and collect each
/// dep's `[targets].supported` patterns. Returns a
/// [`DepTargets`] snapshot ready to feed into
/// [`check_target_compatibility`].
///
/// Skips deps whose `vw.toml` doesn't load — they contribute
/// nothing (treated as universal). This matches the "empty =
/// universal" policy the compat check applies.
pub fn collect_dep_targets(entry_workspace_dir: &Utf8Path) -> DepTargets {
    let mut out = DepTargets::default();
    let Ok(paths) = transitive_dep_cache_paths(entry_workspace_dir) else {
        return out;
    };
    for (name, path) in paths {
        let Ok(path_utf8) = Utf8PathBuf::from_path_buf(path) else {
            continue;
        };
        let Ok(cfg) = load_workspace_config(&path_utf8) else {
            continue;
        };
        let Some(targets) = cfg.targets else {
            out.per_dep.insert(name.clone(), Vec::new());
            out.per_dep_not_supported.insert(name, Vec::new());
            continue;
        };
        let mut compiled: Vec<TargetPattern> = Vec::new();
        for raw in &targets.supported {
            match parse_target_pattern(raw) {
                Ok(p) => compiled.push(p),
                Err(e) => out.errors.push((name.clone(), e)),
            }
        }
        let mut compiled_not_supported: Vec<TargetPattern> = Vec::new();
        for raw in &targets.not_supported {
            match parse_target_pattern(raw) {
                Ok(p) => compiled_not_supported.push(p),
                Err(e) => out.errors.push((name.clone(), e)),
            }
        }
        out.per_dep.insert(name.clone(), compiled);
        out.per_dep_not_supported
            .insert(name, compiled_not_supported);
    }
    out
}

/// Severity of a target-compatibility mismatch. The two-tier
/// treatment reflects what `<xilinx:supportedFamilies>` actually
/// means in Vivado's IP catalog: the family list is a lifeCycle-
/// tagged compatibility MATRIX, not a hard availability filter.
/// Vivado exposes IPs even for families that aren't listed. So:
///
/// - `NotSupported` — target-part matched a Not-Supported entry.
///   Xilinx explicitly attests the IP won't work here. Error.
/// - `Unblessed` — target-part matched no entry at all (neither
///   supported nor not-supported). Warning: the IP MAY work but
///   Xilinx hasn't blessed the combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMismatchKind {
    NotSupported,
    Unblessed,
}

/// One target-compatibility observation. Emitted by
/// [`check_target_compatibility`]; the caller uses `kind` to
/// choose diagnostic severity (error vs warning).
#[derive(Debug, Clone)]
pub struct TargetMismatch {
    /// Dep name (e.g. `clk-wizard`).
    pub dep: String,
    /// The target part string that was checked.
    pub target_part: String,
    /// Family cues gathered from the dep's BLESSED patterns —
    /// its `[targets] supported` list. Empty when the dep has no
    /// blessed patterns (e.g. clk-wizard v1.0, which has only
    /// `not-supported` entries). The diagnostic message uses this
    /// to say "clk-wizard blesses parts in the following families;
    /// yours isn't among them."
    pub supported_families: Vec<String>,
    /// Family cues gathered from the dep's BAN LIST — its
    /// `[targets] not-supported` list. Reported separately from
    /// `supported_families` because the semantics differ:
    /// families named in `supported` are blessed for use, families
    /// named ONLY in `not-supported` are ones Xilinx has attested
    /// don't work (at least for specific parts). Reporting them
    /// as "declared families" without qualification is misleading
    /// (see #vw-check clarity).
    pub not_supported_families: Vec<String>,
    /// Whether Xilinx explicitly forbids this combination or
    /// simply hasn't blessed it.
    pub kind: TargetMismatchKind,
}

/// Return each dep's target-compatibility observation against
/// `target_part`. Deps with no `[targets]` at all (`supported`
/// AND `not_supported` both empty) contribute nothing — treated
/// as universal. When `target_part` is `None` (library workspace,
/// no project target) the check is a no-op.
///
/// Decision matrix per dep:
/// - matches a Not-Supported pattern → `NotSupported` (error).
///   Trumps a supported match — an explicit ban wins.
/// - matches a Supported pattern → clean pass, no observation.
/// - matches neither, but the dep has SOME patterns declared →
///   `Unblessed` (warning).
pub fn check_target_compatibility(
    target_part: Option<&str>,
    dep_targets: &DepTargets,
) -> Vec<TargetMismatch> {
    let Some(part) = target_part else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (dep, supported) in &dep_targets.per_dep {
        let not_supported = dep_targets
            .per_dep_not_supported
            .get(dep)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if supported.is_empty() && not_supported.is_empty() {
            continue;
        }
        let sup_families = dedup_families(supported);
        let ns_families = dedup_families(not_supported);
        let hit_not_supported =
            not_supported.iter().any(|p| p.regex.is_match(part));
        if hit_not_supported {
            out.push(TargetMismatch {
                dep: dep.clone(),
                target_part: part.to_string(),
                supported_families: sup_families,
                not_supported_families: ns_families,
                kind: TargetMismatchKind::NotSupported,
            });
            continue;
        }
        let hit_supported = supported.iter().any(|p| p.regex.is_match(part));
        if hit_supported {
            continue;
        }
        out.push(TargetMismatch {
            dep: dep.clone(),
            target_part: part.to_string(),
            supported_families: sup_families,
            not_supported_families: ns_families,
            kind: TargetMismatchKind::Unblessed,
        });
    }
    out
}

fn dedup_families(patterns: &[TargetPattern]) -> Vec<String> {
    let mut families: Vec<String> =
        patterns.iter().map(|p| p.family.clone()).collect();
    families.sort();
    families.dedup();
    families
}

pub fn parse_target_pattern(
    raw: &str,
) -> std::result::Result<TargetPattern, TargetParseError> {
    let Some((family, rest)) = raw.split_once('{') else {
        return Err(TargetParseError::MissingBraces {
            raw: raw.to_string(),
        });
    };
    let Some(regex_src) = rest.strip_suffix('}') else {
        return Err(TargetParseError::MissingBraces {
            raw: raw.to_string(),
        });
    };
    // Anchor at start: `xcvm3(.*)` should match ONLY parts that
    // begin with `xcvm3`, not any string containing `xcvm3` mid-
    // way. End anchor is optional because the source patterns
    // themselves use `(.*)` to slop up the suffix.
    let anchored = format!("^{regex_src}");
    let regex = regex::Regex::new(&anchored).map_err(|e| {
        TargetParseError::BadRegex {
            raw: raw.to_string(),
            source: e,
        }
    })?;
    Ok(TargetPattern {
        family: family.to_string(),
        regex,
        raw: raw.to_string(),
    })
}

/// How a workspace dependency identifies its source. Currently a git
/// repo or a local filesystem path; the natural future addition is a
/// registry-resolved variant (`Registry { name, version }`) once a
/// crates.io-like index exists.
///
/// `#[serde(untagged)]` keeps the `vw.toml` ergonomics that came
/// before: an entry with `repo = "..."` reads as `Git`, an entry with
/// `path = "..."` reads as `Path`. New variants need new
/// non-ambiguous required keys for serde to discriminate cleanly.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum DependencySource {
    Git {
        repo: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        commit: Option<String>,
        #[serde(default)]
        submodules: bool,
    },
    Path {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dependency {
    #[serde(flatten)]
    pub source: DependencySource,
    #[serde(default)]
    pub src: Vec<String>,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub sim_only: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Dependency {
    pub fn is_local(&self) -> bool {
        matches!(self.source, DependencySource::Path { .. })
    }

    /// Git-only accessor: the upstream repo URL.
    pub fn repo(&self) -> Option<&str> {
        match &self.source {
            DependencySource::Git { repo, .. } => Some(repo),
            DependencySource::Path { .. } => None,
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match &self.source {
            DependencySource::Git { branch, .. } => branch.as_deref(),
            DependencySource::Path { .. } => None,
        }
    }

    pub fn commit(&self) -> Option<&str> {
        match &self.source {
            DependencySource::Git { commit, .. } => commit.as_deref(),
            DependencySource::Path { .. } => None,
        }
    }

    pub fn submodules(&self) -> bool {
        match &self.source {
            DependencySource::Git { submodules, .. } => *submodules,
            DependencySource::Path { .. } => false,
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match &self.source {
            DependencySource::Path { path } => Some(path.as_path()),
            DependencySource::Git { .. } => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    /// Ordered, and that is load-bearing rather than tidy.
    ///
    /// This file is serialized back out whenever dependencies are fetched, and
    /// its bytes are part of the synth checkpoint's source fingerprint. A
    /// `HashMap` iterates in an order Rust randomizes per process, so writing
    /// an unchanged lockfile produced different bytes every time — which read
    /// downstream as "the sources changed" and threw away a perfectly good
    /// checkpoint on every fetch.
    ///
    /// That went unnoticed locally, where `~/.vw/deps` is warm and the file is
    /// never rewritten. It showed up the moment builds moved to CI, where
    /// every job starts on an empty worker, re-fetches, and rewrites it.
    pub dependencies: BTreeMap<String, LockedDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedDependency {
    pub repo: String,
    /// The branch `commit` was resolved from, when the declaration named one.
    ///
    /// Without it the lock cannot say what question its commit is the answer
    /// to, so a dependency moved to another branch in `vw.toml` would keep
    /// building the old one. Absent for a commit-pinned dependency, and for
    /// entries written before the field existed — those are taken at face
    /// value, and name their branch the next time the lock is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub commit: String,
    #[serde(default)]
    pub src: Vec<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub sim_only: bool,
    #[serde(default)]
    pub submodules: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct CargoToml {
    package: CargoPackage,
}

#[derive(Deserialize, Debug)]
struct CargoPackage {
    name: String,
}

// ============================================================================
// Mixed-Signal Configuration (per-bench mist.toml)
// ============================================================================

/// Configuration parsed from a per-bench `mist.toml` file.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MistConfig {
    /// Path to Xyce netlist, relative to the bench directory.
    pub netlist: String,
    /// VHDL entity name to co-simulate.
    pub entity: String,
    /// Clock frequency in Hz.
    pub clock: f64,
    /// Number of cycles to prime the pipeline before recording.
    #[serde(default, rename = "prime-cycles")]
    pub prime_cycles: Option<u32>,
    /// Port-to-DAC mappings.
    #[serde(default)]
    pub ports: HashMap<String, PortMapping>,
}

/// Maps a VHDL output port to a Xyce YDAC device.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PortMapping {
    /// Xyce YDAC device name (e.g., "dac_sym_main").
    pub dac: String,
    /// Encoding type: "pam4" or "unsigned".
    pub encoding: String,
}

// ============================================================================
// Credentials
// ============================================================================

/// Credentials for authenticating with git repositories.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// Create new credentials from username and password.
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
}

// ============================================================================
// Authentication Helpers
// ============================================================================

/// Get access token for a given host from the netrc file.
///
/// This function reads the user's .netrc file and looks for credentials
/// for the specified host. For GitHub, it returns the password field
/// which should contain the personal access token.
/// Get access credentials (username, password) for a given host from the netrc file.
pub fn get_access_credentials_from_netrc(
    host: &str,
) -> Result<Option<Credentials>> {
    let home_dir = dirs::home_dir().ok_or_else(|| VwError::FileSystem {
        message: "Could not determine home directory".to_string(),
    })?;

    let netrc_path = home_dir.join(".netrc");
    if !netrc_path.exists() {
        return Ok(None);
    }

    let netrc_content = std::fs::read_to_string(&netrc_path).map_err(|e| {
        VwError::FileSystem {
            message: format!("Failed to read .netrc file: {e}"),
        }
    })?;

    let netrc = netrc::Netrc::parse(netrc_content.as_bytes()).map_err(|e| {
        VwError::FileSystem {
            message: format!("Failed to parse .netrc file: {e:?}"),
        }
    })?;

    // netrc.hosts is a Vec<(String, Machine)>, so we need to iterate
    for (hostname, machine) in &netrc.hosts {
        if hostname == host {
            // Return both login and password if both are present
            if let Some(password) = &machine.password {
                let login = machine.login.clone();
                return Ok(Some(Credentials::new(login, password.clone())));
            }
        }
    }

    Ok(None)
}

/// Look up netrc credentials for a git-repository URL. Returns
/// `None` when the URL has no parseable host, or the host has no
/// entry in `~/.netrc`. Never errors — a missing / malformed
/// netrc is treated as "no credentials," matching the "unauthenticated
/// clone" path.
///
/// Callers: `vw-cli::Commands::Update`, `vw-vivado`'s
/// auto-update RPC handler.
pub fn get_access_credentials_for_repo(repo_url: &str) -> Option<Credentials> {
    let hostname = extract_hostname_from_repo_url(repo_url).ok()?;
    get_access_credentials_from_netrc(&hostname).ok().flatten()
}

/// Scan the workspace's declared git dependencies for the first
/// one that has netrc credentials. Cheap and pragmatic: one set
/// of credentials feeds the whole `update_workspace_with_token`
/// pass (all deps to the same host share the same login), and
/// most workspaces target a single provider (github, gitea, …)
/// so the first match is usually the right one.
///
/// `include_test = true` also scans `[test-dependencies]`,
/// matching the same shape [`vhdl_dependency_sources_with_test`]
/// uses. Returns `None` when no git dep has creds — e.g. when
/// every git URL is a public repo.
pub fn get_access_credentials_for_workspace(
    workspace_dir: &Utf8Path,
    include_test: bool,
) -> Option<Credentials> {
    let cfg = load_workspace_config(workspace_dir).ok()?;
    for dep in cfg
        .dependencies
        .values()
        .chain(cfg.test_dependencies.values().filter(|_| include_test))
    {
        let Some(repo) = dep.repo() else { continue };
        if let Some(creds) = get_access_credentials_for_repo(repo) {
            return Some(creds);
        }
    }
    None
}

/// Get access token for a given host from the netrc file.
///
/// This function reads the user's .netrc file and looks for credentials
/// for the specified host. For GitHub, it returns the password field
/// which should contain the personal access token.
pub fn get_access_token_from_netrc(host: &str) -> Result<Option<String>> {
    if let Some(creds) = get_access_credentials_from_netrc(host)? {
        Ok(Some(creds.password))
    } else {
        Ok(None)
    }
}

/// Extract hostname from a git repository URL.
///
/// Supports both HTTPS and SSH URLs:
/// - https://github.com/user/repo.git -> github.com
/// - git@github.com:user/repo.git -> github.com
pub fn extract_hostname_from_repo_url(repo_url: &str) -> Result<String> {
    if repo_url.starts_with("https://") {
        let url = url::Url::parse(repo_url).map_err(|e| VwError::Config {
            message: format!("Invalid repository URL '{repo_url}': {e}"),
        })?;
        Ok(url.host_str().unwrap_or("").to_string())
    } else if repo_url.starts_with("git@") {
        // Parse SSH format: git@hostname:path
        if let Some(at_pos) = repo_url.find('@') {
            if let Some(colon_pos) = repo_url[at_pos..].find(':') {
                let hostname = &repo_url[at_pos + 1..at_pos + colon_pos];
                return Ok(hostname.to_string());
            }
        }
        Err(VwError::Config {
            message: format!("Invalid SSH repository URL format: {repo_url}"),
        })
    } else {
        Err(VwError::Config {
            message: format!("Unsupported repository URL format: {repo_url}"),
        })
    }
}

// ============================================================================
// Public API - Workspace Management
// ============================================================================

/// Initialize a new workspace with the given name.
pub fn init_workspace(
    workspace_dir: &Utf8Path,
    name: String,
    target_part: Option<String>,
) -> Result<()> {
    let config_path = workspace_dir.join("vw.toml");
    if config_path.exists() {
        return Err(VwError::Config {
            message: format!("vw.toml already exists in {workspace_dir}"),
        });
    }

    let target_parts = target_part
        .map(|part| {
            vec![TargetPart {
                part,
                default: true,
            }]
        })
        .unwrap_or_default();

    let config = WorkspaceConfig {
        workspace: WorkspaceInfo {
            name,
            version: "0.1.0".to_string(),
            target_parts,
            variants: Vec::new(),
            top: Some("top".to_string()),
        },
        dependencies: HashMap::new(),
        test_dependencies: HashMap::new(),
        targets: None,
    };

    save_workspace_config(workspace_dir, &config)?;
    scaffold_top_vhd(workspace_dir)?;
    Ok(())
}

/// Scaffold a minimal `hdl/top.vhd` alongside a fresh `vw.toml` so
/// `vw run` and the analyzer have something to elaborate against
/// out of the box.
fn scaffold_top_vhd(workspace_dir: &Utf8Path) -> Result<()> {
    let hdl_dir = workspace_dir.join("hdl");
    std::fs::create_dir_all(&hdl_dir).map_err(|e| VwError::Config {
        message: format!("failed to create {hdl_dir}: {e}"),
    })?;
    let top_path = hdl_dir.join("top.vhd");
    if top_path.exists() {
        return Ok(());
    }
    let contents = "\
library ieee;
use ieee.std_logic_1164.all;

entity top is
  port (
    clk: in std_logic
  );
end top;
";
    std::fs::write(&top_path, contents).map_err(|e| VwError::Config {
        message: format!("failed to write {top_path}: {e}"),
    })?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub dependencies: Vec<DependencyUpdateInfo>,
}

#[derive(Debug, Clone)]
pub struct DependencyUpdateInfo {
    pub name: String,
    pub commit: String,
    pub was_cached: bool,
}

/// Update workspace dependencies by downloading them and generating configuration files.
pub async fn update_workspace(
    workspace_dir: &Utf8Path,
) -> Result<UpdateResult> {
    update_workspace_with_token(workspace_dir, None).await
}

/// Update workspace dependencies with optional credentials for private repositories.
///
/// # Arguments
/// * `workspace_dir` - Path to the workspace directory
/// * `credentials` - Optional credentials for authentication
pub async fn update_workspace_with_token(
    workspace_dir: &Utf8Path,
    credentials: Option<Credentials>,
) -> Result<UpdateResult> {
    fetch(workspace_dir, credentials, Resolution::Fresh).await
}

/// Install exactly what `vw.lock` names, fetching whatever is missing.
///
/// What every implicit fetch should do: a build starting on a machine with a
/// cold dependency cache must end up with the same dependencies as one on a
/// warm machine, or the build is not reproducible and its checkpoints are
/// worthless. Only `vw update` moves a pin.
///
/// A dependency the lockfile does not mention still resolves — that is a
/// dependency somebody just added, and there is nothing to honour.
pub async fn install_locked_dependencies(
    workspace_dir: &Utf8Path,
    credentials: Option<Credentials>,
) -> Result<UpdateResult> {
    fetch(workspace_dir, credentials, Resolution::Locked).await
}

async fn fetch(
    workspace_dir: &Utf8Path,
    credentials: Option<Credentials>,
    resolution: Resolution,
) -> Result<UpdateResult> {
    // Resolve and fetch the WHOLE transitive dependency graph. Cargo
    // model: the entry's `vw.lock` pins every dep — direct AND
    // deps-of-deps — so a transitive import like `src @vivado-cmd` from
    // inside `@clk-wizard` resolves without the consumer redeclaring
    // it. Missing git deps are downloaded as the graph is built.
    let creds = credentials
        .as_ref()
        .map(|c| (c.username.as_str(), c.password.as_str()));
    let graph =
        build_dependency_graph(workspace_dir, true, creds, resolution).await?;

    let mut lock_file = LockFile {
        dependencies: BTreeMap::new(),
    };
    let mut update_info = Vec::new();
    for idx in graph.node_indices() {
        let node = &graph[idx];
        let Some(name) = node.name.clone() else {
            continue; // the entry workspace itself — not a dependency
        };
        match &node.locked {
            // Git dep: record its pin in the entry lock.
            Some(locked) => {
                update_info.push(DependencyUpdateInfo {
                    name: name.clone(),
                    commit: locked.commit.clone(),
                    was_cached: node.was_cached,
                });
                lock_file.dependencies.insert(name, locked.clone());
            }
            // Path dep: no commit to pin, so no lock entry.
            None => {
                update_info.push(DependencyUpdateInfo {
                    name,
                    commit: "local".into(),
                    was_cached: true,
                });
            }
        }
    }

    write_lock_file(workspace_dir, &lock_file)?;
    warn_stale_vhdl_ls_toml(workspace_dir);

    Ok(UpdateResult {
        dependencies: update_info,
    })
}

/// One workspace in the dependency graph built by
/// [`build_dependency_graph`]: the entry, a git dep, or a path dep.
#[derive(Debug, Clone)]
struct DepGraphNode {
    /// The dependency name this node was reached as; `None` for the
    /// entry workspace (which nothing depends on).
    name: Option<String>,
    /// On-disk root — a git dep's cache dir, a path dep's source tree,
    /// or the entry's own directory.
    root: Utf8PathBuf,
    /// Lock entry to record for a git dependency; `None` for the entry
    /// and for path deps (no commit to pin).
    locked: Option<LockedDependency>,
    /// For a git dep: whether its cache dir was already present rather
    /// than freshly downloaded. Always `true` for non-git nodes.
    was_cached: bool,
}

/// Build the transitive dependency graph rooted at `entry`, downloading
/// any missing git dependencies into the per-user cache as it walks.
///
/// petgraph gives cycle-safe traversal: a dev-dependency cycle (e.g.
/// `vw` test-depends on `testlib`, which depends back on `vw`) collapses
/// to a single shared node visited once, so the walk terminates. It
/// also matches the Cargo resolution model the import resolver already
/// assumes — the entry workspace pins the whole graph: the first time a
/// dep name is seen (starting from the entry) fixes its node; a later
/// occurrence of the same name only adds an edge, never a re-fetch.
///
/// Only the entry's `[test-dependencies]` are followed (when
/// `include_test`); a transitive dep's dev-deps stay private to it,
/// mirroring [`transitive_dep_cache_paths_with_test`].
/// Whether a fetch may move a dependency to a commit the lockfile does not
/// name.
///
/// A dependency written as `branch = "main"` has no fixed commit in `vw.toml`;
/// the commit it resolved to lives in `vw.lock`. Asking the remote what the
/// branch points at *now* is therefore an update, and doing it implicitly
/// means a dependency that gained a commit five minutes ago silently changes
/// what a build is made of — which is what a lockfile exists to stop.
///
/// It matters most where it is least visible. Every CI job starts on a clean
/// machine and fetches, so a push to a dependency partway through a pipeline
/// would re-resolve, rewrite `vw.lock`, change the synth fingerprint, and
/// throw away checkpoints from jobs that had already finished.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// Ask each remote where its branch points now. `vw update` only.
    Fresh,
    /// Take what `vw.lock` already pins, and resolve only what it does not
    /// answer — a dependency just added, or one whose declaration has moved
    /// out from under its entry (see [`lock_entry_matches`]).
    Locked,
}

/// Whether a lock entry is still an answer to what `vw.toml` asks for.
///
/// A pin only means something as the answer to a specific question. Repoint a
/// dependency at another repository or another branch and the commit recorded
/// against its name describes something nobody asked for.
///
/// Both the gate that decides whether to fetch at all
/// ([`dependencies_present`], [`workspace_has_unlocked_git_deps`]) and the
/// fetch that decides whether to re-resolve go through here, so they cannot
/// reach different conclusions — a gate that says "nothing to do" about an
/// entry the fetch would have replaced is how an edit to `vw.toml` goes
/// silently unbuilt.
///
/// Not covered: `src`, `exclude`, `recursive`. Those change which files a
/// dependency contributes, not which commit it is at, and the cache is keyed
/// by commit alone — so agreeing here would let the lock claim a file set the
/// tree on disk does not have.
fn lock_entry_matches(dep: &Dependency, locked: &LockedDependency) -> bool {
    let DependencySource::Git {
        repo,
        branch,
        commit,
        ..
    } = &dep.source
    else {
        // A path dep has no commit to pin, so any entry under its name is
        // left over from before the switch.
        return false;
    };
    if *repo != locked.repo {
        return false;
    }
    match (branch, commit) {
        // Pinned by hand: the manifest is the more specific statement, and an
        // entry that disagrees with it is stale however it got that way.
        (_, Some(commit)) => *commit == locked.commit,
        // Resolved from a branch: only an entry resolved from the same branch
        // answers it. One that predates the field is taken at face value —
        // rejecting it would re-resolve every dependency in every lockfile
        // written before today, which is the behaviour being fixed.
        (Some(branch), None) => {
            locked.branch.as_deref().is_none_or(|b| b == branch)
        }
        // Says neither. Nothing to be an answer to; let resolution report it.
        (None, None) => false,
    }
}

async fn build_dependency_graph(
    entry: &Utf8Path,
    include_test: bool,
    credentials: Option<(&str, &str)>,
    resolution: Resolution,
) -> Result<DiGraph<DepGraphNode, ()>> {
    // Only consulted under `Resolution::Locked`, and absent on a workspace
    // that has never been updated — in which case everything resolves fresh,
    // which is the only thing it could do.
    let locked = load_lock_file(entry).unwrap_or(LockFile {
        dependencies: BTreeMap::new(),
    });
    let deps_dir = deps_directory()?;
    let mut graph: DiGraph<DepGraphNode, ()> = DiGraph::new();
    // First-seen (entry-wins) node per dep name; also the cycle guard.
    let mut node_by_name: HashMap<String, NodeIndex> = HashMap::new();

    let entry_root = entry
        .canonicalize_utf8()
        .unwrap_or_else(|_| entry.to_path_buf());
    let entry_idx = graph.add_node(DepGraphNode {
        name: None,
        root: entry_root.clone(),
        locked: None,
        was_cached: true,
    });

    // Worklist of (parent node, workspace root, is_entry).
    let mut queue = vec![(entry_idx, entry_root, true)];
    while let Some((parent, ws, is_entry)) = queue.pop() {
        let Ok(config) = load_workspace_config(&ws) else {
            continue;
        };
        // The entry contributes its dev-deps too; transitive deps only
        // propagate their regular `[dependencies]`.
        let mut deps: Vec<(String, Dependency)> =
            config.dependencies.into_iter().collect();
        if is_entry && include_test {
            deps.extend(config.test_dependencies);
        }
        for (name, dep) in deps {
            // Entry-wins: a name already resolved keeps its node — just
            // wire the edge so the graph stays complete — and is never
            // re-fetched or re-queued (this is also the cycle guard).
            if let Some(&existing) = node_by_name.get(&name) {
                graph.update_edge(parent, existing, ());
                continue;
            }
            let node = match &dep.source {
                DependencySource::Git {
                    repo,
                    branch,
                    commit,
                    submodules,
                } => {
                    // The lockfile wins when it still answers what the
                    // manifest asks, unless the caller explicitly asked for a
                    // fresh resolve.
                    let pinned = (resolution == Resolution::Locked)
                        .then(|| locked.dependencies.get(&name))
                        .flatten()
                        .filter(|entry| lock_entry_matches(&dep, entry))
                        .map(|entry| entry.commit.clone());

                    let sha =
                        match pinned {
                            Some(sha) => sha,
                            None => resolve_dependency_commit(
                                repo,
                                branch,
                                commit,
                                credentials,
                            )
                            .await
                            .map_err(|e| VwError::Dependency {
                                message: format!(
                                    "Failed to resolve commit for dependency \
                                 '{name}': {e}"
                                ),
                            })?,
                        };
                    let root = deps_dir.join(format!("{name}-{sha}"));
                    // A dir left by a PARTIAL/failed prior download
                    // (created but empty) must not count as cached.
                    let was_cached = root.exists()
                        && fs::read_dir(&root)
                            .map(|mut d| d.next().is_some())
                            .unwrap_or(false);
                    if !was_cached {
                        if root.exists() {
                            let _ = fs::remove_dir_all(&root);
                        }
                        download_dependency(
                            repo,
                            &sha,
                            &dep.src,
                            &root,
                            dep.recursive,
                            &dep.exclude,
                            *submodules,
                            credentials,
                            None,
                        )
                        .await
                        .map_err(|e| {
                            VwError::Dependency {
                                message: format!(
                                "Failed to download dependency '{name}': {e}"
                            ),
                            }
                        })?;
                    }
                    let root =
                        Utf8PathBuf::from_path_buf(root).map_err(|p| {
                            VwError::FileSystem {
                                message: format!(
                                    "dependency cache path is not UTF-8: {}",
                                    p.display()
                                ),
                            }
                        })?;
                    DepGraphNode {
                        name: Some(name.clone()),
                        root,
                        was_cached,
                        locked: Some(LockedDependency {
                            repo: repo.clone(),
                            branch: branch.clone(),
                            commit: sha.clone(),
                            src: dep.src.clone(),
                            path: PathBuf::from(format!("{name}-{sha}")),
                            recursive: dep.recursive,
                            sim_only: dep.sim_only,
                            submodules: *submodules,
                            exclude: dep.exclude.clone(),
                        }),
                    }
                }
                DependencySource::Path { .. } => {
                    let Some(p) = dep.local_path() else {
                        continue;
                    };
                    let root = resolve_local_dep_path(&ws, p);
                    let root =
                        Utf8PathBuf::from_path_buf(root).map_err(|p| {
                            VwError::FileSystem {
                                message: format!(
                                    "path dependency is not UTF-8: {}",
                                    p.display()
                                ),
                            }
                        })?;
                    DepGraphNode {
                        name: Some(name.clone()),
                        root,
                        locked: None,
                        was_cached: true,
                    }
                }
            };
            let root = node.root.clone();
            // Recurse only into deps that are themselves htcl
            // workspaces (a leaf dep is just files).
            let recurse = root.join("vw.toml").exists();
            let idx = graph.add_node(node);
            node_by_name.insert(name, idx);
            graph.add_edge(parent, idx, ());
            if recurse {
                queue.push((idx, root, false));
            }
        }
    }
    Ok(graph)
}

/// Whether every dependency in this workspace's transitive closure is
/// already materialized. Cheap and fully offline — reads only
/// `vw.toml`, `vw.lock`, and the cache dir, never the network — so it
/// can gate `vw check` the way `cargo check` transparently fetches
/// absent deps instead of face-planting on an unresolved `src @dep`.
///
/// Path deps need no cache, so a workspace with only path deps is
/// always "present". Returns `false` when a declared git dep has no
/// lock entry (added but never `vw update`d) or its cache dir is
/// missing/empty (never fetched, or `vw clear`ed) — the signal that a
/// fetch is needed. Both `[dependencies]` and `[test-dependencies]` are
/// considered, matching what `vw update` materializes.
pub fn dependencies_present(workspace_dir: &Utf8Path) -> bool {
    let Ok(config) = load_workspace_config(workspace_dir) else {
        // Missing/unreadable vw.toml — nothing we can assert is
        // missing; let the check itself surface any real problem.
        return true;
    };
    // (a) Every DECLARED git dep must have a lock entry that still answers
    // its declaration. A dep freshly added to vw.toml but never `vw update`d
    // won't appear in the transitive walk below (which reads the lock), so
    // catch it here — as well as one whose declaration has since moved to
    // another repo, branch, or commit, whose entry is on disk and complete
    // and describes the wrong thing.
    let declared_git: Vec<(&String, &Dependency)> = config
        .dependencies
        .iter()
        .chain(config.test_dependencies.iter())
        .filter(|(_, d)| matches!(d.source, DependencySource::Git { .. }))
        .collect();
    if !declared_git.is_empty() {
        match load_lock_file(workspace_dir) {
            Ok(lock) => {
                let unanswered = declared_git.iter().any(|(name, dep)| {
                    lock.dependencies
                        .get(*name)
                        .is_none_or(|entry| !lock_entry_matches(dep, entry))
                });
                if unanswered {
                    return false;
                }
            }
            Err(_) => return false, // git deps declared, no lock at all
        }
    }
    // (b) Every dep in the transitive closure must be materialized on
    // disk. `transitive_dep_cache_paths_with_test` walks the whole
    // graph (direct + deps-of-deps, via each dep's bundled lock); a
    // `vw clear`ed or partially-fetched cache dir counts as missing.
    let Ok(paths) = transitive_dep_cache_paths_with_test(workspace_dir, true)
    else {
        return true;
    };
    for (_name, path) in paths {
        // Path deps resolve to real source trees (always present); git
        // deps resolve into the cache and may be absent or empty.
        let present = path.exists()
            && fs::read_dir(&path)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        if !present {
            return false;
        }
    }
    true
}

/// One-time migration nudge on `vw update`: if the workspace still
/// has a `vhdl_ls.toml` sitting at its root, print a warning to
/// stderr. vw no longer writes or reads that file — both the sim
/// path and `vw-analyzer` compute their VHDL config in memory from
/// live workspace state.
fn warn_stale_vhdl_ls_toml(workspace_dir: &Utf8Path) {
    let path = workspace_dir.join("vhdl_ls.toml");
    if path.exists() {
        eprintln!(
            "warning: {path} is no longer consumed by vw; the LSP \
             and sim paths render config in memory. Remove the file \
             or ignore it — any user-added libraries there won't be \
             picked up."
        );
    }
}

/// Add a new dependency to the workspace configuration.
#[allow(clippy::too_many_arguments)]
pub async fn add_dependency(
    workspace_dir: &Utf8Path,
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
    recursive: bool,
    sim_only: bool,
) -> Result<()> {
    add_dependency_with_token(
        workspace_dir,
        repo,
        branch,
        commit,
        src,
        name,
        recursive,
        sim_only,
        None,
    )
    .await
}

/// Add a new dependency with optional credentials for private repositories.
///
/// # Arguments
/// * `workspace_dir` - Path to the workspace directory
/// * `repo` - Git repository URL
/// * `branch` - Optional branch name
/// * `commit` - Optional commit hash
/// * `src` - Optional source path within the repository
/// * `name` - Optional dependency name
/// * `recursive` - Whether to recursively include VHDL files
/// * `sim_only` - Whether this dependency is only for simulation (excluded from deps.tcl)
/// * `credentials` - Optional credentials for authentication
#[allow(clippy::too_many_arguments)]
pub async fn add_dependency_with_token(
    workspace_dir: &Utf8Path,
    repo: String,
    branch: Option<String>,
    commit: Option<String>,
    src: Option<String>,
    name: Option<String>,
    recursive: bool,
    sim_only: bool,
    _credentials: Option<Credentials>,
) -> Result<()> {
    let mut config =
        load_workspace_config(workspace_dir).unwrap_or_else(|_| {
            WorkspaceConfig {
                workspace: WorkspaceInfo {
                    name: "workspace".to_string(),
                    version: "0.1.0".to_string(),
                    target_parts: Vec::new(),
                    variants: Vec::new(),
                    top: None,
                },
                dependencies: HashMap::new(),
                test_dependencies: HashMap::new(),
                targets: None,
            }
        });

    // Validate that either branch or commit is provided
    if branch.is_none() && commit.is_none() {
        return Err(VwError::Config {
            message: "Must specify either --branch or --commit".to_string(),
        });
    }

    let dep_name = name.unwrap_or_else(|| extract_repo_name(&repo));
    let src_paths = vec![src.unwrap_or_else(|| ".".to_string())];

    let dependency = Dependency {
        source: DependencySource::Git {
            repo: repo.clone(),
            branch,
            commit,
            submodules: false,
        },
        src: src_paths,
        recursive,
        sim_only,
        exclude: Vec::new(),
    };

    config.dependencies.insert(dep_name.clone(), dependency);
    save_workspace_config(workspace_dir, &config)?;

    Ok(())
}

/// Remove a dependency from the workspace configuration.
pub fn remove_dependency(workspace_dir: &Utf8Path, name: String) -> Result<()> {
    let mut config = load_workspace_config(workspace_dir)?;

    if config.dependencies.remove(&name).is_some() {
        save_workspace_config(workspace_dir, &config)?;
        Ok(())
    } else {
        Err(VwError::Config {
            message: format!("Dependency '{name}' not found"),
        })
    }
}

/// Clear all cached repositories for the current workspace.
pub fn clear_cache(workspace_dir: &Utf8Path) -> Result<Vec<String>> {
    let config = load_workspace_config(workspace_dir)?;
    let deps_dir = deps_directory()?;

    let mut cleared = Vec::new();

    // Get all dependencies from the current workspace
    for name in config.dependencies.keys() {
        if let Ok(entries) = fs::read_dir(&deps_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if let Some(file_name_str) = file_name.to_str() {
                    if file_name_str.starts_with(&format!("{name}-")) {
                        let dep_path = entry.path();
                        if dep_path.is_dir() {
                            fs::remove_dir_all(&dep_path)
                                .map_err(|e| VwError::FileSystem {
                                    message: format!("Failed to remove cached dependency at {dep_path:?}: {e}")
                                })?;
                            cleared.push(file_name_str.to_string());
                        }
                    }
                }
            }
        }
    }

    Ok(cleared)
}

/// List all dependencies in the workspace (both regular and
/// test-dependencies). Callers that want to render them in
/// separate sections can filter on [`DependencyInfo::is_test`].
pub fn list_dependencies(
    workspace_dir: &Utf8Path,
) -> Result<Vec<DependencyInfo>> {
    let config = load_workspace_config(workspace_dir)?;
    if config.dependencies.is_empty() && config.test_dependencies.is_empty() {
        return Ok(Vec::new());
    }

    // Try to load lock file to get resolved versions
    let lock_file = load_lock_file(workspace_dir).ok();

    let mut deps = Vec::new();
    for (name, dep, is_test) in config
        .dependencies
        .iter()
        .map(|(n, d)| (n, d, false))
        .chain(config.test_dependencies.iter().map(|(n, d)| (n, d, true)))
    {
        let (source_label, version_info) = match &dep.source {
            DependencySource::Path { path } => {
                (path.display().to_string(), VersionInfo::Local)
            }
            DependencySource::Git {
                repo,
                branch,
                commit,
                ..
            } => {
                let from_config =
                    || match (branch.as_deref(), commit.as_deref()) {
                        (Some(b), None) => {
                            VersionInfo::Branch { branch: b.into() }
                        }
                        (None, Some(c)) => {
                            VersionInfo::Commit { commit: c.into() }
                        }
                        _ => VersionInfo::Unknown,
                    };
                let version = match &lock_file {
                    Some(lock) => match lock.dependencies.get(name) {
                        Some(locked_dep) => VersionInfo::Locked {
                            commit: locked_dep.commit.clone(),
                        },
                        None => from_config(),
                    },
                    None => from_config(),
                };
                (repo.clone(), version)
            }
        };

        deps.push(DependencyInfo {
            name: name.clone(),
            source: source_label,
            version: version_info,
            is_test,
        });
    }

    Ok(deps)
}

#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub name: String,
    /// User-facing source description: the repo URL for git deps,
    /// the local path for path deps.
    pub source: String,
    pub version: VersionInfo,
    /// True when this entry came from `[test-dependencies]` rather
    /// than `[dependencies]`. Test-deps only affect `vw test`; other
    /// commands see them but don't act on them.
    pub is_test: bool,
}

#[derive(Debug, Clone)]
pub enum VersionInfo {
    Branch {
        branch: String,
    },
    Commit {
        commit: String,
    },
    Locked {
        commit: String,
    },
    /// Local filesystem dependency — no commit to pin.
    Local,
    Unknown,
}

/// Resolve dependency VHDL files from the lock file.
/// Returns a map of library name to list of paths relative to the
/// per-user dependency cache directory (`$HOME/.vw/deps`), skipping
/// sim-only dependencies. Relative paths keep this output stable across
/// machines and platforms.
pub fn resolve_deps(
    workspace_dir: &Utf8Path,
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let lock_file = load_lock_file(workspace_dir)?;
    let deps_dir = deps_directory()?;
    let mut deps = HashMap::new();

    for (dep_name, locked_dep) in &lock_file.dependencies {
        if locked_dep.sim_only {
            continue;
        }
        let abs_dep_path = resolve_dep_path(&locked_dep.path)?;
        let vhdl_files = find_vhdl_files(
            &abs_dep_path,
            locked_dep.recursive,
            &locked_dep.exclude,
        )?;
        let relative_files = vhdl_files
            .into_iter()
            .map(|f| match f.strip_prefix(&deps_dir) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => f,
            })
            .collect();
        deps.insert(dep_name.clone(), relative_files);
    }

    Ok(deps)
}

// ============================================================================
// Public API - Testbench Management
// ============================================================================

/// List all available testbenches in the workspace.
pub fn list_testbenches(
    bench_dir: &Utf8Path,
    ignore_dirs: &HashSet<String>,
    recurse: bool,
) -> Result<Vec<TestbenchInfo>> {
    let mut entities_cache = HashMap::new();
    list_testbenches_impl(bench_dir, ignore_dirs, recurse, &mut entities_cache)
}

fn list_testbenches_impl(
    bench_dir: &Utf8Path,
    ignore_dirs: &HashSet<String>,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<Vec<TestbenchInfo>> {
    let mut testbenches = Vec::new();

    for entry in fs::read_dir(bench_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read bench directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "vhd" || extension == "vhdl" {
                    let entities = get_cached_entities(&path, entities_cache)?;
                    for entity in entities {
                        testbenches.push(TestbenchInfo {
                            name: entity.clone(),
                            path: path.clone(),
                        });
                    }
                }
            }
        } else if recurse {
            let dir_path: Utf8PathBuf =
                path.try_into().map_err(|e| VwError::FileSystem {
                    message: format!("Failed to get dir path: {e}"),
                })?;
            if let Some(file_name) = dir_path.file_name() {
                if !ignore_dirs.contains(file_name) {
                    let mut lower_testbenches = list_testbenches_impl(
                        &dir_path,
                        ignore_dirs,
                        recurse,
                        entities_cache,
                    )?;
                    testbenches.append(&mut lower_testbenches);
                }
            }
        }
    }

    Ok(testbenches)
}

#[derive(Debug, Clone)]
pub struct TestbenchInfo {
    pub name: String,
    pub path: PathBuf,
}

/// Recursively enumerate every `*.htcl` file under
/// `<workspace_dir>/test/`. Skips hidden directories (`.git`,
/// `.vscode`, etc.) and any directory literally named `target`
/// (the vw-standard build-output location, matches the shape used
/// by `vw::make_wrapper`). Returns file paths sorted
/// lexicographically for deterministic test order.
///
/// Returns an empty vec when `<workspace_dir>/test/` doesn't exist
/// — matches `vw test`'s expected "no tests found" UX rather than
/// erroring.
/// One VHDL source shipped by a dep: the target VHDL library name
/// it should compile into, plus the absolute on-disk path.
///
/// Library name is derived from the dep name with hyphens replaced
/// by underscores — the same rule the `vhdl_ls.toml` generator
/// already uses, matching NVC/Vivado convention. This will move to
/// a dep-controlled override later; for now the rule is uniform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhdlDepSource {
    pub library: String,
    pub path: PathBuf,
}

/// Enumerate every VHDL source published by any transitive dep
/// of `workspace_dir`. Files are absolute paths pointing at the
/// dep's materialized cache (or its local path, for
/// `path = "..."` deps).
///
/// A dep only contributes when it explicitly declares a `src`
/// field in its vw.toml entry. Path deps for htcl-only libraries
/// (e.g. `[dependencies.vw] path = "..."`) omit `src` and
/// therefore publish no VHDL — otherwise a recursive scan would
/// happily pick up the library's OWN `target/`, `test/`, and
/// other non-shipped subtrees.
///
/// For each `src` entry we honor the dep's `recursive` /
/// `exclude` config, matching what `vw update` uses when it
/// populates the cache from a git dep. For path deps the same
/// filtering runs at read time (no copy step) so both dep kinds
/// present identical surfaces.
///
/// Depends on the deps being present on disk — call after
/// `vw update`. Missing dep caches are silently skipped so a
/// half-updated workspace still gives a partial result rather
/// than erroring mid-enumeration.
///
/// Sort order: library name, then path within library. Callers
/// that need topological order do their own downstream analysis.
pub fn vhdl_dependency_sources(
    workspace_dir: &Utf8Path,
) -> Result<Vec<VhdlDepSource>> {
    vhdl_dependency_sources_ext(workspace_dir, false, false)
}

/// Detect whether the workspace has any git dep declared in vw.toml that
/// vw.lock does not answer — missing outright (the user hasn't yet run
/// `vw update`, or the lockfile has been truncated / wiped), or pinned
/// against a declaration that has since changed. Cheap check (loads the
/// config + lockfile once, no network); consumers use it to decide whether to
/// auto-invoke [`install_locked_dependencies`].
///
/// `include_test` mirrors the same flag on the enumeration side
/// so a test-deps-only unlocked entry is caught when the caller
/// intends to enumerate test-deps too.
pub fn workspace_has_unlocked_git_deps(
    workspace_dir: &Utf8Path,
    include_test: bool,
) -> Result<bool> {
    let cfg = load_workspace_config(workspace_dir)?;
    let git_deps: Vec<(&String, &Dependency)> = cfg
        .dependencies
        .iter()
        .chain(cfg.test_dependencies.iter().filter(|_| include_test))
        .filter(|(_, dep)| matches!(dep.source, DependencySource::Git { .. }))
        .collect();
    if git_deps.is_empty() {
        return Ok(false);
    }
    match load_lock_file(workspace_dir) {
        Ok(lock) => Ok(git_deps.iter().any(|(name, dep)| {
            lock.dependencies
                .get(*name)
                .is_none_or(|entry| !lock_entry_matches(dep, entry))
        })),
        // No lockfile at all: every git dep is unlocked.
        Err(_) => Ok(true),
    }
}

/// Same as [`vhdl_dependency_sources`] but optionally includes
/// the entry workspace's `[test-dependencies]`. Cargo-parity
/// semantic for `dev-dependencies`: test-deps are private to the
/// workspace that declares them. Recursed-into workspaces are
/// walked with `include_test = false` so a dep's own test-deps
/// aren't pulled into your consumer.
///
/// The test runner uses `include_test = true` so htcl tests
/// under `test/` can enumerate `[test-dependencies]` VHDL
/// alongside regular deps. Production `vw run` uses `false` so
/// test-only VHDL doesn't sneak into a synth flow.
pub fn vhdl_dependency_sources_with_test(
    workspace_dir: &Utf8Path,
    include_test: bool,
) -> Result<Vec<VhdlDepSource>> {
    vhdl_dependency_sources_ext(workspace_dir, include_test, false)
}

/// Full-shape enumeration primitive. `exclude_sim_only = true`
/// drops every dep whose vw.toml sets `sim_only = true` (unisim,
/// xpm, etc.) — used by synth flows that need the design-only
/// surface. `include_test` mirrors the same knob on the sibling
/// wrapper.
pub fn vhdl_dependency_sources_ext(
    workspace_dir: &Utf8Path,
    include_test: bool,
    exclude_sim_only: bool,
) -> Result<Vec<VhdlDepSource>> {
    // Walk the entry workspace's deps + transitive deps. We need
    // each dep's Dependency config (for src/recursive/exclude),
    // so `transitive_dep_cache_paths` (name → path only) isn't
    // enough — walk the graph ourselves.
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    let mut queue: Vec<(Utf8PathBuf, bool)> =
        vec![(workspace_dir.to_path_buf(), include_test)];
    while let Some((ws, want_test)) = queue.pop() {
        if !visited.insert(ws.as_std_path().to_path_buf()) {
            continue;
        }
        let Ok(cfg) = load_workspace_config(&ws) else {
            continue;
        };
        // Combine regular + test deps for this level.
        let deps: Vec<(String, Dependency)> = cfg
            .dependencies
            .into_iter()
            .chain(cfg.test_dependencies.into_iter().filter(|_| want_test))
            .collect();
        for (name, dep) in deps {
            // Skip sim-only deps when the caller wants a
            // synth-clean surface. Filter happens BEFORE the
            // transitive-workspace push, so if a dep is a
            // workspace whose only purpose is sim glue, we
            // don't descend and pick up its transitive deps
            // either.
            if exclude_sim_only && dep.sim_only {
                continue;
            }
            let Some(dep_path) =
                resolve_dep_source_path(workspace_dir, &ws, &name, &dep)?
            else {
                continue;
            };
            // If the dep is itself a workspace, follow it too so
            // we pick up its own deps' VHDL. Recursed workspaces
            // never see their own test-deps — Cargo parity.
            if dep_path.join("vw.toml").is_file() {
                if let Ok(u) = Utf8PathBuf::from_path_buf(dep_path.clone()) {
                    queue.push((u, false));
                }
            }
            let files = enumerate_dep_vhdl_files(&dep_path, &dep)?;
            if files.is_empty() {
                continue;
            }
            let library = library_name_for_dep(&name);
            for path in files {
                out.push(VhdlDepSource {
                    library: library.clone(),
                    path,
                });
            }
        }
    }
    out.sort_by(|a, b| a.library.cmp(&b.library).then(a.path.cmp(&b.path)));
    Ok(out)
}

/// Resolve one dep's on-disk root. Local (`path = "..."`) deps
/// point at the user's tree; git deps resolve through the
/// workspace's lockfile. Returns `Ok(None)` when the dep is a
/// git dep the caller hasn't `vw update`-d yet — the enumeration
/// treats that as "no VHDL" rather than erroring, since a half-
/// updated workspace shouldn't gate every downstream call.
fn resolve_dep_source_path(
    entry_workspace_dir: &Utf8Path,
    parent_workspace_dir: &Utf8Path,
    name: &str,
    dep: &Dependency,
) -> Result<Option<PathBuf>> {
    if let Some(p) = dep.local_path() {
        // Relative path deps resolve against the workspace that
        // DECLARES them (same rule Cargo uses). Absolute paths
        // pass through unchanged. This lets a workspace ship a
        // portable path-dep like `path = "test/fixtures/foo"`
        // without hard-coding a machine-specific prefix.
        if p.is_absolute() {
            return Ok(Some(p.to_path_buf()));
        }
        return Ok(Some(parent_workspace_dir.as_std_path().join(p)));
    }
    // Git dep — look up the resolved cache path in the lockfile
    // of the ENTRY workspace (only the entry has a meaningful
    // lockfile; transitive walks reuse the entry's pins for
    // Cargo-parity).
    let Ok(lock) = load_lock_file(entry_workspace_dir) else {
        return Ok(None);
    };
    let Some(locked) = lock.dependencies.get(name) else {
        return Ok(None);
    };
    Ok(Some(resolve_dep_path(&locked.path)?))
}

/// Enumerate every VHDL file a dep publishes, honoring its
/// declared `src` / `recursive` / `exclude` filters. Empty when
/// `dep.src` is empty (htcl-only dep, publishes no VHDL).
///
/// Two dep kinds diverge here:
/// - **Git deps** cache into `~/.vw/deps/<name>-<sha>/` with the
///   `src` prefix STRIPPED at copy time. `copy_vhdl_files_glob`
///   flattens away the source repo's directory structure. So
///   applying `src` here as a subdirectory path finds nothing —
///   we just walk the whole cache dir recursively (its contents
///   were already filtered by the update step).
/// - **Path deps** point at an unmodified checkout of the dep's
///   tree, so `src` still maps to a real subdirectory.
fn enumerate_dep_vhdl_files(
    dep_root: &Path,
    dep: &Dependency,
) -> Result<Vec<PathBuf>> {
    if dep.src.is_empty() {
        return Ok(Vec::new());
    }
    // Git-dep cache: flattened at copy time — walk everything and
    // apply the exclude patterns (which are structure-relative, so
    // they still work as-is over the flat tree).
    if !dep.is_local() {
        let mut files =
            find_vhdl_files(dep_root, /*recursive=*/ true, &[])?;
        if !dep.exclude.is_empty() {
            let exclude_patterns: Vec<glob::Pattern> = dep
                .exclude
                .iter()
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect();
            files.retain(|f| {
                let rel = f.strip_prefix(dep_root).unwrap_or(f);
                let rel_str = rel.to_string_lossy();
                !exclude_patterns.iter().any(|p| p.matches(&rel_str))
            });
        }
        files.sort();
        files.dedup();
        return Ok(files);
    }
    // Path dep: honor src patterns against the real tree.
    let exclude_patterns: Vec<glob::Pattern> = dep
        .exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    let mut out = Vec::new();
    for src_pattern in &dep.src {
        let src_path = dep_root.join(src_pattern);
        let candidates = if src_path.is_dir() {
            let base =
                src_path.to_str().ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in dep src path".to_string(),
                })?;
            let mut cands = Vec::new();
            let patterns = if dep.recursive {
                vec![format!("{base}/**/*.vhd"), format!("{base}/**/*.vhdl")]
            } else {
                vec![format!("{base}/*.vhd"), format!("{base}/*.vhdl")]
            };
            for p in patterns {
                let entries =
                    glob::glob(&p).map_err(|e| VwError::FileSystem {
                        message: format!("Invalid glob pattern '{p}': {e}"),
                    })?;
                for entry in entries.flatten() {
                    cands.push((src_path.clone(), entry));
                }
            }
            cands
        } else if src_path.is_file() {
            vec![(
                src_path
                    .parent()
                    .ok_or_else(|| VwError::FileSystem {
                        message: "dep src file has no parent".to_string(),
                    })?
                    .to_path_buf(),
                src_path.clone(),
            )]
        } else {
            // Glob pattern rooted at the dep root — exclude
            // patterns match relative to the dep root here.
            let base = dep_root.to_path_buf();
            let pat = src_path
                .to_str()
                .ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in dep src glob".to_string(),
                })?
                .to_string();
            let entries =
                glob::glob(&pat).map_err(|e| VwError::FileSystem {
                    message: format!("Invalid glob pattern '{pat}': {e}"),
                })?;
            entries.flatten().map(|e| (base.clone(), e)).collect()
        };
        for (strip_prefix, path) in candidates {
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if ext != Some("vhd") && ext != Some("vhdl") {
                continue;
            }
            if !exclude_patterns.is_empty() {
                let rel = path.strip_prefix(&strip_prefix).unwrap_or(&path);
                let rel_str = rel.to_string_lossy();
                if exclude_patterns.iter().any(|p| p.matches(&rel_str)) {
                    continue;
                }
            }
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Enumerate every VHDL source under `<workspace_dir>/hdl/`
/// (recursively). These are the workspace's own design sources,
/// as distinct from IP wrappers (which live under `target/ip/` and
/// are enumerated by a separate helper) and testbenches (which
/// live under `bench/`).
///
/// Returns an empty vec when `<workspace_dir>/hdl/` doesn't exist
/// — a freshly-scaffolded workspace hasn't checked anything in
/// yet, and that's not an error.
pub fn vhdl_design_sources(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    vhdl_design_sources_for_variant(workspace_dir, None)
}

/// Every VHDL source under `<workspace>/hdl/**`, with no variant filtering at
/// all — the union of what every variant could build.
///
/// [`vhdl_design_sources`] answers a different question. It gives the surface
/// of one build, so under no active variant it drops every variant-owned file;
/// that is right for the analyzer and wrong for anything asking "could a change
/// to this file matter to somebody". A workspace's top level is usually
/// variant-owned, and a caller that wanted to know whether the design changed
/// would be told no.
///
/// Empty vec when `<workspace_dir>/hdl/` does not exist.
pub fn vhdl_design_sources_all_variants(
    workspace_dir: &Utf8Path,
) -> Result<Vec<PathBuf>> {
    let hdl_dir = workspace_dir.join("hdl");
    if !hdl_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files =
        find_vhdl_files(hdl_dir.as_std_path(), /*recursive=*/ true, &[])?;
    files.sort();
    Ok(files)
}

/// Enumerate every VHDL source under `<workspace>/hdl/**` and
/// filter by the `exclusive` file lists on the workspace's
/// variants:
///
/// - A file is "variant-owned" if it matches any `exclusive`
///   glob on ANY variant (across the whole list).
/// - Variant-owned files contribute ONLY when their owning
///   variant is the active one.
/// - Files not in any `exclusive` set are shared and always
///   contribute.
///
/// `active_variant` is the CURRENTLY active variant's name; when
/// `None`, no variant is active (used by the analyzer's default
/// path and by tools that don't yet flow a variant selection
/// through). Under `None`, variant-owned files are still
/// excluded — otherwise a variant-mode workspace would drag
/// every other variant's exclusive sources into the surface.
///
/// Empty vec when `<workspace_dir>/hdl/` doesn't exist.
pub fn vhdl_design_sources_for_variant(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let hdl_dir = workspace_dir.join("hdl");
    if !hdl_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files =
        find_vhdl_files(hdl_dir.as_std_path(), /*recursive=*/ true, &[])?;
    files.sort();
    // Compile each variant's `exclusive` globs relative to the
    // workspace root. Empty variants list → nothing to filter,
    // early-return keeps the common no-variants path cheap.
    let cfg = match load_workspace_config(workspace_dir) {
        Ok(c) => c,
        // No workspace config → can't know about variants, skip filter.
        Err(_) => return Ok(files),
    };
    if cfg.workspace.variants.is_empty() {
        return Ok(files);
    }
    let owner = build_variant_ownership(workspace_dir, &cfg.workspace)?;
    files.retain(|path| {
        match owner.owner_of(path) {
            // Shared file — always keep.
            None => true,
            // Variant-owned file — keep iff active variant owns it.
            Some(name) => Some(name) == active_variant,
        }
    });
    Ok(files)
}

/// Precomputed variant-ownership index. Each entry maps a
/// canonicalized absolute path to the variant that "owns" it
/// (the first variant whose `exclusive` glob matched during the
/// build). Files not present in the map are shared.
struct VariantOwnership {
    /// Absolute path → owning variant name.
    owners: std::collections::HashMap<PathBuf, String>,
}

impl VariantOwnership {
    fn owner_of(&self, path: &Path) -> Option<&str> {
        self.owners.get(path).map(|s| s.as_str())
    }
}

fn build_variant_ownership(
    workspace_dir: &Utf8Path,
    ws: &WorkspaceInfo,
) -> Result<VariantOwnership> {
    let mut owners: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    for variant in &ws.variants {
        for pattern in &variant.exclusive {
            // Absolutize relative-to-workspace patterns so the
            // glob crate walks the right filesystem tree.
            let abs_pattern = workspace_dir.as_std_path().join(pattern);
            let pattern_str =
                abs_pattern.to_str().ok_or_else(|| VwError::FileSystem {
                    message: format!(
                        "variant `{}` exclusive pattern is not valid UTF-8: {}",
                        variant.name,
                        abs_pattern.display(),
                    ),
                })?;
            let entries =
                glob::glob(pattern_str).map_err(|e| VwError::FileSystem {
                    message: format!(
                        "variant `{}` invalid glob `{pattern}`: {e}",
                        variant.name,
                    ),
                })?;
            for entry in entries.flatten() {
                if !entry.is_file() {
                    continue;
                }
                // First-writer wins: if two variants claim the
                // same file exclusively, the first entry in the
                // list owns it. That's a config bug the user
                // should fix; we don't error to keep the surface
                // predictable in the interim.
                owners.entry(entry).or_insert_with(|| variant.name.clone());
            }
        }
    }
    Ok(VariantOwnership { owners })
}

/// Enumerate every Vivado design-constraint file under
/// `<workspace_dir>/constraints/**/*.{xdc,sdc}`. Handles both
/// physical (`.xdc`) and Synopsys-style (`.sdc`) constraints —
/// both are accepted by `read_xdc` in Vivado.
///
/// Returned separately from [`vhdl_design_sources`] because
/// constraints have their own file kind and a different
/// consumption command (`read_xdc` vs. `read_vhdl`).
///
/// Empty vec when `constraints/` doesn't exist yet.
pub fn design_constraints(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    design_constraints_in(workspace_dir, None)
}

/// Enumerate only the `synth`-scoped constraints —
/// `<workspace_dir>/constraints/synth/**/*.{xdc,sdc}`. Used to
/// hand synthesis-only constraints to `read_xdc -used_in
/// synthesis` (or the equivalent set_property USED_IN) so
/// route/place-only constraints don't spuriously apply during
/// synth. Empty vec when the subdir doesn't exist.
pub fn design_synth_constraints(
    workspace_dir: &Utf8Path,
) -> Result<Vec<PathBuf>> {
    design_constraints_in(workspace_dir, Some("synth"))
}

/// Enumerate only the `place`-scoped constraints under
/// `<workspace_dir>/constraints/place/**/*.{xdc,sdc}`. Mirrors
/// [`design_synth_constraints`] for the placement flow. Empty
/// vec when the subdir doesn't exist.
pub fn design_place_constraints(
    workspace_dir: &Utf8Path,
) -> Result<Vec<PathBuf>> {
    design_constraints_in(workspace_dir, Some("place"))
}

/// Enumerate only the `route`-scoped constraints under
/// `<workspace_dir>/constraints/route/**/*.{xdc,sdc}`. Mirrors
/// [`design_synth_constraints`] for the routing flow. Empty
/// vec when the subdir doesn't exist.
pub fn design_route_constraints(
    workspace_dir: &Utf8Path,
) -> Result<Vec<PathBuf>> {
    design_constraints_in(workspace_dir, Some("route"))
}

/// Enumeration primitive shared by [`design_constraints`] and
/// the phase-scoped variants. `subdir = None` walks
/// `<workspace_dir>/constraints/`; `subdir = Some(name)` walks
/// `<workspace_dir>/constraints/<name>/`.
fn design_constraints_in(
    workspace_dir: &Utf8Path,
    subdir: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let mut dir = workspace_dir.join("constraints");
    if let Some(sub) = subdir {
        dir = dir.join(sub);
    }
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    find_constraint_files_impl(
        dir.as_std_path(),
        &mut files,
        /*recursive=*/ true,
    )?;
    files.sort();
    Ok(files)
}

/// Mirror of `find_vhdl_files_impl` but for the `.xdc` / `.sdc`
/// extension set. Kept separate rather than parameterizing the
/// existing walker because the extension list is small and
/// domain-specific — a generic "find by extensions" helper would
/// obscure the intent at the call site.
fn find_constraint_files_impl(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    recursive: bool,
) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                find_constraint_files_impl(&path, files, recursive)?;
            }
        } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext == "xdc" || ext == "sdc" {
                files.push(path);
            }
        }
    }
    Ok(())
}

/// Enumerate every generated IP wrapper under
/// `<workspace_dir>/target/ip/**/*.{vhd,vhdl}`. Populated by
/// `vw::make_wrapper` — see `~/src/htcl/vw/module.htcl` — which
/// drops one `wrapper.vhd` per IP into `target/ip/<ip>/`.
///
/// Returned separately from [`vhdl_design_sources`] because IP
/// wrappers have a different lifecycle: they're TOOL-generated
/// (regen on IP config change), not human-authored, and typically
/// compile into their own VHDL library (`ip` by convention). The
/// caller decides the library assignment.
///
/// **Excludes**:
/// - `target/ip/bd/**` and `target/ip/xci/**` — legacy caches
///   from the old custom BD/XCI save-restore path (retained as
///   an exclusion for now to survive workspaces that still have
///   those dirs on disk; the caches themselves are deleted at
///   session start — see the migration cleanup in `vw run` /
///   `vw repl`).
/// - `target/vw-project/**` — defensive. The on-disk Vivado
///   project (see `vw_project_dir`) sits SIBLING to `target/ip/`,
///   so this walker's root at `target/ip/` shouldn't ever
///   traverse it — but keep the filter as an invariant guard
///   for future walker refactors and to document intent.
///
/// The excluded content lands in the Vivado project via
/// `read_bd`/`read_ip`/`synth_ip` — re-adding through
/// `read_vhdl` would trigger `[filemgmt 20-1440] already exists
/// in the project as a part of sub-design file` CRITICAL WARNINGs.
///
/// Empty vec when `target/ip/` doesn't exist yet — a fresh
/// workspace hasn't run `vw::make_wrapper` for anything.
pub fn vhdl_ip_sources(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let ip_dir = workspace_dir.join("target").join("ip");
    if !ip_dir.exists() {
        return Ok(Vec::new());
    }
    let bd_cache = ip_dir.join("bd");
    let xci_cache = ip_dir.join("xci");
    let vw_project = workspace_dir.join("target").join("vw-project");
    let mut files =
        find_vhdl_files(ip_dir.as_std_path(), /*recursive=*/ true, &[])?;
    // `starts_with` on each canonical prefix filters every
    // nested path (`bd/cips/synth/cips.vhd`, `xci/primary_clock/
    // primary_clock.vhd`, and defensively any hypothetical
    // `vw-project/...` VHDL that a future walker refactor might
    // reach).
    files.retain(|p| {
        !p.starts_with(bd_cache.as_std_path())
            && !p.starts_with(xci_cache.as_std_path())
            && !p.starts_with(vw_project.as_std_path())
    });
    files.sort();
    Ok(files)
}

/// Enumerate every VHDL source under `<workspace>/bench/` — the
/// testbenches plus any shared bench code — skipping the `bench/target/`
/// build tree. These join `defaultlib` so the LSP can resolve an opened
/// testbench against the design it exercises (otherwise its `work.*`
/// instantiations are all undefined), and so `run_testbench` sees shared
/// bench code.
pub fn vhdl_bench_sources(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let bench_dir = workspace_dir.join("bench");
    if !bench_dir.exists() {
        return Ok(Vec::new());
    }
    let target = bench_dir.join("target");
    let mut files = find_vhdl_files(
        bench_dir.as_std_path(),
        /*recursive=*/ true,
        &[],
    )?;
    files.retain(|p| !p.starts_with(target.as_std_path()));
    files.sort();
    Ok(files)
}

/// Derive the VHDL library name a dep's sources compile into.
/// Hyphens become underscores (Vivado's `xelab` and NVC both
/// reject library names containing hyphens). Same rule
/// `vhdl_ls.toml` generation uses so the analyzer and the
/// synthesizer see identical library assignments.
fn library_name_for_dep(name: &str) -> String {
    name.replace('-', "_")
}

pub fn list_htcl_tests(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let test_dir = workspace_dir.join("test");
    if !test_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk_htcl_tests(test_dir.as_std_path(), &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_htcl_tests(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!(
            "Failed to read test directory {}: {e}",
            dir.display()
        ),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" {
            continue;
        }
        if path.is_file() {
            if path.extension().and_then(|s| s.to_str()) == Some("htcl") {
                out.push(path);
            }
        } else if path.is_dir() {
            walk_htcl_tests(&path, out)?;
        }
    }
    Ok(())
}

/// Enumerate every `.htcl` file under `<workspace_dir>` recursively,
/// excluding hidden dirs (`.git`, `.vw`), the build-artifact `target/`
/// dir, and vendored deps (`~/.vw/deps/` isn't under a workspace but
/// callers should never point us there anyway).
///
/// Broader than [`list_htcl_tests`] (which is `test/**/*.htcl` only)
/// — this walks the whole workspace so `synth_needs_update` can
/// invalidate a checkpoint when ANY authored htcl changes (the
/// entry-file `design.htcl`, per-IP `ip/*.htcl`, whatever the user
/// writes). Sorted for determinism.
pub fn list_workspace_htcl_files(
    workspace_dir: &Utf8Path,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_workspace_htcl(workspace_dir.as_std_path(), &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_workspace_htcl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!(
            "Failed to read workspace directory {}: {e}",
            dir.display()
        ),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs (.git, .vw, .vscode, etc.) and the
        // build-artifact `target/` tree. A user's checked-in
        // sources never live in either.
        if name_str.starts_with('.') || name_str == "target" {
            continue;
        }
        if path.is_file() {
            if path.extension().and_then(|s| s.to_str()) == Some("htcl") {
                out.push(path);
            }
        } else if path.is_dir() {
            walk_workspace_htcl(&path, out)?;
        }
    }
    Ok(())
}

/// Enumerate every tracked source file for the synth checkpoint,
/// in the fixed order fingerprint computation depends on.
///
/// Tracked source scope — matches what [`vw::synth`] actually
/// reads, plus a few coarse-grained triggers:
/// - VHDL design under `<ws>/hdl/**` (variant-filtered).
/// - IP wrappers under `<ws>/target/ip/**`.
/// - Synth-scoped XDCs under `<ws>/constraints/synth/**`.
/// - Every `.htcl` under `<ws>` (excludes hidden dirs + `target/`)
///   — captures edits to `design.htcl`, `ip/*.htcl`, and any
///   local htcl libs.
/// - Every dep-published VHDL file (from
///   `vhdl_dependency_sources_ext(exclude_sim_only=true)`, same
///   surface `vw::synth` feeds `read_vhdl`). Includes git deps
///   (materialized in `~/.vw/deps/<name>-<sha>`, content locked
///   by commit) and path deps (files change in place).
/// - `<ws>/vw.toml` — variant / target-part / deps-list changes
///   should re-trigger synth.
/// - `<ws>/vw.lock` — captures dep-version bumps that alter
///   which cache-dir a name resolves to.
///
/// Missing files pass through as-is; the fingerprint hasher
/// distinguishes "file present with content X" from "file
/// absent" by only folding present files into the digest and
/// including the sorted list of paths as part of the mix.
fn synth_source_paths(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.extend(vhdl_design_sources_for_variant(
        workspace_dir,
        active_variant,
    )?);
    sources.extend(vhdl_ip_sources(workspace_dir)?);
    sources.extend(design_synth_constraints(workspace_dir)?);
    sources.extend(list_workspace_htcl_files(workspace_dir)?);
    // Dependency VHDL — same surface `vw::synth` feeds into
    // `read_vhdl`. Matters for path deps (whose files change
    // in place, invisible to vw.lock) and belt-and-braces for
    // git deps (content is locked by commit, but hashing the
    // materialized files means a torn `.vw/deps` extraction
    // or a manual edit invalidates too).
    if let Ok(dep_sources) =
        vhdl_dependency_sources_ext(workspace_dir, false, true)
    {
        sources.extend(dep_sources.into_iter().map(|s| s.path));
    }
    sources.push(workspace_dir.join("vw.toml").into_std_path_buf());
    sources.push(workspace_dir.join("vw.lock").into_std_path_buf());
    sources.sort();
    sources.dedup();
    Ok(sources)
}

/// FNV-1a 64-bit hash. Stable across Rust versions (unlike
/// `std::hash::DefaultHasher`, which the language reserves the
/// right to change), fast, and good enough for cache-invalidation
/// use. Not cryptographic — collisions here just mean a false
/// "up-to-date" and a stale checkpoint, but the search space is
/// dozens to hundreds of files.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    fnv1a_64_extend(0xcbf2_9ce4_8422_2325, bytes)
}

fn fnv1a_64_extend(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Combined fingerprint over the contents + relative paths of a
/// list of source files. Mtime-independent, so tool-regenerated
/// files (IP wrappers rewritten identically each run) don't
/// spuriously invalidate a fresh checkpoint. Missing files are
/// folded in with a marker byte so a source appearing /
/// disappearing changes the digest. Shared engine for every
/// checkpoint kind — synth, IP configure, and future
/// checkpoints just supply their own path list.
fn fingerprint_paths(workspace_dir: &Utf8Path, paths: &[PathBuf]) -> u64 {
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for path in paths {
        // Fold the path (relative to workspace root when possible,
        // else absolute) so renames register as changes even when
        // content is identical.
        let rel = path
            .strip_prefix(workspace_dir.as_std_path())
            .unwrap_or(path);
        digest = fnv1a_64_extend(digest, rel.to_string_lossy().as_bytes());
        match fs::read(path) {
            Ok(content) => {
                // Marker byte `0x01` for "file present"; then the
                // file's own content hash mixed into the running
                // digest. Hashing the per-file hash (rather than
                // the full content stream) keeps this branch small.
                digest = fnv1a_64_extend(digest, &[0x01]);
                let file_hash = fnv1a_64(&content);
                digest = fnv1a_64_extend(digest, &file_hash.to_le_bytes());
            }
            Err(_) => {
                // Marker byte `0x00` for "file absent". Consistent
                // with the "missing = separate state from empty"
                // rule so `vw.lock` present-and-empty ≠ absent.
                digest = fnv1a_64_extend(digest, &[0x00]);
            }
        }
    }
    digest
}

/// Combined fingerprint over the synth source set. Backs
/// [`synth_needs_update`] and [`write_synth_checkpoint_manifest`].
pub fn synth_source_fingerprint(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> Result<u64> {
    let paths = synth_source_paths(workspace_dir, active_variant)?;
    Ok(fingerprint_paths(workspace_dir, &paths))
}

/// Manifest sidecar path — sits next to the checkpoint under
/// `<checkpoint>.manifest`. Small plain-text file (single u64 in
/// decimal). Kept alongside the checkpoint so `rm -rf target/`
/// wipes both together and there's never a stale manifest
/// pointing at a deleted checkpoint.
fn checkpoint_manifest_path(checkpoint: &Path) -> PathBuf {
    let mut name = checkpoint.file_name().unwrap_or_default().to_os_string();
    name.push(".manifest");
    checkpoint.with_file_name(name)
}

/// Write a manifest sidecar recording `fingerprint` next to
/// `checkpoint`. Shared by `write_synth_checkpoint_manifest`,
/// `write_place_checkpoint_manifest`, and `write_project_manifest`
/// so every checkpoint kind uses the same on-disk format.
fn write_checkpoint_manifest_with_fingerprint(
    checkpoint: &Path,
    fingerprint: u64,
) -> Result<()> {
    let manifest = checkpoint_manifest_path(checkpoint);
    fs::write(&manifest, format!("{fingerprint}\n")).map_err(|e| {
        VwError::FileSystem {
            message: format!(
                "Failed to write checkpoint manifest {}: {e}",
                manifest.display()
            ),
        }
    })
}

/// Compare `current_fingerprint` against the manifest sidecar
/// next to `checkpoint`. Returns `true` when the checkpoint is
/// missing, the manifest is missing / unparseable, or the
/// fingerprints disagree. Shared by every `*_needs_update` fn.
fn checkpoint_needs_update_with_fingerprint(
    checkpoint: &Path,
    current_fingerprint: u64,
) -> bool {
    !matches!(
        checkpoint_status(checkpoint, current_fingerprint),
        CheckpointStatus::Fresh
    )
}

/// Why a stage did or did not reuse its checkpoint.
///
/// The bool [`checkpoint_needs_update_with_fingerprint`] returns is what the
/// flow acts on, but it is not what somebody debugging a build needs. "Synth
/// ran again" has four quite different causes, and telling them apart from the
/// outside means guessing — which is exactly the position this type exists to
/// end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointStatus {
    /// The checkpoint is usable; the stage can be skipped.
    Fresh,
    /// No checkpoint file. Either the stage has never run here, or something
    /// removed it — `vw::synth` wipes its own stage directory on the full
    /// path, so a run that failed before writing leaves this state behind.
    Missing,
    /// The checkpoint is there but its manifest sidecar is not. Means the
    /// checkpoint was written and never marked, so its provenance is unknown
    /// and it cannot safely be trusted.
    Unmarked,
    /// The manifest exists and does not contain a fingerprint.
    Unreadable,
    /// Both are present and the tracked sources have changed since.
    Stale { stored: u64, current: u64 },
}

impl std::fmt::Display for CheckpointStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh => write!(f, "up to date"),
            Self::Missing => write!(f, "no checkpoint file"),
            Self::Unmarked => {
                write!(f, "checkpoint present but never marked (no manifest)")
            }
            Self::Unreadable => write!(f, "manifest unreadable"),
            Self::Stale { stored, current } => write!(
                f,
                "sources changed (checkpoint {stored:016x}, now {current:016x})"
            ),
        }
    }
}

/// Classify `checkpoint` against `current_fingerprint`.
///
/// The single place that decides freshness; every `*_needs_update` is this
/// with the answer narrowed to a bool.
pub fn checkpoint_status(
    checkpoint: &Path,
    current_fingerprint: u64,
) -> CheckpointStatus {
    if !checkpoint.exists() {
        return CheckpointStatus::Missing;
    }
    let manifest = checkpoint_manifest_path(checkpoint);
    let Ok(stored) = fs::read_to_string(&manifest) else {
        return CheckpointStatus::Unmarked;
    };
    let Ok(stored_fp) = stored.trim().parse::<u64>() else {
        return CheckpointStatus::Unreadable;
    };
    if stored_fp == current_fingerprint {
        CheckpointStatus::Fresh
    } else {
        CheckpointStatus::Stale {
            stored: stored_fp,
            current: current_fingerprint,
        }
    }
}

/// Which tracked source files disagree with what the checkpoint was marked
/// against, by re-fingerprinting each one on its own.
///
/// Only useful once [`checkpoint_status`] has said `Stale`: the combined
/// fingerprint says something moved, and this says what. The stage manifest
/// records only the combined value, so this cannot compare against the old
/// per-file hashes — what it reports is the tracked set as it stands, which is
/// enough to see a file nobody expected to be in it.
pub fn fingerprint_breakdown(
    workspace_dir: &Utf8Path,
    paths: &[PathBuf],
) -> Vec<(PathBuf, u64)> {
    paths
        .iter()
        .map(|path| {
            let rel = path
                .strip_prefix(workspace_dir.as_std_path())
                .unwrap_or(path);
            let mut h = fnv1a_64_extend(
                0xcbf2_9ce4_8422_2325,
                rel.to_string_lossy().as_bytes(),
            );
            match fs::read(path) {
                Ok(content) => {
                    h = fnv1a_64_extend(h, &[0x01]);
                    h = fnv1a_64_extend(h, &fnv1a_64(&content).to_le_bytes());
                }
                Err(_) => h = fnv1a_64_extend(h, &[0x00]),
            }
            (path.clone(), h)
        })
        .collect()
}

/// [`checkpoint_status`] for the synth stage.
pub fn synth_checkpoint_status(
    workspace_dir: &Utf8Path,
    checkpoint: &Path,
    active_variant: Option<&str>,
) -> Result<CheckpointStatus> {
    let fp = synth_source_fingerprint(workspace_dir, active_variant)?;
    Ok(checkpoint_status(checkpoint, fp))
}

/// Write the manifest sidecar for a freshly-produced synth
/// checkpoint. Records the current source fingerprint so
/// [`synth_needs_update`] can decide freshness by content
/// comparison rather than mtime.
///
/// Called from `vw::synth` immediately after
/// `vivado_cmd::write_checkpoint` completes.
pub fn write_synth_checkpoint_manifest(
    workspace_dir: &Utf8Path,
    checkpoint: &Path,
    active_variant: Option<&str>,
) -> Result<()> {
    let fp = synth_source_fingerprint(workspace_dir, active_variant)?;
    write_checkpoint_manifest_with_fingerprint(checkpoint, fp)
}

/// Returns `true` when either
/// - the checkpoint file is missing, or
/// - its manifest sidecar is missing / unreadable / stores a
///   fingerprint different from the tracked source set's current
///   fingerprint.
///
/// Content-hash based (not mtime): identical `make_wrapper`
/// output with a shifted mtime does NOT invalidate the checkpoint,
/// which is the whole point — the wrapper is regenerated on every
/// design.htcl run but its stripped-header body is stable when
/// the source `.bd` hasn't changed.
pub fn synth_needs_update(
    workspace_dir: &Utf8Path,
    checkpoint: &Path,
    active_variant: Option<&str>,
) -> Result<bool> {
    let current_fp = synth_source_fingerprint(workspace_dir, active_variant)?;
    Ok(checkpoint_needs_update_with_fingerprint(
        checkpoint, current_fp,
    ))
}

// ---------------------------------------------------------------------
// Place checkpoint — a per-workspace cache scoped to the place stage.
// Backs `vw::place` in the htcl vw module.
//
// Source scope is narrower than synth's: place XDCs under
// `<ws>/constraints/place/**` PLUS the upstream synth checkpoint
// file itself. The synth DCP acts as a proxy for "everything synth
// depended on" — if any synth-scope source changed, synth re-ran
// and produced a fresh DCP, invalidating the place fingerprint.
// This avoids duplicating the synth source enumeration here.
// ---------------------------------------------------------------------

/// Path list feeding [`place_source_fingerprint`]. Sorted +
/// deduped. Missing files pass through — the fingerprint's
/// present/absent marker byte covers the "checkpoint doesn't
/// exist yet" case correctly.
fn place_source_paths(
    workspace_dir: &Utf8Path,
    synth_checkpoint: &Path,
) -> Result<Vec<PathBuf>> {
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.extend(design_place_constraints(workspace_dir)?);
    sources.push(synth_checkpoint.to_path_buf());
    sources.sort();
    sources.dedup();
    Ok(sources)
}

/// Combined fingerprint over the place source set (place XDCs +
/// the upstream synth checkpoint file). Backs
/// [`place_needs_update`] and [`write_place_checkpoint_manifest`].
pub fn place_source_fingerprint(
    workspace_dir: &Utf8Path,
    synth_checkpoint: &Path,
) -> Result<u64> {
    let paths = place_source_paths(workspace_dir, synth_checkpoint)?;
    Ok(fingerprint_paths(workspace_dir, &paths))
}

/// Write the manifest sidecar for a freshly-produced place
/// checkpoint. Called from `vw::place` after
/// `vivado_cmd::write_checkpoint` completes.
pub fn write_place_checkpoint_manifest(
    workspace_dir: &Utf8Path,
    place_checkpoint: &Path,
    synth_checkpoint: &Path,
) -> Result<()> {
    let fp = place_source_fingerprint(workspace_dir, synth_checkpoint)?;
    write_checkpoint_manifest_with_fingerprint(place_checkpoint, fp)
}

/// Returns `true` when the place checkpoint OR its manifest is
/// missing, OR the fingerprint stored in the manifest differs
/// from the current one. Mirrors [`synth_needs_update`] with a
/// tighter source scope (place XDCs + the synth DCP proxy).
pub fn place_needs_update(
    workspace_dir: &Utf8Path,
    place_checkpoint: &Path,
    synth_checkpoint: &Path,
) -> Result<bool> {
    let current_fp = place_source_fingerprint(workspace_dir, synth_checkpoint)?;
    Ok(checkpoint_needs_update_with_fingerprint(
        place_checkpoint,
        current_fp,
    ))
}

// ---------------------------------------------------------------------
// Route checkpoint helpers
//
// Same shape as the place helpers above but one stage down: route
// XDCs (`<ws>/constraints/route/**`) PLUS the upstream *place*
// DCP file. The place DCP acts as the "everything place depended
// on" proxy — if place re-ran, its DCP is fresh and the route
// fingerprint invalidates automatically. Same reason place folds
// in the synth DCP.
// ---------------------------------------------------------------------

/// Path list feeding [`route_source_fingerprint`]. Sorted + deduped.
/// Missing files pass through — the fingerprint's present/absent
/// marker byte covers the "checkpoint doesn't exist yet" case.
fn route_source_paths(
    workspace_dir: &Utf8Path,
    place_checkpoint: &Path,
) -> Result<Vec<PathBuf>> {
    let mut sources: Vec<PathBuf> = Vec::new();
    sources.extend(design_route_constraints(workspace_dir)?);
    sources.push(place_checkpoint.to_path_buf());
    sources.sort();
    sources.dedup();
    Ok(sources)
}

/// Combined fingerprint over the route source set (route XDCs +
/// the upstream place checkpoint file). Backs
/// [`route_needs_update`] and [`write_route_checkpoint_manifest`].
pub fn route_source_fingerprint(
    workspace_dir: &Utf8Path,
    place_checkpoint: &Path,
) -> Result<u64> {
    let paths = route_source_paths(workspace_dir, place_checkpoint)?;
    Ok(fingerprint_paths(workspace_dir, &paths))
}

/// Write the manifest sidecar for a freshly-produced route
/// checkpoint. Called from `vw::route` after
/// `vivado_cmd::write_checkpoint` completes.
pub fn write_route_checkpoint_manifest(
    workspace_dir: &Utf8Path,
    route_checkpoint: &Path,
    place_checkpoint: &Path,
) -> Result<()> {
    let fp = route_source_fingerprint(workspace_dir, place_checkpoint)?;
    write_checkpoint_manifest_with_fingerprint(route_checkpoint, fp)
}

/// Returns `true` when the route checkpoint OR its manifest is
/// missing, OR the fingerprint stored in the manifest differs
/// from the current one. Mirrors [`place_needs_update`] with the
/// route source scope (route XDCs + the place DCP proxy).
pub fn route_needs_update(
    workspace_dir: &Utf8Path,
    route_checkpoint: &Path,
    place_checkpoint: &Path,
) -> Result<bool> {
    let current_fp = route_source_fingerprint(workspace_dir, place_checkpoint)?;
    Ok(checkpoint_needs_update_with_fingerprint(
        route_checkpoint,
        current_fp,
    ))
}

// ---------------------------------------------------------------------
// IP configuration checkpoint — a per-workspace cache scoped to the
// entry `ip/module.htcl` (and everything it srcs). Backs
// `vw::configure_ip` in the htcl vw module.
//
// Source scope: every `.htcl` under `<ws>/ip/`. This is intentionally
// tighter than the synth scope — `ip::configure` produces BDs / XCI
// IPs / wrappers whose input surface is defined entirely by the ip/
// tree. If the user's ip/ htcl changes (adding an IP, tweaking
// a configure_* parameter), the checkpoint invalidates. If the
// user's design.htcl changes, it doesn't — that only matters for
// the downstream synth step, which has its own checkpoint.
// ---------------------------------------------------------------------

/// Enumerate every `.htcl` file under `<workspace_dir>/ip/`,
/// recursively, sorted. Empty vec when `ip/` doesn't exist yet.
/// Skips hidden dirs (`.git`, `.vw`) and `target/` for the same
/// reason [`list_workspace_htcl_files`] does — those aren't
/// authored sources.
pub fn list_ip_htcl_files(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let ip_dir = workspace_dir.join("ip");
    if !ip_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk_workspace_htcl(ip_dir.as_std_path(), &mut out)?;
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------
// On-disk Vivado project — vw manages a persistent Vivado project at
// `<ws>/target/vw-project/<name>/` so BD/IP state survives across
// sessions via Vivado's native `save_project`/`open_project`
// machinery (instead of hand-serializing BD .bd files, XCI dirs,
// ipshared/, etc.). See `~/.claude/plans/abundant-petting-lecun.md`.
//
// Fingerprint scope: `<ws>/ip/**/*.htcl` + `<ws>/vw.toml`. Any
// substantive edit to those invalidates the on-disk project —
// wiped + recreated on the next spawn, then `ip::configure` re-runs
// and `vw::mark_project_configured` writes a fresh manifest.
// ---------------------------------------------------------------------

/// Absolute path of the on-disk Vivado project directory vw
/// manages. The `.xpr` itself lives at
/// `<returned_path>/<project_name>/<project_name>.xpr` (Vivado's
/// `create_project -dir` convention — it always nests one level
/// deep under the given dir).
pub fn vw_project_dir(workspace_dir: &Utf8Path) -> Utf8PathBuf {
    workspace_dir.join("target").join("vw-project")
}

/// Path list feeding [`project_source_fingerprint`]. Sorted +
/// deduped. Every `.htcl` under `<ws>/ip/` plus `<ws>/vw.toml`.
/// A missing `vw.toml` folds in as the "absent" marker via
/// [`fingerprint_paths`]'s present/absent handling, so an empty
/// or missing workspace still hashes deterministically.
fn project_source_paths(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let mut sources = list_ip_htcl_files(workspace_dir)?;
    sources.push(workspace_dir.join("vw.toml").into_std_path_buf());
    sources.sort();
    sources.dedup();
    Ok(sources)
}

/// Combined fingerprint over the on-disk-project source set.
/// Backs [`project_needs_wipe`] and [`write_project_manifest`].
pub fn project_source_fingerprint(workspace_dir: &Utf8Path) -> Result<u64> {
    let paths = project_source_paths(workspace_dir)?;
    Ok(fingerprint_paths(workspace_dir, &paths))
}

/// Absolute path of the `.xpr` inside the on-disk project dir.
/// Vivado's `create_project -dir <D> -name <N>` produces
/// `<D>/<N>/<N>.xpr`. All manifest / checkpoint plumbing uses
/// this path as the "checkpoint" argument to the shared helpers
/// (`write_checkpoint_manifest_with_fingerprint`,
/// `checkpoint_needs_update_with_fingerprint`), which append
/// `.manifest` to derive the sidecar path
/// (`<D>/<N>/<N>.xpr.manifest`).
pub fn vw_project_xpr(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join(name).join(format!("{name}.xpr"))
}

/// Write the manifest sidecar for a freshly-configured on-disk
/// project. Called from `vw::configure_ip` (via the
/// `mark_project_configured` RPC) after `save_project` completes.
///
/// Invariant: manifest presence means "the project at this dir
/// was successfully configured with these sources' fingerprint."
/// `project_needs_wipe` relies on this — never write the manifest
/// before `save_project` succeeds.
pub fn write_project_manifest(
    workspace_dir: &Utf8Path,
    project_dir: &Path,
    name: &str,
) -> Result<()> {
    let fp = project_source_fingerprint(workspace_dir)?;
    write_checkpoint_manifest_with_fingerprint(
        &vw_project_xpr(project_dir, name),
        fp,
    )
}

/// Returns `true` when the on-disk project at `<project_dir>/<name>/`
/// should be wiped + recreated:
/// - the `.xpr` is missing, OR
/// - the manifest sidecar is missing / unreadable, OR
/// - the stored fingerprint differs from the current one.
///
/// Called by `vw run` / `vw repl` before spawning Vivado (see
/// `vw-cli/src/main.rs` / `vw-repl/src/app.rs`); when it returns
/// true, the caller `remove_dir_all(project_dir)` before passing
/// `persist_dir: Some(project_dir)` to `AutoProject`.
pub fn project_needs_wipe(
    workspace_dir: &Utf8Path,
    project_dir: &Path,
    name: &str,
) -> Result<bool> {
    let current_fp = project_source_fingerprint(workspace_dir)?;
    Ok(checkpoint_needs_update_with_fingerprint(
        &vw_project_xpr(project_dir, name),
        current_fp,
    ))
}

/// Idempotent one-shot cleanup of the legacy IP-cache artifacts
/// that lived under `<ws>/target/ip/` before the on-disk Vivado
/// project migration:
///   `<ws>/target/ip/bd/`
///   `<ws>/target/ip/xci/`
///   `<ws>/target/ip/.ip-cache`
///   `<ws>/target/ip/.ip-cache.manifest`
///
/// Called by `vw run` / `vw repl` at bootstrap; on first
/// on-disk-mode session it wipes stale bytes and returns the
/// count. On subsequent sessions it's a no-op (returns 0).
///
/// Deliberately does NOT touch `<ws>/target/ip/<ip>/wrapper.vhd`
/// — those are `vw::make_wrapper` outputs still consumed by
/// `vw::synth`, and their `<ip>` sibling dirs may still be
/// populated by the on-disk project's `generate_target` outputs.
pub fn cleanup_legacy_ip_cache(workspace_dir: &Utf8Path) -> usize {
    let ip_dir = workspace_dir.join("target").join("ip");
    let targets = [
        ip_dir.join("bd"),
        ip_dir.join("xci"),
        ip_dir.join(".ip-cache"),
        ip_dir.join(".ip-cache.manifest"),
    ];
    let mut removed = 0;
    for t in targets {
        let p = t.as_std_path();
        if !p.exists() {
            continue;
        }
        let ok = if p.is_dir() {
            fs::remove_dir_all(p).is_ok()
        } else {
            fs::remove_file(p).is_ok()
        };
        if ok {
            removed += 1;
        }
    }
    removed
}

/// Outcome of [`prepare_vw_project_dir`]. The caller decides how
/// to surface the messages (`tracing::info!`, REPL banner, etc.);
/// vw-lib itself intentionally doesn't do user-facing IO.
#[derive(Debug, Clone)]
pub struct PreparedProjectDir {
    /// Absolute path of `<ws>/target/vw-project/`, ready to pass
    /// as `AutoProject::persist_dir = Some(...)`.
    pub project_dir: Utf8PathBuf,
    /// Number of legacy `<ws>/target/ip/{bd,xci,.ip-cache*}`
    /// entries removed by the one-shot Phase 6 migration cleanup.
    pub legacy_cache_removed: usize,
    /// `Some(path)` iff the persist dir existed AND
    /// [`project_needs_wipe`] returned true, so we wiped it before
    /// returning. The caller can log this as the reason for the
    /// fresh recreate that Vivado is about to do.
    ///
    /// We wipe `<ws>/target/vw-project/` in its entirety (not
    /// just the per-`name` subdir): historically bugs have
    /// caused Vivado to scatter flat siblings like
    /// `<persist>/<name>.xpr`, `<persist>/<name>.srcs` alongside
    /// the nested `<persist>/<name>/` — a full-dir wipe reliably
    /// sweeps any layout drift instead of leaving cross-version
    /// clutter behind.
    pub wiped_project: Option<Utf8PathBuf>,
}

/// Bootstrap the on-disk Vivado project dir for a workspace and
/// return its absolute path, ready to pass as
/// `AutoProject::persist_dir = Some(...)`.
///
/// Does three things in order (all idempotent):
///
/// 1. Silently clean up legacy `target/ip/{bd,xci,.ip-cache*}`
///    artifacts from the pre-migration IP cache
///    ([`cleanup_legacy_ip_cache`]).
/// 2. Consult [`project_needs_wipe`]; if the `.xpr` is missing,
///    the manifest sidecar is missing, or the fingerprint is
///    stale, `remove_dir_all(<project_dir>/<name>)` so the worker
///    takes the fresh-create branch instead of `open_project` on
///    stale bytes.
/// 3. `create_dir_all(<project_dir>)` so Vivado's later
///    `create_project -dir` has a real parent.
///
/// Everything worth reporting to the user (legacy-cleanup counts,
/// wipe reason) lands in the returned [`PreparedProjectDir`] for
/// the caller to log.
pub fn prepare_vw_project_dir(
    workspace_dir: &Utf8Path,
    name: &str,
) -> Result<PreparedProjectDir> {
    let project_dir = vw_project_dir(workspace_dir);
    let legacy_cache_removed = cleanup_legacy_ip_cache(workspace_dir);
    let mut wiped_project = None;
    if project_needs_wipe(workspace_dir, project_dir.as_std_path(), name)?
        && project_dir.exists()
    {
        fs::remove_dir_all(project_dir.as_std_path())?;
        wiped_project = Some(project_dir.clone());
    }
    fs::create_dir_all(project_dir.as_std_path())?;
    Ok(PreparedProjectDir {
        project_dir,
        legacy_cache_removed,
        wiped_project,
    })
}

/// Per-bench output directory under the workspace `target/`, holding a
/// testbench run's artifacts (waveform, Xyce `.prn`, generated plots).
pub fn bench_output_dir(workspace_dir: &Utf8Path, name: &str) -> Utf8PathBuf {
    workspace_dir.join("target").join("bench").join(name)
}

/// Workspace-relative locations for anodizer artifacts.
const ANODIZER_BUILD_SUBDIR: &str = "target/anodizer/build";
const ANODIZER_GEN_SUBDIR: &str = "target/anodizer/gen";
const ANODIZER_FINGERPRINT_FILE: &str = "target/anodizer/.fingerprint";

/// Generate anodizer Rust structs for the workspace's `serialize_rust`-tagged
/// VHDL records when they are missing or stale, so the testbench Rust build can
/// consume them.
///
/// Detection is two-stage: (1) skip entirely when no design source carries the
/// `serialize_rust` attribute; (2) otherwise regenerate only when the design
/// sources' content fingerprint differs from the last successful run. The nvc
/// scratch build lands in `target/anodizer/build` and the generated Rust in
/// `target/anodizer/gen`, both under the workspace root.
pub async fn ensure_anodized(
    workspace_dir: &Utf8Path,
    vhdl_std: VhdlStandard,
    active_variant: Option<&str>,
) -> Result<()> {
    anodize(workspace_dir, vhdl_std, active_variant, Freshness::IfStale)
        .await
        .map(|_| ())
}

/// Whether an anodization pass may decide it has nothing to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Skip when the design sources have not changed since the last pass.
    /// What every build path wants: the generated file is a pure function of
    /// those sources, so regenerating an unchanged one only costs an nvc run
    /// and a rebuild of everything downstream of it.
    IfStale,
    /// Regenerate regardless. What `vw cosim anodize` wants: a developer
    /// working on the anodizer itself is asking to watch it run, and a pass
    /// that reports "already current" answers a question nobody asked.
    Always,
}

/// What an anodization pass found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnodizeReport {
    /// Design sources considered.
    pub sources: usize,
    /// How many of them carry a `serialize_rust` attribute.
    ///
    /// Zero is a real answer and worth reporting rather than passing over in
    /// silence: a developer who has just tagged a record and sees zero has
    /// learnt that the file they edited is not in the design set.
    pub tagged: usize,
    /// Where the generated Rust went, when there was any to write.
    pub generated: Option<Utf8PathBuf>,
    /// How many lines it came to.
    pub lines: usize,
    /// Whether this pass ran the anodizer, or found the last one's output
    /// still current.
    pub regenerated: bool,
}

/// What an anodization pass would look at, without running one.
///
/// Returns the number of design sources and how many of them carry a
/// `serialize_rust` attribute. Cheap — it is the same scan the generator gates
/// itself on — and worth having separately so a caller can say what is about
/// to happen *before* it happens. A pass that fails is exactly when knowing
/// how much it was looking at matters most.
pub fn anodize_scope(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> Result<(usize, usize)> {
    let config = render_vhdl_ls_config(workspace_dir, active_variant, false)?;
    let files = config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();
    let tagged = tagged_sources(&files);
    Ok((files.len(), tagged))
}

/// How many of these sources ask for anything to be generated.
fn tagged_sources(files: &[PathBuf]) -> usize {
    files
        .iter()
        .filter(|f| {
            fs::read_to_string(f)
                .map(|c| c.contains("serialize_rust"))
                .unwrap_or(false)
        })
        .count()
}

/// Generate anodizer Rust structs for the workspace's `serialize_rust`-tagged
/// VHDL records, and say what happened.
///
/// [`ensure_anodized`] is this with [`Freshness::IfStale`] and the answer
/// thrown away — which is what a build wants, since a build only cares that
/// the file is there and current.
pub async fn anodize(
    workspace_dir: &Utf8Path,
    vhdl_std: VhdlStandard,
    active_variant: Option<&str>,
    freshness: Freshness,
) -> Result<AnodizeReport> {
    let config = render_vhdl_ls_config(workspace_dir, active_variant, false)?;

    // Tagged records live in the design sources, i.e. `defaultlib`.
    let defaultlib_files = config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    let mut report = AnodizeReport {
        sources: defaultlib_files.len(),
        // Stage 1: cheap gate — is anything tagged for serialization at all?
        tagged: tagged_sources(&defaultlib_files),
        ..Default::default()
    };
    if report.tagged == 0 {
        return Ok(report);
    }

    // Stage 2: regenerate only when the design sources changed.
    let gen_dir = workspace_dir.join(ANODIZER_GEN_SUBDIR);
    let generated = gen_dir.join("generated_structs.rs");
    let fingerprint_file = workspace_dir.join(ANODIZER_FINGERPRINT_FILE);
    let fingerprint = fingerprint_paths(workspace_dir, &defaultlib_files);

    let stored = fs::read_to_string(&fingerprint_file)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let current = generated.exists() && stored == Some(fingerprint);
    if current && freshness == Freshness::IfStale {
        report.generated = Some(generated);
        return Ok(report);
    }

    let build_dir = workspace_dir.join(ANODIZER_BUILD_SUBDIR);
    fs::create_dir_all(&gen_dir)?;
    anodizer::anodize(&config, &build_dir, &gen_dir, vhdl_std).await?;

    // Written even for a forced pass: the work was done, and leaving the
    // fingerprint stale would make the next build redo it for nothing.
    fs::write(&fingerprint_file, fingerprint.to_string())?;

    report.lines = fs::read_to_string(&generated)
        .map(|text| text.lines().count())
        .unwrap_or(0);
    report.generated = Some(generated);
    report.regenerated = true;
    Ok(report)
}

/// Run a testbench using NVC simulator.
#[allow(clippy::too_many_arguments)]
/// True when `dir` holds a Cargo.toml that declares a `[package]` — i.e. a
/// buildable crate (a cosim testbench driver), as opposed to a
/// `[workspace]`-only manifest like the top-level `bench/Cargo.toml`.
fn dir_is_rust_crate(dir: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    toml::from_str::<toml::Value>(&contents)
        .map(|v| v.get("package").is_some())
        .unwrap_or(false)
}

/// Regenerate the cosim bridge scaffold (`Cargo.toml`, `build.rs`,
/// generated sources) for every `bench/<name>/mist.toml` in the
/// workspace, up front.
///
/// `bench/` is a single cargo workspace whose members include each
/// mixed-signal bench crate, so a missing `bench/<name>/Cargo.toml`
/// (e.g. after `git clean -fdx`, which wipes the generated scaffold)
/// makes cargo fail to LOAD the workspace manifest — breaking
/// `cargo build` for EVERY bench, not just the cosim ones. Scaffolding
/// them all before any bench builds keeps the workspace valid; the
/// mixed-signal benches themselves don't have to be run (or even
/// discovered) for their crate to need to exist. `write_file` is
/// content-aware, so unchanged scaffolds don't touch the tree.
pub fn ensure_bench_scaffolds(workspace_dir: &Utf8Path) -> Result<()> {
    // A cosim crate's `build.rs` is generated too, and goes missing the same
    // ways — see `bench_init::heal_cosim_scaffolds`.
    bench_init::heal_cosim_scaffolds(workspace_dir)?;

    let bench_dir = workspace_dir.join("bench");
    let Ok(entries) = fs::read_dir(bench_dir.as_std_path()) else {
        return Ok(()); // no bench dir → nothing to scaffold
    };
    let mut ws_config: Option<WorkspaceConfig> = None;
    for entry in entries.flatten() {
        let dir = entry.path();
        let mist_toml = dir.join("mist.toml");
        if !dir.is_dir() || !mist_toml.exists() {
            continue;
        }
        let mist_content =
            fs::read_to_string(&mist_toml).map_err(|e| VwError::Config {
                message: format!("Failed to read {}: {e}", mist_toml.display()),
            })?;
        let mist_config: MistConfig =
            toml::from_str(&mist_content).map_err(|e| VwError::Config {
                message: format!(
                    "Failed to parse {}: {e}",
                    mist_toml.display()
                ),
            })?;
        // Load the workspace config lazily and once — only needed when
        // there's at least one mixed-signal bench.
        if ws_config.is_none() {
            ws_config = Some(load_workspace_config(workspace_dir)?);
        }
        let bench_test_dir =
            Utf8PathBuf::from_path_buf(dir.clone()).map_err(|p| {
                VwError::FileSystem {
                    message: format!(
                        "bench path is not UTF-8: {}",
                        p.display()
                    ),
                }
            })?;
        sim::scaffold(&bench_test_dir, &mist_config)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_testbench(
    workspace_dir: &Utf8Path,
    testbench_name: String,
    vhdl_std: VhdlStandard,
    recurse: bool,
    runtime_flags: &[String],
    build_rust: bool,
    scaffold: bool,
    build_dir: &str,
) -> Result<()> {
    let bench_test_dir = workspace_dir.join("bench").join(&testbench_name);

    // A cosim bench that drives a design entity directly: `cosim.toml` names
    // the entity, nvc elaborates that entity as the top level, and the crate
    // beside the config is the only thing driving it. No VHDL harness is
    // involved, so none of the testbench-file machinery below applies.
    let cosim_toml = bench_test_dir.join("cosim.toml");
    if cosim_toml.exists() {
        let config = cosim::read_config(cosim_toml.as_std_path())?;
        return cosim::run(
            workspace_dir,
            &testbench_name,
            &bench_test_dir,
            &config,
            vhdl_std,
            build_dir,
            runtime_flags,
        )
        .await;
    }

    // Check for mixed-signal test (mist.toml in bench/<name>/)
    let mist_toml = bench_test_dir.join("mist.toml");
    if mist_toml.exists() {
        let mist_content =
            fs::read_to_string(&mist_toml).map_err(|e| VwError::Config {
                message: format!("Failed to read mist.toml: {e}"),
            })?;
        let mist_config: MistConfig =
            toml::from_str(&mist_content).map_err(|e| VwError::Config {
                message: format!("Failed to parse mist.toml: {e}"),
            })?;
        if scaffold {
            return sim::scaffold(&bench_test_dir, &mist_config);
        }
        // Auto-scaffold before simulating so `vw bench run` works straight
        // from a clean checkout (`git clean -fdx` wipes the generated
        // bridge crate — `Cargo.toml`, `build.rs`, generated sources —
        // that `run_analog_test`'s `build_bridge_library` needs) without
        // a manual `vw bench run <name> --scaffold` pre-step. `scaffold`
        // regenerates only the boilerplate (the user-owned `src/lib.rs`
        // is left alone) and `write_file` is content-aware, so this is a
        // cheap no-op when nothing changed.
        sim::scaffold(&bench_test_dir, &mist_config)?;
        return sim::run_analog_test(
            workspace_dir,
            &testbench_name,
            &bench_test_dir,
            &mist_config,
            vhdl_std,
            build_dir,
        )
        .await;
    }

    let vhdl_ls_config = render_vhdl_ls_config(workspace_dir, None, false)?;
    let mut processor = RecordProcessor::new(vhdl_std);
    let mut cache = FileCache::new();

    fs::create_dir_all(build_dir)?;

    // First, analyze all non-defaultlib libraries
    analyze_ext_libraries(
        &vhdl_ls_config,
        &mut processor,
        vhdl_std,
        build_dir,
        &mut cache,
    )
    .await?;

    // Get defaultlib files for later use
    let defaultlib_files = vhdl_ls_config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    // Look for the testbench file in bench folder
    let bench_dir = workspace_dir.join("bench");
    if !bench_dir.exists() {
        return Err(VwError::Testbench {
            message: format!("No 'bench' directory found in {workspace_dir}"),
        });
    }

    let testbench_file = find_testbench_file(
        &testbench_name,
        &bench_dir,
        recurse,
        cache.entities_cache_mut(),
    )?;

    // Filter defaultlib files to exclude OTHER testbenches but allow common bench code
    let bench_dir_abs = workspace_dir.as_std_path().join("bench");

    // Pre-compute entities for bench files to avoid mutable borrow in closure
    let mut bench_file_entities: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for file_path in &defaultlib_files {
        let absolute_path = if file_path.is_relative() {
            workspace_dir.as_std_path().join(file_path)
        } else {
            file_path.clone()
        };
        if absolute_path.starts_with(&bench_dir_abs) {
            if let Ok(entities) = cache.get_entities(&absolute_path) {
                bench_file_entities.insert(absolute_path, entities.clone());
            }
        }
    }

    let filtered_defaultlib_files: Vec<PathBuf> = defaultlib_files
        .into_iter()
        .filter(|file_path| {
            // Convert to absolute path for comparison
            let absolute_path = if file_path.is_relative() {
                workspace_dir.as_std_path().join(file_path)
            } else {
                file_path.clone()
            };

            // If it's not in the bench directory, include it
            if !absolute_path.starts_with(&bench_dir_abs) {
                return true;
            }

            // If it's in the bench directory, check if it's a different testbench
            if let Some(entities) = bench_file_entities.get(&absolute_path) {
                // Exclude files that contain testbench entities other than the one we're running
                for entity in entities {
                    if entity.to_lowercase().ends_with("_tb")
                        && entity != &testbench_name
                    {
                        return false; // This is a different testbench, exclude it
                    }
                }
            }

            // Include this file (it's either the current testbench or common bench code)
            true
        })
        .collect();

    // Find only the defaultlib files that are actually referenced by this testbench
    let mut referenced_files = find_referenced_files(
        &testbench_file,
        &filtered_defaultlib_files,
        &mut cache,
    )?;

    // Sort files in dependency order (dependencies first)
    sort_files_by_dependencies(
        &mut processor,
        &mut referenced_files,
        &mut cache,
    )?;

    let mut files: Vec<String> = referenced_files
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();

    files.push(testbench_file.to_string_lossy().to_string());

    run_nvc_analysis(vhdl_std, build_dir, "work", &files, false).await?;

    run_nvc_elab(vhdl_std, build_dir, "work", &testbench_name, false).await?;

    // A testbench whose directory is a Rust *crate* is a cosim bench: its DUT
    // inputs are driven by that Rust driver, so it must be loaded or the
    // inputs float and numeric_std floods with metavalue warnings. Build and
    // load it automatically (even without an explicit `--build-rust`). A
    // pure-VHDL testbench sitting directly in `bench/` is *not* — its parent
    // `Cargo.toml` is the bench `[workspace]` manifest (no `[package]`), so we
    // must check for a real crate rather than any `Cargo.toml`.
    let is_cosim_bench = testbench_file
        .parent()
        .map(dir_is_rust_crate)
        .unwrap_or(false);
    let rust_lib_path = if build_rust || is_cosim_bench {
        Some(
            build_rust_library(&bench_dir, &testbench_file)
                .await?
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };

    // Run NVC simulation, writing the waveform into the per-bench output dir.
    let bench_out = bench_output_dir(workspace_dir, &testbench_name);
    fs::create_dir_all(&bench_out)?;
    run_nvc_sim(
        vhdl_std,
        build_dir,
        "work",
        &testbench_name,
        bench_out.as_str(),
        rust_lib_path,
        &runtime_flags.to_vec(),
        false,
    )
    .await?;

    Ok(())
}

/// Build a `VhdlLsConfig` in memory from the workspace's live
/// enumeration — no disk I/O against `<ws>/vhdl_ls.toml`.
///
/// Populated libraries:
/// - `defaultlib` = design VHDL under `<ws>/hdl/**` (variant-filtered).
/// - `ip` = IP wrappers under `<ws>/target/ip/<name>/wrapper.vhd`
///   (Vivado cache subtrees are excluded by `vhdl_ip_sources`).
/// - `xil_defaultlib` = Vivado-generated BD RTL under
///   `<ws>/target/vw-project/**/*.gen/sources_1/bd/**/*.vhd`,
///   present only when the on-disk Vivado project exists.
/// - one library per dep name (hyphens→underscores via
///   `library_name_for_dep`), sourced from
///   `vhdl_dependency_sources_ext`.
///
/// The sim and LSP layers both consume this instead of parsing
/// `vhdl_ls.toml`; the file is no longer authoritative.
pub fn render_vhdl_ls_config(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
    include_bench: bool,
) -> Result<VhdlLsConfig> {
    let mut libraries: HashMap<String, VhdlLsLibrary> = HashMap::new();

    // `active_variant = None` in a variant-mode workspace means
    // "the workspace's default variant". Unioning across every
    // variant instead would put both `top-vpk120.vhd` AND
    // `top-metro.vhd` in `defaultlib`, and vhdl_lang would report
    // duplicate-entity / cross-variant unresolved references as
    // workspace-wide diagnostics — polluting the LSP outline with
    // the inactive variant's broken references.
    let resolved_variant =
        resolve_active_variant(workspace_dir, active_variant);
    let mut design_files = vhdl_design_sources_for_variant(
        workspace_dir,
        resolved_variant.as_deref(),
    )?;
    // The LSP puts testbenches in `defaultlib` so an opened tb resolves
    // against `work.*`. The sim / anodize paths must NOT — they compile only
    // the referenced design set and would choke on bench files that pull in
    // external libs (e.g. VUnit) or on unrelated broken testbenches.
    if include_bench {
        design_files.extend(vhdl_bench_sources(workspace_dir)?);
    }
    if !design_files.is_empty() {
        libraries.insert(
            "defaultlib".to_string(),
            VhdlLsLibrary {
                files: design_files,
                exclude: None,
                is_third_party: None,
            },
        );
    }

    let ip_files = vhdl_ip_sources(workspace_dir)?;
    if !ip_files.is_empty() {
        libraries.insert(
            "ip".to_string(),
            VhdlLsLibrary {
                files: ip_files,
                exclude: None,
                is_third_party: None,
            },
        );
    }

    let bd_rtl = vivado_generated_sources(workspace_dir)?;
    if !bd_rtl.is_empty() {
        libraries.insert(
            "xil_defaultlib".to_string(),
            VhdlLsLibrary {
                files: bd_rtl,
                exclude: None,
                // Suppresses lint noise on Xilinx-generated RTL,
                // matching how vhdl_ls treats vendor deps.
                is_third_party: Some(true),
            },
        );
    }

    let dep_sources = vhdl_dependency_sources_ext(workspace_dir, true, false)?;
    for src in dep_sources {
        libraries
            .entry(src.library)
            .or_insert_with(|| VhdlLsLibrary {
                files: Vec::new(),
                exclude: None,
                is_third_party: Some(true),
            })
            .files
            .push(src.path);
    }

    Ok(VhdlLsConfig {
        // VHDL 2019 is vw's baseline (mode views etc. show up in
        // every non-legacy workspace we support). Without this
        // vhdl_lang defaults to 2008 and bails out on view syntax
        // partway through the source, which drops entire packages
        // from library visibility and shows up as `No primary
        // unit '<name>' within library 'defaultlib'` in the LSP.
        standard: Some("2019".to_string()),
        libraries,
        lint: None,
    })
}

/// Same as [`render_vhdl_ls_config`] but converts to the
/// `vhdl_lang::Config` shape the LSP-embed path wants. Round-trips
/// through TOML because `vhdl_lang::LibraryConfig` fields are
/// private and no builder API is exposed.
pub fn render_vhdl_lang_config(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> Result<vhdl_lang::Config> {
    // The LSP wants testbenches resolvable, so it includes bench sources.
    let ls_config = render_vhdl_ls_config(workspace_dir, active_variant, true)?;
    let toml_str = toml::to_string(&ls_config)?;
    vhdl_lang::Config::from_str(&toml_str, workspace_dir.as_std_path()).map_err(
        |e| VwError::Config {
            message: format!("vhdl_lang config parse failed: {e}"),
        },
    )
}

/// Severity of a [`VhdlDiagnostic`] — a `vw`-local mirror of
/// `vhdl_lang::Severity` so callers don't need to depend on
/// `vhdl_lang` themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VhdlSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A single vhdl_lang static-analysis finding, flattened for display.
/// Line/column are 1-based and ready to print.
#[derive(Debug, Clone)]
pub struct VhdlDiagnostic {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub severity: VhdlSeverity,
    pub message: String,
}

/// Git source of the VHDL standard library (`std` / `ieee`).
/// `vhdl_lang` needs these VHDL sources to build its type graph but
/// doesn't bundle them — on a machine with no rust_hdl install, its
/// `load_external_config` search comes up empty and analysis panics on
/// the universal-integer lookup. Rather than vendoring the library, vw
/// materializes it through the normal dependency cache
/// (`~/.vw/deps/rust_hdl-<sha>/vhdl_libraries`) and hands vhdl_lang that
/// path.
///
/// Tracks the default branch (`master`) — the only ref
/// `download_dependency`'s shallow clone can fetch — resolved once at
/// first download. The stdlib is the fixed VHDL standard, so tracking
/// HEAD is safe and independent of the analyzer version; a cached copy
/// is then reused forever with no further network. `vw clear` drops it
/// and the next check re-fetches.
const VHDL_STDLIB_REPO: &str = "https://github.com/vhdl-ls/rust_hdl";
const VHDL_STDLIB_BRANCH: &str = "master";

/// Path to an already-materialized rust_hdl `vhdl_libraries` dir in the
/// dependency cache, if any. The stdlib is stable, so any cached copy
/// is usable — checked with no network so it works offline.
fn find_cached_vhdl_stdlib(deps_dir: &Path) -> Option<Utf8PathBuf> {
    for entry in fs::read_dir(deps_dir).ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("rust_hdl-") {
            continue;
        }
        let libs = entry.path().join("vhdl_libraries");
        let present = libs.exists()
            && fs::read_dir(&libs)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        if present {
            if let Ok(u) = Utf8PathBuf::from_path_buf(libs) {
                return Some(u);
            }
        }
    }
    None
}

/// Ensure the VHDL standard library is present in the dependency cache
/// and return the path to its `vhdl_libraries` dir — ready to hand to
/// [`check_vhdl`] (and, through `load_external_config`, to `vhdl_lang`).
///
/// Reuses any cached copy without touching the network; otherwise
/// downloads [`VHDL_STDLIB_REPO`] on first use through the same
/// `download_dependency` machinery as any other git dependency.
pub async fn ensure_vhdl_stdlib() -> Result<Utf8PathBuf> {
    let deps_dir = deps_directory()?;
    if let Some(libs) = find_cached_vhdl_stdlib(&deps_dir) {
        return Ok(libs);
    }
    let sha = resolve_dependency_commit(
        VHDL_STDLIB_REPO,
        &Some(VHDL_STDLIB_BRANCH.to_string()),
        &None,
        None,
    )
    .await?;
    let dep_path = deps_dir.join(format!("rust_hdl-{sha}"));
    let libs = dep_path.join("vhdl_libraries");
    let present = libs.exists()
        && fs::read_dir(&libs)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    if !present {
        if dep_path.exists() {
            let _ = fs::remove_dir_all(&dep_path);
        }
        download_dependency(
            VHDL_STDLIB_REPO,
            &sha,
            &[],
            &dep_path,
            false,
            &[],
            false,
            None,
            Some("vhdl_libraries"),
        )
        .await?;
    }
    Utf8PathBuf::from_path_buf(libs).map_err(|p| VwError::FileSystem {
        message: format!("VHDL stdlib path is not UTF-8: {}", p.display()),
    })
}

/// Cheap check for whether a workspace renders any VHDL. Gates the
/// (stdlib-fetching) VHDL check so pure-htcl workspaces skip it — and
/// its one-time network download — entirely.
pub fn workspace_has_vhdl(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
) -> bool {
    render_vhdl_ls_config(workspace_dir, active_variant, true)
        .map(|c| c.libraries.values().any(|lib| !lib.files.is_empty()))
        .unwrap_or(false)
}

/// Run vhdl_lang static analysis over the workspace's VHDL — the same
/// analysis `vw-analyzer` runs live in the editor, but in one batch —
/// and return the non-suppressed findings that land in the workspace's
/// OWN tree. Dependency libraries and the bundled VHDL standard library
/// are analyzed for name resolution but their internal diagnostics are
/// filtered out: the user can't fix those and they'd only be noise.
/// Results are sorted by file, then position.
///
/// Cheap no-op returning an empty vec when the workspace renders no
/// VHDL libraries — a pure-htcl workspace has no HDL to check, so we
/// skip building the project (and parsing the standard library) too.
pub fn check_vhdl(
    workspace_dir: &Utf8Path,
    active_variant: Option<&str>,
    stdlib_libraries_path: Option<&Utf8Path>,
) -> Result<Vec<VhdlDiagnostic>> {
    let ls_config = render_vhdl_ls_config(workspace_dir, active_variant, true)?;
    if ls_config.libraries.values().all(|lib| lib.files.is_empty()) {
        return Ok(Vec::new()); // no VHDL to analyze
    }
    let toml_str = toml::to_string(&ls_config)?;
    let user_config =
        vhdl_lang::Config::from_str(&toml_str, workspace_dir.as_std_path())
            .map_err(|e| VwError::Config {
                message: format!("vhdl_lang config parse failed: {e}"),
            })?;

    // Load the VHDL standard library (`std` / `ieee`) via
    // `load_external_config`, then append the workspace's own libraries
    // on top. `stdlib_libraries_path` points at a `vhdl_libraries` dir
    // (vw fetches one into the dep cache — see `ensure_vhdl_stdlib`);
    // `None` falls back to vhdl_lang's built-in search of installed
    // locations. Without `std`, vhdl_lang's type-graph construction
    // panics on the universal-integer lookup — so if it's still missing
    // we skip the VHDL check rather than crash. `vw check` still runs
    // the htcl half.
    let mut messages = vhdl_lang::NullMessages;
    let mut config = vhdl_lang::Config::default();
    config.load_external_config(
        &mut messages,
        stdlib_libraries_path.map(|p| p.to_string()),
    );
    if !config.iter_libraries().any(|lib| lib.name() == "std") {
        return Ok(Vec::new());
    }
    config.append(&user_config, &mut messages);

    let severities = *config.severities();
    let mut project = vhdl_lang::Project::from_config(config, &mut messages);

    // Restrict findings to the workspace's own *source* tree: not deps
    // under `~/.vw/deps`, not the bundled standard library, and not the
    // `target/` build output. Generated RTL (Vivado IP wrappers, BD
    // netlists) is still analyzed so the design's references resolve,
    // but diagnostics INSIDE it are the tool's output, not the user's
    // HDL — reporting them (e.g. an undeclared `UNISIM` in a generated
    // wrapper) is just noise here.
    let ws_root = workspace_dir
        .as_std_path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.as_std_path().to_path_buf());
    let ws_target = ws_root.join("target");

    let mut out: Vec<VhdlDiagnostic> = project
        .analyse()
        .into_iter()
        .filter_map(|d| {
            let severity = severities[d.code]?; // `None` = suppressed
            let file = d.pos.file_name();
            let canon = file.canonicalize();
            let path = canon.as_deref().unwrap_or(file);
            if !path.starts_with(&ws_root) || path.starts_with(&ws_target) {
                return None;
            }
            let start = d.pos.start();
            Some(VhdlDiagnostic {
                file: file.to_path_buf(),
                line: start.line + 1,
                column: start.character + 1,
                severity: match severity {
                    vhdl_lang::Severity::Error => VhdlSeverity::Error,
                    vhdl_lang::Severity::Warning => VhdlSeverity::Warning,
                    vhdl_lang::Severity::Info => VhdlSeverity::Info,
                    vhdl_lang::Severity::Hint => VhdlSeverity::Hint,
                },
                message: d.message,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    Ok(out)
}

/// Resolve which variant the LSP renderer should filter to.
///
/// Precedence:
/// 1. Caller-supplied `active_variant` (e.g. sim invocation).
/// 2. `VW_ACTIVE_VARIANT` env var — the LSP-side selector: users
///    launch `helix` (or a shell) with the var set to switch
///    which board's tree is analyzed. Empty / whitespace-only
///    values are ignored so `unset` and `export FOO=""` behave
///    identically.
/// 3. Workspace's `default = true` variant — the natural fallback
///    (matches `vw run` semantics).
/// 4. `None` — workspace has no variants; caller does no
///    filtering.
fn resolve_active_variant(
    workspace_dir: &Utf8Path,
    caller_supplied: Option<&str>,
) -> Option<String> {
    if let Some(v) = caller_supplied {
        return Some(v.to_string());
    }
    if let Ok(env) = std::env::var("VW_ACTIVE_VARIANT") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let cfg = load_workspace_config(workspace_dir).ok()?;
    if cfg.workspace.variants.is_empty() {
        return None;
    }
    cfg.workspace
        .default_variant()
        .ok()
        .flatten()
        .map(|v| v.name.clone())
}

/// Walk `<ws>/target/vw-project/*/*.gen/sources_1/**/*.{vhd,vhdl}`,
/// keeping only the files vhdl_lang can meaningfully analyze from
/// Vivado's output tree.
///
/// The unfiltered tree has three inclusion hazards:
/// - **Sim/synth duplicates.** Every BD gets a `bd/<name>/synth/<name>.vhd`
///   AND a `bd/<name>/sim/<name>.vhd`, both declaring `entity <name> is`.
///   Including both fires vhdl_lang's duplicate-declaration path plus
///   thousands of Xilinx-specific attribute errors from the sim
///   variant (`entity txr0` alone contributed 614 errors on metroid).
/// - **`ipshared/` shared IP bundles.** Xilinx-provided reusable
///   function sets (`*_rfs.vhd`) use vendor extensions and error out
///   under vhdl_lang.
/// - **`*_sim_netlist.vhdl`.** Post-synth flattened netlists that
///   redeclare the same entity name as the corresponding `_stub`.
///
/// Kept: `bd/*/hdl/*_wrapper.vhd`, `bd/*/synth/**/*.{vhd,vhdl}`,
/// `bd/*/ip/**/synth/**/*.{vhd,vhdl}`, `ip/*/*_stub.{vhd,vhdl}`.
fn vivado_generated_sources(workspace_dir: &Utf8Path) -> Result<Vec<PathBuf>> {
    let project_root = workspace_dir.join("target/vw-project");
    if !project_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(project_root.as_std_path()) else {
        return Ok(Vec::new());
    };
    for entry in entries.flatten() {
        let dir_path = entry.path();
        let Some(name) = dir_path.file_name() else {
            continue;
        };
        let gen_dir = format!("{}.gen", name.to_string_lossy());
        let sources_1 = dir_path.join(gen_dir).join("sources_1");
        if !sources_1.is_dir() {
            continue;
        }
        let mut all = Vec::new();
        find_vhdl_files_impl(&sources_1, &mut all, true)?;
        for path in all {
            if keep_vivado_generated_path(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Filter predicate for [`vivado_generated_sources`]. Only files
/// the user's own code can name are retained; the deeper
/// synthesis-time entities user code never references directly are
/// dropped so vhdl_lang doesn't parse them at all.
///
/// User code references BD components as `<name>_wrapper` (the
/// entity in `bd/<name>/hdl/<name>_wrapper.vhd`) — that wrapper
/// wraps `bd/<name>/synth/<name>.vhd` internally through a
/// component declaration, and the component isn't part of the LSP
/// resolution surface. Include the wrapper, skip the synth entity.
/// Same reasoning drops `bd/<name>/ip/**` sub-IP wrappers and every
/// `ipshared/` bundle: they're compiled by Vivado but never
/// mentioned by user-authored VHDL. XCI IPs like `primary_clock`
/// have the same shape one level up — `ip/<name>/<name>_stub.vhdl`
/// declares the entity user code names.
fn keep_vivado_generated_path(path: &Path) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // BD subtree: only the top-level `hdl/<name>_wrapper.vhd`.
    if let Some(bd_rel) = s
        .split("/sources_1/bd/")
        .nth(1)
        .and_then(|rel| rel.split_once('/'))
    {
        let sub_path = bd_rel.1;
        return sub_path.starts_with("hdl/") && name.ends_with("_wrapper.vhd");
    }
    // XCI IP subtree: only `<name>_stub.{vhd,vhdl}`.
    if s.contains("/sources_1/ip/") {
        return name.ends_with("_stub.vhd") || name.ends_with("_stub.vhdl");
    }
    // Unknown subtree under `sources_1/` — keep by default so
    // future Vivado output shapes aren't silently dropped.
    true
}

/// Strip VHDL line comments (`-- …` to end of line) so a structural
/// scan doesn't trip over the `.vho` template's banner lines (e.g.
/// `------ Begin Cut here for COMPONENT Declaration`, which would
/// otherwise look like a `component` token).
fn strip_vhdl_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rewrite the VHDL *component* declaration Vivado emits in an IP's
/// `.vho` instantiation template into a standalone black-box *entity*
/// plus an empty architecture. A component and an entity share
/// generic/port syntax verbatim, so this is a mechanical splice — no
/// port parsing — which keeps it robust to whatever types the IP's
/// ports use. Returns `None` when no component declaration is found.
///
/// The result declares only the interface; the empty architecture is
/// a black box. It's compiled into `xil_defaultlib` (per
/// `vhdl_ls.toml`) so `entity xil_defaultlib.<ip>` resolves for the
/// static check. It is NOT for synthesis — `vw::synth` uses the real
/// IP netlist.
fn vho_component_to_entity(vho: &str) -> Option<String> {
    let clean = strip_vhdl_line_comments(vho);
    let lower = clean.to_ascii_lowercase();

    // Find the `component` keyword that OPENS the declaration — not the
    // `end component` terminator. Scan for a `component` token whose
    // preceding word isn't `end` and which sits on word boundaries.
    let is_ident_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let decl = {
        let bytes = lower.as_bytes();
        let mut from = 0;
        loop {
            let rel = lower[from..].find("component")?;
            let idx = from + rel;
            let after = idx + "component".len();
            let left_ok = idx == 0 || !is_ident_char(bytes[idx - 1]);
            let right_ok =
                bytes.get(after).map(|b| !is_ident_char(*b)).unwrap_or(true);
            let prev_word_end =
                lower[..idx].split_whitespace().last() != Some("end");
            if left_ok && right_ok && prev_word_end {
                break idx;
            }
            from = after;
        }
    };

    // The IP/entity name is the first identifier after `component`.
    let after_kw = decl + "component".len();
    let rest = &clean[after_kw..];
    let name_off = rest.find(|c: char| !c.is_whitespace())?;
    let name_rest = &rest[name_off..];
    let name_len = name_rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(name_rest.len());
    let name = &name_rest[..name_len];
    if name.is_empty() {
        return None;
    }

    // Body = generics/ports between the name and `end component`.
    let body_start = after_kw + name_off + name_len;
    let term_rel = lower[body_start..].find("end component")?;
    let mut body = clean[body_start..body_start + term_rel].trim();
    // VHDL-2008 allows `component NAME is`; drop a leading `is` so we
    // don't emit `entity NAME is is …`.
    if body.len() >= 2
        && body[..2].eq_ignore_ascii_case("is")
        && body[2..].starts_with(char::is_whitespace)
    {
        body = body[2..].trim_start();
    }

    Some(format!(
        "-- Auto-generated black-box stub for IP `{name}`, derived from\n\
         -- Vivado's VHDL instantiation template (`{name}.vho`) so the\n\
         -- static VHDL check can resolve `entity xil_defaultlib.{name}`.\n\
         -- NOT for synthesis — vw::synth uses the real IP netlist.\n\
         library ieee;\n\
         use ieee.std_logic_1164.all;\n\
         use ieee.numeric_std.all;\n\
         \n\
         entity {name} is\n\
         {body}\n\
         end entity;\n\
         \n\
         architecture stub of {name} is\n\
         begin\n\
         end architecture;\n"
    ))
}

/// Turn each standalone XCI IP's Vivado instantiation template
/// (`<ip>.vho`) into a black-box `<ip>_stub.vhdl` alongside it, so the
/// static VHDL check can resolve `entity xil_defaultlib.<ip>` without
/// the IP ever being synthesized. Scans
/// `target/vw-project/*/*.gen/sources_1/ip/*/` for a top-level `.vho`
/// (the top IP; sub-IP templates in nested dirs are skipped). Writes
/// only when content changed, and is a no-op when there are no
/// templates. Returns how many stubs were (re)written.
pub fn write_ip_stubs_from_templates(
    workspace_dir: &Utf8Path,
) -> Result<usize> {
    let project_root = workspace_dir.join("target/vw-project");
    let Ok(projects) = fs::read_dir(project_root.as_std_path()) else {
        return Ok(0);
    };
    let mut written = 0usize;
    for project in projects.flatten() {
        let name = project.file_name();
        let ip_root = project
            .path()
            .join(format!("{}.gen", name.to_string_lossy()))
            .join("sources_1")
            .join("ip");
        let Ok(ip_dirs) = fs::read_dir(&ip_root) else {
            continue;
        };
        for ip_dir in ip_dirs.flatten() {
            if !ip_dir.path().is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(ip_dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("vho") {
                    continue;
                }
                let Ok(vho) = fs::read_to_string(&path) else {
                    continue;
                };
                let Some(stub) = vho_component_to_entity(&vho) else {
                    continue;
                };
                let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                else {
                    continue;
                };
                let out = ip_dir.path().join(format!("{stem}_stub.vhdl"));
                let unchanged = fs::read_to_string(&out)
                    .map(|c| c == stub)
                    .unwrap_or(false);
                if !unchanged && fs::write(&out, &stub).is_ok() {
                    written += 1;
                }
            }
        }
    }
    Ok(written)
}

fn find_testbench_file_recurse(
    testbench_name: &str,
    bench_dir: &Utf8Path,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<Vec<PathBuf>> {
    let mut found_files = Vec::new();

    for entry in fs::read_dir(bench_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read bench directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_file() {
            if let Some(extension) = path.extension() {
                if extension == "vhd" || extension == "vhdl" {
                    // Check if this file contains the entity we're looking for
                    if file_contains_entity(
                        &path,
                        testbench_name,
                        entities_cache,
                    )? {
                        found_files.push(path);
                    }
                }
            }
        } else if recurse {
            let dir_path: Utf8PathBuf =
                path.try_into().map_err(|e| VwError::FileSystem {
                    message: format!("Failed to get dir path: {e}"),
                })?;
            let mut lower_testbenches = find_testbench_file_recurse(
                testbench_name,
                &dir_path,
                recurse,
                entities_cache,
            )?;
            found_files.append(&mut lower_testbenches);
        }
    }
    Ok(found_files)
}

fn find_testbench_file(
    testbench_name: &str,
    bench_dir: &Utf8Path,
    recurse: bool,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<PathBuf> {
    let found_files = find_testbench_file_recurse(
        testbench_name,
        bench_dir,
        recurse,
        entities_cache,
    )?;

    match found_files.len() {
        0 => Err(VwError::Testbench {
            message: format!("Testbench entity '{testbench_name}' not found in bench directory")
        }),
        1 => Ok(found_files.into_iter().next().unwrap()),
        _ => Err(VwError::Testbench {
            message: format!("Multiple files contain entity '{testbench_name}': {found_files:?}")
        }),
    }
}

fn file_contains_entity(
    file_path: &Path,
    entity_name: &str,
    entities_cache: &mut HashMap<PathBuf, Vec<String>>,
) -> Result<bool> {
    let entities = get_cached_entities(file_path, entities_cache)?;
    Ok(entities.iter().any(|e| e.eq_ignore_ascii_case(entity_name)))
}

/// Get entities from cache, parsing and caching if not present.
fn get_cached_entities<'a>(
    path: &Path,
    entities_cache: &'a mut HashMap<PathBuf, Vec<String>>,
) -> Result<&'a Vec<String>> {
    match entities_cache.entry(path.to_path_buf()) {
        Entry::Occupied(e) => Ok(e.into_mut()),
        Entry::Vacant(e) => {
            let content =
                fs::read_to_string(path).map_err(|e| VwError::FileSystem {
                    message: format!("Failed to read file {path:?}: {e}"),
                })?;
            let entities = parse_entities(&content)?;
            Ok(e.insert(entities))
        }
    }
}

fn extract_repo_name(repo_url: &str) -> String {
    repo_url
        .trim_end_matches(".git")
        .split('/')
        .next_back()
        .unwrap_or("dependency")
        .to_string()
}

fn save_workspace_config(
    workspace_dir: &Utf8Path,
    config: &WorkspaceConfig,
) -> Result<()> {
    let toml_content = toml::to_string_pretty(config)?;
    let config_path = workspace_dir.join("vw.toml");

    fs::write(&config_path, toml_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write vw.toml file: {e}"),
    })?;

    Ok(())
}

/// Walk up from `start` (typically a directory) looking for the
/// first `vw.toml` file in the ancestor chain. Returns the
/// containing directory, or `None` if none is found.
///
/// Callers that hold a FILE path — say `entry.htcl` — should pass
/// `entry.parent()` since a file itself cannot contain a
/// `vw.toml`. The check uses `is_file()` so a stray directory
/// named `vw.toml` doesn't trip a false positive.
///
/// Consolidates three near-duplicate helpers that previously
/// lived in `vw-cli::find_workspace_dir`, `vw-repl::app::
/// find_vw_toml_ancestor`, and `vw-repl::lower::
/// find_workspace_dir`. The `vw-analyzer` multi-root discovery
/// (LSP `initialize`-supplied roots) is a different concept and
/// stays in the analyzer.
/// Return `<workspace_dir>/design.htcl` when it exists on disk,
/// else `None`. Mirrors the `module.htcl` convention for library
/// workspaces — `design.htcl` is the project workspace's entry
/// script, auto-discovered by `vw run` / `vw repl` / `vw check`
/// when the user invokes them with no file argument.
pub fn find_design_file(workspace_dir: &Utf8Path) -> Option<Utf8PathBuf> {
    let p = workspace_dir.join("design.htcl");
    p.is_file().then_some(p)
}

pub fn find_workspace_dir(start: &Path) -> Option<Utf8PathBuf> {
    // Canonicalize UP FRONT so the walk-up starts from an
    // absolute path. Callers hand us relative or empty paths in
    // real workflows: `vw run prime.htcl` derives
    // `Path("prime.htcl").parent() == ""`, which used to yield an
    // empty-string workspace root — served over RPC to htcl,
    // that caused `[file join $root target ip $ip]` to compute a
    // RELATIVE path (`target/ip/cips`), which Vivado created
    // inside its auto-cleaned tempdir cwd. The wrapper was
    // silently written and immediately deleted on process exit.
    // Canonicalizing here fixes every downstream consumer
    // (workspace_root RPC, LSP compat check, htcl-test) in one
    // place. Fallback to the raw start when canonicalize fails
    // (e.g. path doesn't exist yet) so we don't regress the "no
    // workspace" branch for freshly-created files.
    // Empty path is a special case — `Path("").canonicalize()`
    // errors with ENOENT, and `parent()` on it yields None
    // immediately, so we'd terminate the walk before ever
    // checking the cwd. Fold empty → cwd first, then canonicalize.
    let start_pb = if start.as_os_str().is_empty() {
        std::env::current_dir().ok()?
    } else {
        start.to_path_buf()
    };
    let canon = start_pb.canonicalize().unwrap_or(start_pb);
    let mut cur = Utf8PathBuf::from_path_buf(canon).ok()?;
    loop {
        if cur.join("vw.toml").is_file() {
            return Some(cur);
        }
        // `Utf8Path::parent()` returns `None` at the filesystem
        // root; the loop naturally terminates without needing a
        // manual `parent == cur` guard.
        cur = cur.parent()?.to_path_buf();
    }
}

pub fn load_workspace_config(
    workspace_dir: &Utf8Path,
) -> Result<WorkspaceConfig> {
    let config_path = workspace_dir.join("vw.toml");
    if !config_path.exists() {
        return Err(VwError::Config {
            message: format!("No vw.toml file found in {workspace_dir}"),
        });
    }

    let config_content =
        fs::read_to_string(&config_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read vw.toml: {e}"),
        })?;

    let config: WorkspaceConfig = toml::from_str(&config_content)?;
    validate_variant_shape(&config.workspace)?;
    Ok(config)
}

/// Post-deserialize validation for the `[[target-parts]]` /
/// `[[workspace.variants]]` mutual exclusion + variant-name
/// uniqueness. Returns [`VwError::Config`] with the same
/// user-facing message the [`VariantSelectError`] carries so
/// the loader surfaces the specific failure verbatim.
fn validate_variant_shape(ws: &WorkspaceInfo) -> Result<()> {
    if !ws.variants.is_empty() && !ws.target_parts.is_empty() {
        return Err(VwError::Config {
            message: VariantSelectError::BothPartsAndVariants.to_string(),
        });
    }
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::new();
    for v in &ws.variants {
        if !seen.insert(v.name.as_str()) {
            return Err(VwError::Config {
                message: VariantSelectError::DuplicateName {
                    name: v.name.clone(),
                }
                .to_string(),
            });
        }
    }
    Ok(())
}

fn load_lock_file(workspace_dir: &Utf8Path) -> Result<LockFile> {
    let lock_path = workspace_dir.join("vw.lock");
    if !lock_path.exists() {
        return Err(VwError::Config {
            message: format!("No vw.lock file found in {workspace_dir}"),
        });
    }

    let lock_content =
        fs::read_to_string(&lock_path).map_err(|e| VwError::FileSystem {
            message: format!("Failed to read vw.lock: {e}"),
        })?;

    let lock_file: LockFile = toml::from_str(&lock_content)?;

    Ok(lock_file)
}

/// Return the per-user dependency cache directory used by vw.
///
/// Resolved from `$VW_DEPS_DIR` if set, otherwise `$HOME/.vw/deps`.
/// The directory is created if it does not exist. Callers holding
/// relative paths from [`resolve_deps`] or `vw.lock` should join against
/// the value returned here to obtain absolute paths.
pub fn deps_directory() -> Result<PathBuf> {
    let deps_dir = if let Some(override_dir) =
        std::env::var_os("VW_DEPS_DIR").filter(|v| !v.is_empty())
    {
        PathBuf::from(override_dir)
    } else {
        let home_dir = dirs::home_dir().ok_or_else(|| VwError::FileSystem {
            message: "Could not determine home directory".to_string(),
        })?;
        home_dir.join(".vw").join("deps")
    };

    fs::create_dir_all(&deps_dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create dependencies directory: {e}"),
    })?;

    Ok(deps_dir)
}

/// Resolve a path stored in `vw.lock` against the local dependency cache.
///
/// Lock-file dep paths are stored as `<name>-<sha>` (relative to the
/// per-user `$HOME/.vw/deps` directory) so the file is identical across
/// machines. Absolute paths are returned unchanged to remain compatible
/// with lock files written by older versions of vw.
/// Build a `name → absolute cache path` map for every dependency in
/// the workspace's `vw.lock`. Used by htcl's `src @name/...` resolver
/// in `vw-htcl::src_path::Resolver` so the language-layer crate stays
/// free of workspace / lockfile concerns.
///
/// Returns an empty map (not an error) if the workspace has no
/// `vw.lock` yet — relative and absolute `src` imports still work
/// against an empty resolver, only `@name/` lookups fail.
pub fn dep_cache_paths(
    workspace_dir: &Utf8Path,
) -> Result<HashMap<String, PathBuf>> {
    dep_cache_paths_with_test(workspace_dir, false)
}

/// Same as [`dep_cache_paths`] but optionally includes
/// `[test-dependencies]`. Only `vw test` should pass
/// `include_test = true`; other callers see the same map they
/// always did, so `vw run`/`vw check`/`vw update`'s dep behavior
/// is unchanged.
pub fn dep_cache_paths_with_test(
    workspace_dir: &Utf8Path,
    include_test: bool,
) -> Result<HashMap<String, PathBuf>> {
    let mut out = HashMap::new();

    // Local deps live wherever `vw.toml` says; they don't need a
    // lockfile (nothing to pin). Read them straight from the workspace
    // config so they work before — or without — a `vw update`.
    if let Ok(config) = load_workspace_config(workspace_dir) {
        for (name, dep) in config.dependencies {
            if let Some(path) = dep.local_path() {
                out.insert(name, resolve_local_dep_path(workspace_dir, path));
            }
        }
        if include_test {
            for (name, dep) in config.test_dependencies {
                if let Some(path) = dep.local_path() {
                    out.insert(
                        name,
                        resolve_local_dep_path(workspace_dir, path),
                    );
                }
            }
        }
    }

    // Git deps are resolved through the lockfile and the per-user
    // cache. A missing lock isn't an error here — just skip git entries.
    // The lockfile stores test-dep and normal-dep entries in the same
    // `dependencies` section (no separate section for locks — see the
    // rationale on `update_workspace_with_token`). Non-test callers
    // want to filter out entries that came exclusively from
    // `[test-dependencies]`; simplest safe rule for now: everything in
    // the lockfile is exposed regardless of section. A subsequent PR
    // can add a `test = true` marker per lock entry if this becomes a
    // real concern.
    match load_lock_file(workspace_dir) {
        Ok(lock) => {
            for (name, locked) in lock.dependencies {
                // A manifest path dep already claimed this name. The
                // manifest is authoritative for a dependency's KIND, so
                // a stale git lock entry left over from a `repo → path`
                // switch must not override it (the reason the `out`
                // insert order matters). The lock is rewritten to drop
                // the entry on the next re-resolve — see
                // `lock_is_stale_against_manifest` — but resolution has
                // to be correct even before that runs.
                if out.contains_key(&name) {
                    continue;
                }
                let abs = resolve_dep_path(&locked.path)?;
                out.insert(name, abs);
            }
        }
        Err(VwError::Config { .. }) => {}
        Err(e) => return Err(e),
    }

    Ok(out)
}

/// Drop `vw.lock` entries the manifest now declares as **path** deps —
/// the stale git pin left behind by a `repo → path` switch. Path deps
/// are never locked (nothing to pin), so the correct lock simply omits
/// them. Rewrites the lock in place when it changed; returns whether it
/// did (so the caller can report it).
///
/// Deliberately surgical: it removes only the now-path entries and
/// leaves every other (git) pin untouched. A full re-resolve would hit
/// the network and re-pin every branch-tracking git dep to its current
/// HEAD — a `repo → path` edit must not silently bump unrelated
/// dependencies. The reverse switch (`path → repo`) needs no handling
/// here: it leaves the manifest dep git-shaped and unlocked, which
/// [`dependencies_present`] already treats as "fetch me".
pub fn prune_stale_path_deps_from_lock(
    workspace_dir: &Utf8Path,
) -> Result<bool> {
    let Ok(mut lock) = load_lock_file(workspace_dir) else {
        return Ok(false);
    };
    let Ok(config) = load_workspace_config(workspace_dir) else {
        return Ok(false);
    };
    let path_dep_names: std::collections::HashSet<String> = config
        .dependencies
        .iter()
        .chain(config.test_dependencies.iter())
        .filter(|(_, dep)| dep.local_path().is_some())
        .map(|(name, _)| name.clone())
        .collect();
    let before = lock.dependencies.len();
    lock.dependencies
        .retain(|name, _| !path_dep_names.contains(name));
    if lock.dependencies.len() == before {
        return Ok(false);
    }
    write_lock_file(workspace_dir, &lock)?;
    Ok(true)
}

fn resolve_dep_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let deps_dir = deps_directory()?;
    Ok(deps_dir.join(path))
}

/// Resolve a `path = "..."` dependency to an absolute, canonicalized
/// root. Relative paths resolve against the workspace that DECLARES
/// them (the same Cargo rule [`resolve_dep_source_path`] uses), so a
/// dep buried in a transitive workspace — e.g. an in-tree `testlib`
/// that declares `vw = ".."` — points at the real directory instead of
/// leaking its literal `..` into the import resolver. Canonicalizing
/// also lets the transitive walk dedup a circular dep (vw → testlib →
/// vw) by real path rather than looping on `<vw>/testlib/..`.
fn resolve_local_dep_path(workspace_dir: &Utf8Path, path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_dir.as_std_path().join(path)
    };
    abs.canonicalize().unwrap_or(abs)
}

/// Like [`dep_cache_paths`], but walks the dependency graph
/// transitively: for every dep whose cached root is itself a
/// workspace (i.e. has its own `vw.toml`), pull in *its* deps too,
/// and so on. The result is a flat `name → root` map covering every
/// dep any file in this workspace's transitive closure might
/// `src @<name>/...`-import.
///
/// First-seen-wins on name conflicts so the entry workspace's
/// declarations take precedence over a dep's choice of the same
/// name (matching Cargo's resolution: the top-level `Cargo.toml`
/// pins the version for the whole graph).
///
/// Returns an empty map (not an error) if the entry workspace has
/// no deps. Per-dep failures (missing `vw.toml`, malformed config)
/// are skipped: a dep may not be its own htcl workspace, and that's
/// fine — we just won't see *its* deps.
pub fn transitive_dep_cache_paths(
    entry_workspace_dir: &Utf8Path,
) -> Result<HashMap<String, PathBuf>> {
    transitive_dep_cache_paths_with_test(entry_workspace_dir, false)
}

/// Like [`transitive_dep_cache_paths`] but optionally includes
/// the ENTRY workspace's `[test-dependencies]`. Cargo-parity
/// semantic for `dev-dependencies`: test-deps are private to the
/// workspace that declares them. Recursed-into workspaces are
/// walked with `include_test = false` so a dep's own test-deps
/// aren't pulled into your consumer.
pub fn transitive_dep_cache_paths_with_test(
    entry_workspace_dir: &Utf8Path,
    include_test: bool,
) -> Result<HashMap<String, PathBuf>> {
    let mut out: HashMap<String, PathBuf> = HashMap::new();
    let mut visited: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    // Only the entry workspace's paths are gathered with
    // `include_test`; everything queued after gets the normal
    // treatment.
    let mut first_iter = true;
    // Canonicalize the entry so it dedups against the canonicalized
    // dep roots `resolve_local_dep_path` produces — otherwise a
    // circular local dep (vw → testlib → vw) reappears as
    // `<vw>/testlib/..` and the walk never converges.
    let entry = entry_workspace_dir.as_std_path().to_path_buf();
    let entry = entry.canonicalize().unwrap_or(entry);
    let mut queue: Vec<PathBuf> = vec![entry];

    while let Some(ws) = queue.pop() {
        if !visited.insert(ws.clone()) {
            continue;
        }
        let Ok(ws_utf8) = Utf8PathBuf::from_path_buf(ws) else {
            continue;
        };
        let want_test = include_test && first_iter;
        first_iter = false;
        let Ok(paths) = dep_cache_paths_with_test(&ws_utf8, want_test) else {
            continue;
        };
        for (name, dep_path) in paths {
            // First-seen wins — don't let a transitive dep override
            // the entry workspace's choice.
            out.entry(name).or_insert_with(|| dep_path.clone());
            // If the dep is itself a workspace, recurse into it. A
            // dep without a `vw.toml` is a leaf (just files).
            if dep_path.join("vw.toml").exists() {
                queue.push(dep_path);
            }
        }
    }
    Ok(out)
}

async fn resolve_dependency_commit(
    repo_url: &str,
    branch: &Option<String>,
    commit: &Option<String>,
    credentials: Option<(&str, &str)>, // (username, password)
) -> Result<String> {
    match (branch, commit) {
        (Some(_), Some(_)) => Err(VwError::Config {
            message: "Cannot specify both branch and commit for dependency"
                .to_string(),
        }),
        (None, None) => Err(VwError::Config {
            message: "Must specify either branch or commit for dependency"
                .to_string(),
        }),
        (None, Some(commit)) => Ok(commit.clone()),
        (Some(branch), None) => {
            get_branch_head_commit(repo_url, branch, credentials).await
        }
    }
}

async fn get_branch_head_commit(
    repo_url: &str,
    branch: &str,
    credentials: Option<(&str, &str)>, // (username, password)
) -> Result<String> {
    // Normalize repository URL to ensure it ends with .git for GitHub
    let normalized_repo_url =
        if repo_url.contains("github.com") && !repo_url.ends_with(".git") {
            format!("{repo_url}.git")
        } else {
            repo_url.to_string()
        };

    let branch = branch.to_string();
    let credentials = credentials.map(|(u, p)| (u.to_string(), p.to_string()));

    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            // Create a temporary directory for the operation
            let temp_dir =
                tempfile::tempdir().map_err(|e| VwError::FileSystem {
                    message: format!(
                        "Failed to create temporary directory: {e}"
                    ),
                })?;

            // Create an empty repository to work with remotes
            let repo =
                git2::Repository::init_bare(temp_dir.path()).map_err(|e| {
                    VwError::Git {
                        message: format!(
                            "Failed to initialize temporary repository: {e}"
                        ),
                    }
                })?;

            // Create a remote
            let mut remote = repo
                .remote_anonymous(&normalized_repo_url)
                .map_err(|e| VwError::Git {
                    message: format!("Failed to create remote: {e}"),
                })?;

            // Connect and list references
            // Always set a credentials callback so git2 doesn't fail with "no callback set".
            // The callback will try explicit credentials first, then fall back to git's
            // credential helper system (which includes .netrc support).
            let mut callbacks = git2::RemoteCallbacks::new();
            let attempt_count = RefCell::new(0);

            callbacks.credentials(
                move |url, username_from_url, allowed_types| {
                    let mut attempts = attempt_count.borrow_mut();
                    *attempts += 1;

                    // Limit attempts to prevent infinite loops
                    if *attempts > 1 {
                        return git2::Cred::default();
                    }

                    // First, try explicit credentials from netrc if available
                    if allowed_types
                        .contains(git2::CredentialType::USER_PASS_PLAINTEXT)
                    {
                        if let Some((ref username, ref password)) = credentials
                        {
                            // Use both username and password from netrc
                            return git2::Cred::userpass_plaintext(
                                username, password,
                            );
                        }
                    }

                    // Try SSH key if available
                    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                        if let Some(username) = username_from_url {
                            if let Ok(cred) =
                                git2::Cred::ssh_key_from_agent(username)
                            {
                                return Ok(cred);
                            }
                        }
                    }

                    // Fall back to git's credential helper system (includes .netrc)
                    if let Ok(config) = git2::Config::open_default() {
                        if let Ok(cred) = git2::Cred::credential_helper(
                            &config,
                            url,
                            username_from_url,
                        ) {
                            return Ok(cred);
                        }
                    }

                    git2::Cred::default()
                },
            );

            remote
                .connect_auth(git2::Direction::Fetch, Some(callbacks), None)
                .map_err(|e| VwError::Git {
                    message: format!("Failed to connect to remote: {e}"),
                })?;

            let refs = remote.list().map_err(|e| VwError::Git {
                message: format!("Failed to list remote references: {e}"),
            })?;

            // Look for the specific branch reference
            let ref_name = format!("refs/heads/{branch}");
            for remote_head in refs {
                if remote_head.name() == ref_name {
                    return Ok(remote_head.oid().to_string());
                }
            }

            Err(VwError::Git {
                message: format!(
                    "Branch '{branch}' not found in remote repository"
                ),
            })
        }),
    )
    .await
    .map_err(|_| VwError::Git {
        message: "Git ls-remote timed out after 30 seconds".to_string(),
    })?
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git ls-remote task: {e}"),
    })?
}

#[allow(clippy::too_many_arguments)]
async fn download_dependency(
    repo_url: &str,
    commit: &str,
    src_paths: &[String],
    dest_path: &Path,
    recursive: bool,
    exclude: &[String],
    submodules: bool,
    credentials: Option<(&str, &str)>, // (username, password)
    // When `Some`, materialize ONLY this subdirectory of the checkout
    // (structure-preserving, all file types) instead of the whole tree
    // — lets us pull just `vhdl_libraries/` out of the large rust_hdl
    // repo rather than caching its entire source.
    subdir: Option<&str>,
) -> Result<()> {
    let temp_dir = tempfile::tempdir().map_err(|e| VwError::FileSystem {
        message: format!("Failed to create temporary directory: {e}"),
    })?;

    // Normalize repository URL to ensure it ends with .git for GitHub
    let normalized_repo_url =
        if repo_url.contains("github.com") && !repo_url.ends_with(".git") {
            format!("{repo_url}.git")
        } else {
            repo_url.to_string()
        };

    let commit = commit.to_string();
    let temp_path = temp_dir.path().to_path_buf();
    let src_paths = src_paths.to_vec();
    let credentials = credentials.map(|(u, p)| (u.to_string(), p.to_string()));

    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::task::spawn_blocking(move || {
            // Always set a credentials callback so git2 doesn't fail with "no callback set".
            // The callback will try explicit credentials first, then fall back to git's
            // credential helper system (which includes .netrc support).
            let mut callbacks = git2::RemoteCallbacks::new();
            let attempt_count = RefCell::new(0);

            callbacks.credentials(
                move |url, username_from_url, allowed_types| {
                    let mut attempts = attempt_count.borrow_mut();
                    *attempts += 1;

                    // Limit attempts to prevent infinite loops
                    if *attempts > 1 {
                        return git2::Cred::default();
                    }

                    // First, try explicit credentials from netrc if available
                    if allowed_types
                        .contains(git2::CredentialType::USER_PASS_PLAINTEXT)
                    {
                        if let Some((ref username, ref password)) = credentials
                        {
                            // Use both username and password from netrc
                            return git2::Cred::userpass_plaintext(
                                username, password,
                            );
                        }
                    }

                    // Try SSH key if available
                    if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                        if let Some(username) = username_from_url {
                            if let Ok(cred) =
                                git2::Cred::ssh_key_from_agent(username)
                            {
                                return Ok(cred);
                            }
                        }
                    }

                    // Fall back to git's credential helper system (includes .netrc)
                    if let Ok(config) = git2::Config::open_default() {
                        if let Ok(cred) = git2::Cred::credential_helper(
                            &config,
                            url,
                            username_from_url,
                        ) {
                            return Ok(cred);
                        }
                    }

                    git2::Cred::default()
                },
            );

            // Parse the commit SHA before reaching the network, so a typo in
            // `vw.lock` is not reported as a download failure.
            let commit_oid =
                git2::Oid::from_str(&commit).map_err(|e| VwError::Git {
                    message: format!("Invalid commit SHA '{commit}': {e}"),
                })?;

            // Ask for the commit itself rather than cloning a branch and
            // hoping the commit is on the end of it.
            //
            // Cloning at `depth(1)` gets exactly one commit — the tip of the
            // default branch — so any pin that is not currently that tip is
            // absent from the result, and the checkout below fails with
            // "object not found". Every dependency here is pinned by
            // `vw.lock`, and upstream moves on, so that described most of
            // them: what made it look like it worked was resolving each
            // branch to its HEAD immediately before downloading it, which
            // meant the pin was always the tip by construction.
            //
            // Fetching the object by name is both correct and no more
            // expensive; the server sends the history it needs to and
            // nothing else.
            let repo = git2::Repository::init(&temp_path).map_err(|e| {
                VwError::Git {
                    message: format!("Failed to create repository: {e}"),
                }
            })?;
            let mut remote = repo
                .remote_anonymous(&normalized_repo_url)
                .map_err(|e| VwError::Git {
                    message: format!("Failed to add remote: {e}"),
                })?;

            let mut fetch_options = git2::FetchOptions::new();
            fetch_options.depth(1); // just this commit, not its history
            fetch_options.remote_callbacks(callbacks);

            remote
                .fetch(&[commit.as_str()], Some(&mut fetch_options), None)
                .map_err(|e| VwError::Git {
                    message: format!(
                        "Failed to fetch commit '{commit}' from \
                         {normalized_repo_url}: {e}"
                    ),
                })?;

            // Find the commit object
            let commit_obj =
                repo.find_commit(commit_oid).map_err(|e| VwError::Git {
                    message: format!(
                        "Commit '{commit}' is not in {normalized_repo_url}. \
                         The repository may have been rewritten since \
                         vw.lock recorded it; `vw update` will re-resolve it. \
                         ({e})"
                    ),
                })?;

            // Checkout the specific commit.
            //
            // Forced because the fetch leaves an empty working tree with no
            // HEAD to be safe about: a default checkout has nothing to
            // compare against and writes nothing.
            let mut checkout = git2::build::CheckoutBuilder::new();
            checkout.force();
            repo.checkout_tree(commit_obj.as_object(), Some(&mut checkout))
                .map_err(|e| VwError::Git {
                    message: format!(
                        "Failed to checkout commit '{commit}': {e}"
                    ),
                })?;

            // Set HEAD to the commit
            repo.set_head_detached(commit_oid)
                .map_err(|e| VwError::Git {
                    message: format!(
                        "Failed to set HEAD to commit '{commit}': {e}"
                    ),
                })?;

            // Initialize and update submodules if requested
            if submodules {
                for mut submodule in
                    repo.submodules().map_err(|e| VwError::Git {
                        message: format!("Failed to list submodules: {e}"),
                    })?
                {
                    submodule.init(false).map_err(|e| VwError::Git {
                        message: format!(
                            "Failed to init submodule '{}': {e}",
                            submodule.name().unwrap_or("unknown")
                        ),
                    })?;
                    submodule.update(true, None).map_err(|e| VwError::Git {
                        message: format!(
                            "Failed to update submodule '{}': {e}",
                            submodule.name().unwrap_or("unknown")
                        ),
                    })?;
                }
            }

            Ok::<(), VwError>(())
        }),
    )
    .await
    .map_err(|_| VwError::Git {
        message: "Git clone timed out after 120 seconds".to_string(),
    })?
    .map_err(|e| VwError::Git {
        message: format!("Failed to execute git operations: {e}"),
    })??;

    fs::create_dir_all(dest_path).map_err(|e| VwError::FileSystem {
        message: format!("Failed to create destination directory: {e}"),
    })?;

    if let Some(subdir) = subdir {
        // Subtree dependency: copy just `<checkout>/<subdir>` into
        // `<dest>/<subdir>`, preserving structure and every file type.
        copy_module_tree(
            &temp_dir.path().join(subdir),
            &dest_path.join(subdir),
            exclude,
        )?;
    } else if src_paths.is_empty() {
        // Htcl module dependency: no VHDL `src` globs are declared, so
        // the dep publishes its WHOLE module tree — `module.htcl` plus
        // everything it `src`s and any shipped assets (e.g. a
        // `vivado-shim.tcl`). Materialize the full checkout into the
        // cache, preserving directory structure. Without this the
        // VHDL-only copy below matches nothing and leaves the cache
        // dir empty, so `src @<dep>` can't find `<dep>/module.htcl`.
        copy_module_tree(temp_dir.path(), dest_path, exclude)?;
    } else {
        // VHDL source dependency: copy the declared `src` globs,
        // filtered to VHDL and flattened relative to each prefix.
        for src_path in &src_paths {
            copy_vhdl_files_glob(
                temp_dir.path(),
                src_path,
                dest_path,
                recursive,
                exclude,
            )?;
        }
    }

    Ok(())
}

/// Copy an entire checked-out repo tree from `src_root` into `dest`,
/// preserving directory structure. Used for htcl module dependencies
/// (empty `src`), which publish their whole tree rather than a
/// filtered set of VHDL sources — see the call site in
/// [`download_dependency`]. Skips the `.git` metadata dir and honors
/// structure-relative `exclude` globs.
fn copy_module_tree(
    src_root: &Path,
    dest: &Path,
    exclude: &[String],
) -> Result<()> {
    let exclude_patterns: Vec<glob::Pattern> = exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    copy_module_tree_impl(src_root, src_root, dest, &exclude_patterns)
}

fn copy_module_tree_impl(
    root: &Path,
    dir: &Path,
    dest: &Path,
    exclude: &[glob::Pattern],
) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read directory {dir:?}: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        // Never materialize VCS metadata (at any depth — submodules
        // carry their own `.git` file/dir).
        if entry.file_name() == std::ffi::OsStr::new(".git") {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();
        if exclude.iter().any(|p| p.matches(&rel_str)) {
            continue;
        }
        if path.is_dir() {
            copy_module_tree_impl(root, &path, dest, exclude)?;
        } else if path.is_file() {
            let dest_file = dest.join(rel);
            if let Some(parent) = dest_file.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    VwError::FileSystem {
                        message: format!(
                            "Failed to create directory {parent:?}: {e}"
                        ),
                    }
                })?;
            }
            fs::copy(&path, &dest_file).map_err(|e| VwError::FileSystem {
                message: format!(
                    "Failed to copy {path:?} to {dest_file:?}: {e}"
                ),
            })?;
        }
    }
    Ok(())
}

fn copy_vhdl_files_glob(
    repo_root: &Path,
    src_pattern: &str,
    dest: &Path,
    recursive: bool,
    exclude: &[String],
) -> Result<()> {
    // Build patterns to match
    let src_path = repo_root.join(src_pattern);
    let mut patterns = Vec::new();
    let strip_prefix: PathBuf;

    // Compile exclude patterns
    let exclude_patterns: Vec<glob::Pattern> = exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    // Check if src_pattern points to a directory
    if src_path.is_dir() {
        // It's a directory - create appropriate glob patterns
        let base_pattern =
            src_path.to_str().ok_or_else(|| VwError::FileSystem {
                message: "Invalid UTF-8 in path".to_string(),
            })?;

        if recursive {
            // Recursively find all VHDL files
            patterns.push(format!("{base_pattern}/**/*.vhd"));
            patterns.push(format!("{base_pattern}/**/*.vhdl"));
        } else {
            // Only files directly in the directory
            patterns.push(format!("{base_pattern}/*.vhd"));
            patterns.push(format!("{base_pattern}/*.vhdl"));
        }
        // For directories, strip the src directory from paths
        strip_prefix = src_path;
    } else if src_path.is_file() {
        // It's a single file - use as-is
        patterns.push(
            src_path
                .to_str()
                .ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in path".to_string(),
                })?
                .to_string(),
        );
        // For single files, strip the parent directory
        strip_prefix = src_path
            .parent()
            .ok_or_else(|| VwError::FileSystem {
                message: "File has no parent directory".to_string(),
            })?
            .to_path_buf();
    } else {
        // It's a glob pattern or doesn't exist yet - use as-is
        patterns.push(
            src_path
                .to_str()
                .ok_or_else(|| VwError::FileSystem {
                    message: "Invalid UTF-8 in glob pattern path".to_string(),
                })?
                .to_string(),
        );
        // For glob patterns, strip the repo root to preserve relative structure
        strip_prefix = repo_root.to_path_buf();
    }

    let mut copied_count = 0;
    for pattern_str in &patterns {
        // Use glob to find matching files
        let entries =
            glob::glob(pattern_str).map_err(|e| VwError::FileSystem {
                message: format!("Invalid glob pattern '{pattern_str}': {e}"),
            })?;

        for entry in entries {
            let path = entry.map_err(|e| VwError::FileSystem {
                message: format!("Error reading glob entry: {e}"),
            })?;

            // Only copy VHDL files
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "vhd" || ext == "vhdl" {
                        // Compute relative path based on strip_prefix
                        let relative_path =
                            path.strip_prefix(&strip_prefix).map_err(|e| {
                                VwError::FileSystem {
                                    message: format!(
                                    "Failed to compute relative path for {path:?}: {e}"
                                ),
                                }
                            })?;

                        // Check if file matches any exclude pattern
                        let path_str = relative_path.to_string_lossy();
                        if exclude_patterns.iter().any(|p| p.matches(&path_str))
                        {
                            continue; // Skip excluded files
                        }

                        let dest_file = dest.join(relative_path);

                        // Create parent directories if needed
                        if let Some(parent) = dest_file.parent() {
                            fs::create_dir_all(parent).map_err(|e| {
                                VwError::FileSystem {
                                    message: format!(
                                        "Failed to create directory {parent:?}: {e}"
                                    ),
                                }
                            })?;
                        }

                        fs::copy(&path, &dest_file).map_err(|e| {
                            VwError::FileSystem {
                                message: format!(
                                    "Failed to copy file {path:?}: {e}"
                                ),
                            }
                        })?;
                        copied_count += 1;
                    }
                }
            }
        }
    }

    if copied_count == 0 {
        return Err(VwError::Dependency {
            message: format!("No VHDL files matched pattern '{src_pattern}'"),
        });
    }

    Ok(())
}

fn find_vhdl_files(
    dir: &Path,
    recursive: bool,
    exclude: &[String],
) -> Result<Vec<PathBuf>> {
    let mut vhdl_files = Vec::new();
    find_vhdl_files_impl(dir, &mut vhdl_files, recursive)?;

    // Filter out excluded files
    if !exclude.is_empty() {
        let exclude_patterns: Vec<glob::Pattern> = exclude
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        vhdl_files.retain(|file| {
            // Match against path relative to the base directory
            let relative = file.strip_prefix(dir).unwrap_or(file);
            let path_str = relative.to_string_lossy();
            !exclude_patterns
                .iter()
                .any(|pattern| pattern.matches(&path_str))
        });
    }

    Ok(vhdl_files)
}

fn find_vhdl_files_impl(
    dir: &Path,
    vhdl_files: &mut Vec<PathBuf>,
    recursive: bool,
) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| VwError::FileSystem {
        message: format!("Failed to read directory: {e}"),
    })? {
        let entry = entry.map_err(|e| VwError::FileSystem {
            message: format!("Failed to read directory entry: {e}"),
        })?;
        let path = entry.path();

        if path.is_dir() {
            if recursive {
                find_vhdl_files_impl(&path, vhdl_files, recursive)?;
            }
        } else if let Some(extension) =
            path.extension().and_then(|ext| ext.to_str())
        {
            if extension == "vhd" || extension == "vhdl" {
                vhdl_files.push(path);
            }
        }
    }
    Ok(())
}

fn write_lock_file(
    workspace_dir: &Utf8Path,
    lock_file: &LockFile,
) -> Result<()> {
    let toml_content = toml::to_string_pretty(lock_file)?;
    let lock_path = workspace_dir.join("vw.lock");

    fs::write(&lock_path, toml_content).map_err(|e| VwError::FileSystem {
        message: format!("Failed to write vw.lock file: {e}"),
    })?;

    Ok(())
}

/// Build a Rust library for a testbench.
/// Looks for Cargo.toml in the testbench directory, builds it, and returns the path to the .so file.
async fn build_rust_library(
    bench_dir: &Utf8Path,
    testbench_file: &Path,
) -> Result<PathBuf> {
    // Get the testbench directory
    let testbench_dir =
        testbench_file.parent().ok_or_else(|| VwError::Testbench {
            message: format!(
                "Testbench file {:?} has no parent directory???",
                testbench_file
            ),
        })?;

    // Look for Cargo.toml in the testbench directory
    let cargo_toml_path = testbench_dir.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        return Err(VwError::Testbench {
            message: format!(
                "Cargo.toml not found in testbench directory: {:?}",
                testbench_dir
            ),
        });
    }

    // Parse Cargo.toml to get the package name
    let cargo_toml_content =
        fs::read_to_string(&cargo_toml_path).map_err(|e| {
            VwError::FileSystem {
                message: format!("Failed to read Cargo.toml: {e}"),
            }
        })?;

    let cargo_toml: CargoToml = toml::from_str(&cargo_toml_content)?;
    let package_name = cargo_toml.package.name;

    // Run cargo build in the testbench directory
    let testbench_dir_owned = testbench_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let output = cargo_command()
            .arg("build")
            .current_dir(&testbench_dir_owned)
            .output()
            .map_err(|e| VwError::Testbench {
                message: format!("Failed to execute cargo build: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VwError::Testbench {
                message: format!("cargo build failed:\n{stderr}"),
            });
        }

        Ok::<(), VwError>(())
    })
    .await
    .map_err(|e| VwError::Testbench {
        message: format!("Failed to execute cargo build task: {e}"),
    })??;

    // Find the .so file in the workspace target directory (parent of testbench dir)
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let lib_name = format!("lib{}.{ext}", package_name.replace('-', "_"));
    let workspace_target = bench_dir.join("target").join("debug");

    let lib_path = workspace_target.join(&lib_name);

    if !lib_path.exists() {
        return Err(VwError::Testbench {
            message: format!(
                "Built Rust library not found at expected path: {:?}",
                lib_path
            ),
        });
    }

    Ok(lib_path.into())
}

#[cfg(test)]
mod locked_resolution_tests {
    use super::*;

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";

    /// Point the dependency cache at a scratch directory, once for the whole
    /// test binary so no test ever sees the variable change underneath it.
    /// Cache directories are keyed `<name>-<sha>`, so tests stay out of each
    /// other's way by using distinct dependency names.
    fn deps_cache() -> &'static Utf8Path {
        static CACHE: std::sync::OnceLock<Utf8PathBuf> =
            std::sync::OnceLock::new();
        CACHE.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap().keep();
            let dir = Utf8PathBuf::from_path_buf(dir).unwrap();
            std::env::set_var("VW_DEPS_DIR", dir.as_str());
            dir
        })
    }

    /// A dependency already downloaded: what every CI worker's cache looks
    /// like once the first job in the pipeline has run.
    fn seed_cache(name: &str, sha: &str) {
        let root = deps_cache().join(format!("{name}-{sha}"));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("vw.toml"),
            format!("[workspace]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
    }

    /// A workspace whose only dependency is `name`, declared and locked as
    /// given. `lock` is `(repo, branch, commit)`, where a branch of `None`
    /// writes the pre-`branch` lock format still sitting in checked-in
    /// lockfiles; `lock` of `None` writes no `vw.lock` at all.
    fn workspace(
        dir: &Utf8Path,
        name: &str,
        declared: &str,
        lock: Option<(&str, Option<&str>, &str)>,
    ) {
        fs::write(
            dir.join("vw.toml"),
            format!(
                "[workspace]\nname = \"entry\"\nversion = \"0.1.0\"\n\
                 [dependencies.{name}]\n{declared}"
            ),
        )
        .unwrap();
        if let Some((repo, branch, commit)) = lock {
            let branch = branch
                .map(|b| format!("branch = \"{b}\"\n"))
                .unwrap_or_default();
            fs::write(
                dir.join("vw.lock"),
                format!(
                    "[dependencies.{name}]\nrepo = \"{repo}\"\n{branch}\
                     commit = \"{commit}\"\npath = \"{name}-{commit}\"\n"
                ),
            )
            .unwrap();
        }
    }

    /// The commit `name` resolved to in `graph`.
    fn resolved(graph: &DiGraph<DepGraphNode, ()>, name: &str) -> String {
        graph
            .node_indices()
            .map(|i| &graph[i])
            .find(|n| n.name.as_deref() == Some(name))
            .expect("the dependency is in the graph")
            .locked
            .as_ref()
            .expect("a git dependency is pinned")
            .commit
            .clone()
    }

    /// The bug this all exists for: a branch dependency that gained a commit
    /// partway through a CI run must not move under the jobs still to come.
    /// The remote here does not exist, so reaching for it at all would fail.
    ///
    /// Locked here in the pre-`branch` format, which is what every lockfile
    /// committed before that field existed looks like — including the one
    /// this bug was reported against.
    #[tokio::test]
    async fn a_locked_branch_dependency_is_not_re_resolved() {
        deps_cache();
        seed_cache("lockfix-quiet", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-quiet",
            "repo = \"https://example.invalid/quiet.git\"\n\
             branch = \"main\"\n",
            Some(("https://example.invalid/quiet.git", None, SHA_A)),
        );

        let graph = build_dependency_graph(ws, false, None, Resolution::Locked)
            .await
            .expect("a locked dependency resolves without the network");

        let dep = graph
            .node_indices()
            .map(|i| &graph[i])
            .find(|n| n.name.as_deref() == Some("lockfix-quiet"))
            .expect("the dependency is in the graph");
        assert_eq!(dep.locked.as_ref().unwrap().commit, SHA_A);
        assert!(dep.was_cached, "the seeded cache dir was reused");
        assert_eq!(
            dep.locked.as_ref().unwrap().branch.as_deref(),
            Some("main"),
            "the branch is recorded, so the next fetch can tell it changed"
        );
    }

    /// Move a dependency to another branch and its old pin is an answer to a
    /// question nobody is asking any more — even though the name, the repo,
    /// and the cached tree are all still there.
    #[tokio::test]
    async fn a_lock_entry_for_another_branch_does_not_pin() {
        deps_cache();
        seed_cache("lockfix-branch", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-branch",
            "repo = \"https://example.invalid/branch.git\"\n\
             branch = \"feature\"\n",
            Some(("https://example.invalid/branch.git", Some("main"), SHA_A)),
        );

        let e = build_dependency_graph(ws, false, None, Resolution::Locked)
            .await
            .expect_err("the pin from the old branch is not reused");
        assert!(
            e.to_string().contains("resolve commit for dependency"),
            "expected a failed branch resolution, got: {e}"
        );
    }

    /// A commit written by hand in `vw.toml` is the most specific thing
    /// anyone has said about the dependency, so it outranks a lock entry that
    /// disagrees — which is what an edited pin looks like until the next
    /// fetch catches up.
    #[tokio::test]
    async fn a_manifest_commit_outranks_a_stale_lock_entry() {
        deps_cache();
        seed_cache("lockfix-pinned", SHA_B);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-pinned",
            &format!(
                "repo = \"https://example.invalid/pinned.git\"\n\
                 commit = \"{SHA_B}\"\n"
            ),
            Some(("https://example.invalid/pinned.git", None, SHA_A)),
        );

        let graph = build_dependency_graph(ws, false, None, Resolution::Locked)
            .await
            .expect("an explicitly pinned dependency needs no remote");
        assert_eq!(resolved(&graph, "lockfix-pinned"), SHA_B);
    }

    /// The gate and the pin have to agree. If `vw check` decides everything
    /// is present, no fetch runs at all and the altered declaration is never
    /// looked at — so the same drift that rejects a pin must also fail the
    /// gate, and agreement must leave it alone.
    #[test]
    fn a_changed_declaration_fails_the_present_check() {
        deps_cache();
        seed_cache("lockfix-gate", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        let declared = "repo = \"https://example.invalid/gate.git\"\n\
                        branch = \"feature\"\n";
        let repo = "https://example.invalid/gate.git";

        // Agrees: the tree is cached and the entry answers the declaration.
        workspace(
            ws,
            "lockfix-gate",
            declared,
            Some((repo, Some("feature"), SHA_A)),
        );
        assert!(dependencies_present(ws), "nothing to do");
        assert!(!workspace_has_unlocked_git_deps(ws, true).unwrap());

        // Branch moved on.
        workspace(
            ws,
            "lockfix-gate",
            declared,
            Some((repo, Some("main"), SHA_A)),
        );
        assert!(!dependencies_present(ws), "the branch changed");
        assert!(workspace_has_unlocked_git_deps(ws, true).unwrap());

        // Repo moved on.
        workspace(
            ws,
            "lockfix-gate",
            declared,
            Some(("https://example.invalid/old.git", Some("feature"), SHA_A)),
        );
        assert!(!dependencies_present(ws), "the repo changed");
        assert!(workspace_has_unlocked_git_deps(ws, true).unwrap());

        // Pinned commit edited by hand.
        workspace(
            ws,
            "lockfix-gate",
            &format!("repo = \"{repo}\"\ncommit = \"{SHA_B}\"\n"),
            Some((repo, None, SHA_A)),
        );
        assert!(!dependencies_present(ws), "the pinned commit changed");
        assert!(workspace_has_unlocked_git_deps(ws, true).unwrap());
    }

    /// A lockfile written before the branch was recorded says nothing about
    /// which branch it came from, and rejecting it would re-resolve every
    /// dependency of every workspace that has one — the exact behaviour being
    /// fixed. It is taken at face value until the next write names its branch.
    #[test]
    fn a_legacy_lock_entry_still_satisfies_the_present_check() {
        deps_cache();
        seed_cache("lockfix-legacy", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-legacy",
            "repo = \"https://example.invalid/legacy.git\"\n\
             branch = \"main\"\n",
            Some(("https://example.invalid/legacy.git", None, SHA_A)),
        );
        assert!(dependencies_present(ws));
        assert!(!workspace_has_unlocked_git_deps(ws, true).unwrap());
    }

    /// `vw update` is the one caller that may move a pin, so it asks the
    /// remote even when the lockfile has an answer and the tree is cached.
    #[tokio::test]
    async fn an_update_re_resolves_a_locked_branch_dependency() {
        deps_cache();
        seed_cache("lockfix-fresh", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-fresh",
            "repo = \"https://example.invalid/fresh.git\"\n\
             branch = \"main\"\n",
            Some(("https://example.invalid/fresh.git", Some("main"), SHA_A)),
        );

        let e = build_dependency_graph(ws, false, None, Resolution::Fresh)
            .await
            .expect_err("a fresh resolve consults the remote");
        assert!(
            e.to_string().contains("resolve commit for dependency"),
            "expected a failed branch resolution, got: {e}"
        );
    }

    /// A pin is only about the repository it was taken from. Repoint a
    /// dependency in `vw.toml` and its old commit means nothing.
    #[tokio::test]
    async fn a_lock_entry_for_another_repo_does_not_pin() {
        deps_cache();
        seed_cache("lockfix-moved", SHA_A);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        workspace(
            ws,
            "lockfix-moved",
            "repo = \"https://example.invalid/new.git\"\nbranch = \"main\"\n",
            Some(("https://example.invalid/old.git", Some("main"), SHA_A)),
        );

        let e = build_dependency_graph(ws, false, None, Resolution::Locked)
            .await
            .expect_err("the stale pin is not reused for a new repo");
        assert!(
            e.to_string().contains("resolve commit for dependency"),
            "expected a failed branch resolution, got: {e}"
        );
    }

    /// A dependency somebody just added has nothing to honour, so it
    /// resolves normally rather than failing for want of a lock entry.
    #[tokio::test]
    async fn a_dependency_absent_from_the_lock_still_resolves() {
        deps_cache();
        seed_cache("lockfix-added", SHA_B);
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        // Pinned by commit in the manifest, so resolution needs no remote.
        workspace(
            ws,
            "lockfix-added",
            &format!(
                "repo = \"https://example.invalid/added.git\"\n\
                 commit = \"{SHA_B}\"\n"
            ),
            None,
        );

        let graph = build_dependency_graph(ws, false, None, Resolution::Locked)
            .await
            .expect("an unlocked dependency resolves");

        let dep = graph
            .node_indices()
            .map(|i| &graph[i])
            .find(|n| n.name.as_deref() == Some("lockfix-added"))
            .expect("the dependency is in the graph");
        assert_eq!(dep.locked.as_ref().unwrap().commit, SHA_B);
    }
}

#[cfg(test)]
mod dependency_source_tests {
    use super::*;

    /// A temp dir plus its **canonicalized** path — see the
    /// identically-named helper in `vw-analyzer`'s htcl_backend
    /// tests for the full story. Short version: on macOS `$TMPDIR`
    /// lives under `/var/folders/…`, a symlink to
    /// `/private/var/folders/…`. Dependency resolution
    /// canonicalizes the paths it walks, so a test comparing a
    /// resolved path against one built from the raw `TempDir`
    /// path compares two spellings of the same directory and
    /// fails. Derive test paths from the returned `PathBuf`.
    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    #[test]
    fn manifest_path_dep_wins_over_stale_git_lock_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        fs::create_dir_all(ws.join("local-foo")).unwrap();
        fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"t\"\nversion = \"0.1.0\"\n\
             [dependencies.foo]\npath = \"local-foo\"\n",
        )
        .unwrap();
        // Stale git pin for `foo`, left over from before the
        // `repo → path` switch.
        fs::write(
            ws.join("vw.lock"),
            "[dependencies.foo]\n\
             repo = \"https://example.com/foo.git\"\n\
             commit = \"deadbeef\"\n\
             path = \"foo-deadbeef\"\n",
        )
        .unwrap();
        let paths = dep_cache_paths_with_test(ws, false).unwrap();
        let foo = paths.get("foo").expect("foo resolves");
        assert!(
            foo.ends_with("local-foo"),
            "manifest path dep must win over a stale git lock entry, \
             got {foo:?}"
        );
    }

    #[test]
    fn prune_drops_now_path_dep_but_keeps_git_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        fs::create_dir_all(ws.join("local-foo")).unwrap();
        fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"t\"\nversion = \"0.1.0\"\n\
             [dependencies.foo]\npath = \"local-foo\"\n\
             [dependencies.bar]\n\
             repo = \"https://example.com/bar.git\"\nbranch = \"main\"\n",
        )
        .unwrap();
        fs::write(
            ws.join("vw.lock"),
            "[dependencies.foo]\n\
             repo = \"https://example.com/foo.git\"\n\
             commit = \"dead\"\npath = \"foo-dead\"\n\
             [dependencies.bar]\n\
             repo = \"https://example.com/bar.git\"\n\
             commit = \"beef\"\npath = \"bar-beef\"\n",
        )
        .unwrap();
        // `foo` is now a path dep → its stale git entry is pruned;
        // `bar` (still git) is left untouched.
        assert!(prune_stale_path_deps_from_lock(ws).unwrap());
        let lock = load_lock_file(ws).unwrap();
        assert!(!lock.dependencies.contains_key("foo"), "foo pruned");
        assert!(lock.dependencies.contains_key("bar"), "bar kept");
        // Idempotent: nothing left to prune on a second pass.
        assert!(!prune_stale_path_deps_from_lock(ws).unwrap());
    }

    #[test]
    fn vho_template_rewrites_to_black_box_entity() {
        // A realistic Vivado VHDL instantiation template: banner
        // comments (one of which literally says "COMPONENT"), the
        // component declaration, then the instantiation example.
        let vho = "\
-- (c) Copyright 1995-2024 AMD, Inc. All rights reserved.\n\
-- The following code must appear in the VHDL architecture header:\n\
------------- Begin Cut here for COMPONENT Declaration ------ COMP_TAG\n\
component primary_clock\n\
port (\n\
  clk_out1 : out std_logic;\n\
  locked : out std_logic;\n\
  clk_in1 : in std_logic\n\
);\n\
end component;\n\
-- COMP_TAG_END ------ End COMPONENT Declaration ------------\n\
-- The following code must appear in the VHDL architecture body:\n\
-------------- Begin Cut here for INSTANTIATION Template ----- INST_TAG\n\
your_instance_name : primary_clock\n\
  port map (\n\
    clk_out1 => clk_out1,\n\
    locked => locked,\n\
    clk_in1 => clk_in1\n\
  );\n\
-- INST_TAG_END ------ End INSTANTIATION Template ---------\n";
        let stub = vho_component_to_entity(vho).expect("component found");
        assert!(stub.contains("entity primary_clock is"), "stub:\n{stub}");
        assert!(stub.contains("clk_out1 : out std_logic"), "ports copied");
        assert!(stub.contains("clk_in1 : in std_logic"), "ports copied");
        assert!(stub.contains("end entity;"));
        assert!(stub.contains("architecture stub of primary_clock is"));
        // The instantiation example (a `component`-free section) must
        // not leak in, and we must not have matched the banner comment.
        assert!(
            !stub.contains("your_instance_name"),
            "instantiation template leaked into the stub"
        );
        assert!(!stub.contains("component"), "no component syntax remains");
    }

    #[test]
    fn vho_without_component_returns_none() {
        assert!(vho_component_to_entity("-- just a comment\nfoo\n").is_none());
    }

    /// A `vw.toml` entry with `repo = "..."` parses as a git source —
    /// the historical behaviour that pre-dates path deps.
    #[test]
    fn git_dep_parses_from_repo_key() {
        let toml = r#"
            [workspace]
            name = "demo"
            version = "0.1.0"

            [dependencies.quartz]
            repo = "https://github.com/oxidecomputer/quartz"
            branch = "main"
            src = ["hdl/ip/vhd"]
            recursive = true
        "#;
        let config: WorkspaceConfig = toml::from_str(toml).unwrap();
        let dep = &config.dependencies["quartz"];
        assert!(!dep.is_local());
        assert_eq!(dep.repo(), Some("https://github.com/oxidecomputer/quartz"));
        assert_eq!(dep.branch(), Some("main"));
        assert!(dep.recursive);
        assert_eq!(dep.src, vec!["hdl/ip/vhd".to_string()]);
    }

    /// The metroid layout: `path = "..."` and nothing else.
    #[test]
    fn path_dep_parses_from_path_key() {
        let toml = r#"
            [workspace]
            name = "metroid"
            version = "0.1.0"

            [dependencies.amd-htcl]
            path = "/home/ry/src/amd-htcl"
        "#;
        let config: WorkspaceConfig = toml::from_str(toml).unwrap();
        let dep = &config.dependencies["amd-htcl"];
        assert!(dep.is_local());
        assert_eq!(dep.local_path(), Some(Path::new("/home/ry/src/amd-htcl")));
        assert_eq!(dep.repo(), None);
        assert_eq!(dep.branch(), None);
    }

    #[test]
    fn transitive_dep_resolution_pulls_in_lib_of_lib() {
        // metroid → cips → vivado-cmd.  Asking for metroid's deps
        // transitively should return cips AND vivado-cmd, even though
        // metroid only declares cips.
        let (_tmp, dir) = canonical_tempdir();
        let metroid = dir.as_path().join("metroid");
        let cips = dir.as_path().join("cips");
        let vivado_cmd = dir.as_path().join("vivado-cmd");
        std::fs::create_dir_all(&metroid).unwrap();
        std::fs::create_dir_all(&cips).unwrap();
        std::fs::create_dir_all(&vivado_cmd).unwrap();
        std::fs::write(
            metroid.join("vw.toml"),
            format!(
                "[workspace]\nname=\"metroid\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.cips]\npath = \"{}\"\n",
                cips.display()
            ),
        )
        .unwrap();
        std::fs::write(
            cips.join("vw.toml"),
            format!(
                "[workspace]\nname=\"cips\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.vivado-cmd]\npath = \"{}\"\n",
                vivado_cmd.display()
            ),
        )
        .unwrap();
        // vivado-cmd is a leaf — has a vw.toml but no deps of its own.
        std::fs::write(
            vivado_cmd.join("vw.toml"),
            "[workspace]\nname=\"vivado-cmd\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();

        let metroid_utf8 = Utf8PathBuf::from_path_buf(metroid.clone()).unwrap();
        let resolved = transitive_dep_cache_paths(&metroid_utf8).unwrap();
        assert_eq!(resolved.get("cips"), Some(&cips));
        assert_eq!(resolved.get("vivado-cmd"), Some(&vivado_cmd));
        assert_eq!(resolved.len(), 2, "{resolved:?}");
    }

    #[test]
    fn transitive_dep_resolution_first_seen_wins() {
        // entry → A and entry → B, both A and B declare a dep
        // `shared` pointing at different paths. Entry's view of
        // `shared` is whichever was inserted first; entry itself
        // doesn't declare `shared`, so the test just asserts we got
        // *one* deterministic answer rather than a panic / duplicate.
        let (_tmp, dir) = canonical_tempdir();
        let entry = dir.as_path().join("entry");
        let a = dir.as_path().join("a");
        let b = dir.as_path().join("b");
        let shared_v1 = dir.as_path().join("shared-v1");
        let shared_v2 = dir.as_path().join("shared-v2");
        for d in [&entry, &a, &b, &shared_v1, &shared_v2] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            entry.join("vw.toml"),
            format!(
                "[workspace]\nname=\"entry\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.a]\npath = \"{}\"\n\
                 [dependencies.b]\npath = \"{}\"\n",
                a.display(),
                b.display()
            ),
        )
        .unwrap();
        std::fs::write(
            a.join("vw.toml"),
            format!(
                "[workspace]\nname=\"a\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.shared]\npath = \"{}\"\n",
                shared_v1.display()
            ),
        )
        .unwrap();
        std::fs::write(
            b.join("vw.toml"),
            format!(
                "[workspace]\nname=\"b\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.shared]\npath = \"{}\"\n",
                shared_v2.display()
            ),
        )
        .unwrap();

        let entry_utf8 = Utf8PathBuf::from_path_buf(entry).unwrap();
        let resolved = transitive_dep_cache_paths(&entry_utf8).unwrap();
        // `shared` is present exactly once and points at one of the
        // two candidates; we don't pin which (HashMap iter order).
        let shared = resolved.get("shared").unwrap();
        assert!(*shared == shared_v1 || *shared == shared_v2, "{shared:?}");
    }

    /// Local deps round-trip through serialize/deserialize.
    #[test]
    fn path_dep_roundtrips() {
        let dep = Dependency {
            source: DependencySource::Path {
                path: PathBuf::from("/some/where"),
            },
            src: Vec::new(),
            recursive: false,
            sim_only: false,
            exclude: Vec::new(),
        };
        let serialized = toml::to_string(&dep).unwrap();
        let deserialized: Dependency = toml::from_str(&serialized).unwrap();
        assert!(deserialized.is_local());
        assert_eq!(deserialized.local_path(), Some(Path::new("/some/where")));
    }

    #[test]
    fn test_dependencies_parse_from_test_dependencies_section() {
        let toml = r#"
            [workspace]
            name = "demo"
            version = "0.1.0"

            [dependencies.vivado-cmd]
            path = "/home/x/vivado-cmd"

            [test-dependencies.test]
            path = "/home/x/test"
        "#;
        let config: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.dependencies.len(), 1);
        assert_eq!(config.test_dependencies.len(), 1);
        assert!(config.test_dependencies["test"].is_local());
    }

    #[test]
    fn list_htcl_tests_walks_test_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "").unwrap();
        std::fs::create_dir_all(ws.join("test/nested")).unwrap();
        std::fs::create_dir_all(ws.join("test/.hidden")).unwrap();
        std::fs::create_dir_all(ws.join("test/target")).unwrap();
        std::fs::write(ws.join("test/a.htcl"), "").unwrap();
        std::fs::write(ws.join("test/b.htcl"), "").unwrap();
        std::fs::write(ws.join("test/skip.vhd"), "").unwrap();
        std::fs::write(ws.join("test/nested/c.htcl"), "").unwrap();
        std::fs::write(ws.join("test/.hidden/z.htcl"), "").unwrap();
        std::fs::write(ws.join("test/target/z.htcl"), "").unwrap();
        let tests = list_htcl_tests(&ws).unwrap();
        assert_eq!(tests.len(), 3, "{:?}", tests);
        assert!(tests[0].ends_with("test/a.htcl"));
        assert!(tests[1].ends_with("test/b.htcl"));
        assert!(tests[2].ends_with("test/nested/c.htcl"));
    }

    #[test]
    fn list_htcl_tests_returns_empty_when_test_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "").unwrap();
        let tests = list_htcl_tests(&ws).unwrap();
        assert!(tests.is_empty());
    }

    #[test]
    fn target_pattern_parses_brace_form() {
        let p = parse_target_pattern("versal{xcvm3(.*)}").unwrap();
        assert_eq!(p.family, "versal");
        assert!(p.regex.is_match("xcvm3358-vsvh1747-2M-e-S"));
        assert!(!p.regex.is_match("xc7z020clg484-1"));
    }

    #[test]
    fn target_pattern_rejects_bare_family() {
        // Bare `artix7` (no braces) shouldn't reach downstream vw.
        // toml; `vw ip generate` normalizes into brace form.
        let e = parse_target_pattern("artix7").unwrap_err();
        assert!(matches!(e, TargetParseError::MissingBraces { .. }));
    }

    #[test]
    fn target_pattern_anchors_at_start() {
        // `xcvm3(.*)` should NOT match "xxx-xcvm3358" (regex would
        // otherwise be "contains xcvm3"). Anchoring at start
        // prevents that.
        let p = parse_target_pattern("versal{xcvm3(.*)}").unwrap();
        assert!(p.regex.is_match("xcvm3358"));
        assert!(!p.regex.is_match("blah-xcvm3358"));
    }

    #[test]
    fn workspace_config_parses_target_part_and_targets() {
        let toml = r#"
            [workspace]
            name = "clk-wizard"
            version = "0.1.0"

            [targets]
            supported = [
                "versal{xcvm3(.*)}",
                "versal{xc2ve3(.*)}",
            ]
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.workspace.name, "clk-wizard");
        assert!(cfg.workspace.target_parts.is_empty());
        let t = cfg.targets.expect("expected [targets]");
        assert_eq!(t.supported.len(), 2);
    }

    #[test]
    fn workspace_config_parses_multi_target_parts() {
        let toml = r#"
            [workspace]
            name = "metroid"
            version = "0.1.0"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.workspace.target_parts.len(), 2);
        assert_eq!(
            cfg.workspace.default_target_part().unwrap(),
            Some("xcvp1202-vsva2785-2MHP-e-S"),
        );
        // Substring selector picks the non-default.
        assert_eq!(
            cfg.workspace.select_target_part(Some("3HP")).unwrap(),
            Some("xcvp1202-vsva2785-3HP-e-S"),
        );
    }

    #[test]
    fn multi_parts_without_default_flag_errors() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-2MHP-e-S"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.workspace.default_target_part(),
            Err(TargetSelectError::NoDefault { count: 2 }),
        ));
    }

    #[test]
    fn ambiguous_substring_errors() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        // "xcvp1202" matches both entries.
        assert!(matches!(
            cfg.workspace.select_target_part(Some("xcvp1202")),
            Err(TargetSelectError::Ambiguous { .. }),
        ));
    }

    #[test]
    fn single_part_is_implicit_default() {
        let toml = r#"
            [workspace]
            name = "vw"
            version = "0.1.0"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.workspace.default_target_part().unwrap(),
            Some("xcvp1202-vsva2785-3HP-e-S"),
        );
    }

    #[test]
    fn workspace_config_parses_variants_block() {
        let toml = r#"
            [workspace]
            name = "metroid"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true
            exclusive = ["hdl/ethernet-vpk120.vhd"]

            [[workspace.variants]]
            name = "metro"
            part = "xcvp1202-vsva2785-3HP-e-S"
            exclusive = ["hdl/ethernet-metro.vhd"]
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.workspace.variants.len(), 2);
        assert_eq!(cfg.workspace.variants[0].name, "vpk120");
        assert_eq!(
            cfg.workspace.variants[0].part,
            "xcvp1202-vsva2785-2MHP-e-S"
        );
        assert!(cfg.workspace.variants[0].default);
        assert_eq!(
            cfg.workspace.variants[0].exclusive,
            vec!["hdl/ethernet-vpk120.vhd"],
        );
        assert_eq!(cfg.workspace.variants[1].name, "metro");
        assert!(!cfg.workspace.variants[1].default);
        // Empty target_parts — variants own their parts inline.
        assert!(cfg.workspace.target_parts.is_empty());
    }

    #[test]
    fn variants_and_target_parts_are_mutually_exclusive() {
        // Deserialization allows both (unknown-field serde is
        // lenient), but `load_workspace_config` refuses to
        // return a config that has both — variants own parts.
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.target-parts]]
            part = "xcvp1202-vsva2785-2MHP-e-S"

            [[workspace.variants]]
            name = "v"
            part = "xcvp1202-vsva2785-2MHP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        let err = validate_variant_shape(&cfg.workspace).unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "expected mutual-exclusion error: {err}",
        );
    }

    #[test]
    fn duplicate_variant_names_error() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        let err = validate_variant_shape(&cfg.workspace).unwrap_err();
        assert!(
            err.to_string().contains("duplicate variant name"),
            "expected duplicate-name error: {err}",
        );
    }

    #[test]
    fn default_variant_no_variants_yields_none() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert!(cfg.workspace.default_variant().unwrap().is_none());
    }

    #[test]
    fn default_variant_multi_with_default_flag() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true

            [[workspace.variants]]
            name = "metro"
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        let v = cfg.workspace.default_variant().unwrap().unwrap();
        assert_eq!(v.name, "vpk120");
    }

    #[test]
    fn default_variant_multi_without_default_errors() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"

            [[workspace.variants]]
            name = "metro"
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.workspace.default_variant(),
            Err(VariantSelectError::NoDefault { count: 2 }),
        ));
    }

    #[test]
    fn select_variant_exact_match_only() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true

            [[workspace.variants]]
            name = "metro"
            part = "xcvp1202-vsva2785-3HP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        // Exact match returns the entry.
        assert_eq!(
            cfg.workspace
                .select_variant(Some("metro"))
                .unwrap()
                .unwrap()
                .name,
            "metro",
        );
        // Substring is NOT accepted — variant names are the
        // whole selector.
        assert!(matches!(
            cfg.workspace.select_variant(Some("vpk")),
            Err(VariantSelectError::NoMatch { .. }),
        ));
    }

    #[test]
    fn resolve_top_variant_overrides_workspace() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"
            top = "workspace_default_top"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
            default = true
            top = "top_vpk120"

            [[workspace.variants]]
            name = "metro"
            part = "xcvp1202-vsva2785-3HP-e-S"
            # no per-variant top → falls back to workspace top
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.workspace.resolve_top(Some("vpk120")).as_deref(),
            Some("top_vpk120"),
        );
        assert_eq!(
            cfg.workspace.resolve_top(Some("metro")).as_deref(),
            Some("workspace_default_top"),
        );
        assert_eq!(
            cfg.workspace.resolve_top(None).as_deref(),
            Some("workspace_default_top"),
        );
    }

    #[test]
    fn resolve_top_none_when_unset_everywhere() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert!(cfg.workspace.resolve_top(Some("vpk120")).is_none());
        assert!(cfg.workspace.resolve_top(None).is_none());
    }

    #[test]
    fn resolve_top_unknown_variant_falls_back_to_workspace() {
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"
            top = "workspace_top"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
            top = "top_vpk120"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        // A variant name not in the list falls through to workspace-level.
        // (In practice select_variant would error before this, but the
        // resolver must not panic on unknown names.)
        assert_eq!(
            cfg.workspace.resolve_top(Some("ghost")).as_deref(),
            Some("workspace_top"),
        );
    }

    #[test]
    fn single_variant_no_default_flag_still_parses() {
        // A single-variant list is legal without `default = true`
        // (the default becomes implicit — same rule as
        // `[[target-parts]]` single-entry).
        let toml = r#"
            [workspace]
            name = "x"
            version = "0.1.0"

            [[workspace.variants]]
            name = "vpk120"
            part = "xcvp1202-vsva2785-2MHP-e-S"
        "#;
        let cfg: WorkspaceConfig = toml::from_str(toml).unwrap();
        assert!(validate_variant_shape(&cfg.workspace).is_ok());
        assert_eq!(cfg.workspace.variants.len(), 1);
    }

    fn make_dep(name: &str, patterns: &[&str]) -> (String, Vec<TargetPattern>) {
        let compiled: Vec<TargetPattern> = patterns
            .iter()
            .map(|s| parse_target_pattern(s).unwrap())
            .collect();
        (name.to_string(), compiled)
    }

    #[test]
    fn target_compat_matches_when_pattern_covers_part() {
        let mut dt = DepTargets::default();
        let (name, patterns) = make_dep("clk-wizard", &["versal{xcvm3(.*)}"]);
        dt.per_dep.insert(name, patterns);
        let mismatches =
            check_target_compatibility(Some("xcvm3358-vsvh1747-2M-e-S"), &dt);
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    #[test]
    fn target_compat_reports_unblessed_when_no_pattern_matches() {
        let mut dt = DepTargets::default();
        let (name, patterns) = make_dep("clk-wizard", &["versal{xcvm3(.*)}"]);
        dt.per_dep.insert(name, patterns);
        let mismatches =
            check_target_compatibility(Some("xc7z020clg484-1"), &dt);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].dep, "clk-wizard");
        assert_eq!(mismatches[0].supported_families, vec!["versal"]);
        assert_eq!(mismatches[0].kind, TargetMismatchKind::Unblessed);
    }

    #[test]
    fn target_compat_reports_not_supported_wins_over_supported() {
        // If a target matches both a supported and a not-supported
        // pattern, the explicit ban must win — Xilinx has attested
        // the combination doesn't work.
        let mut dt = DepTargets::default();
        let (name, sup) = make_dep("gadget", &["versal{xcv(.*)}"]);
        dt.per_dep.insert(name.clone(), sup);
        let (_, ns) = make_dep("gadget", &["versal{xcvp1202.*}"]);
        dt.per_dep_not_supported.insert(name, ns);
        let mismatches =
            check_target_compatibility(Some("xcvp1202-vsva2785-3HP-e-S"), &dt);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, TargetMismatchKind::NotSupported);
    }

    #[test]
    fn target_compat_supported_match_clears_when_no_ban() {
        let mut dt = DepTargets::default();
        let (name, sup) = make_dep("gadget", &["versal{xcvp1202.*}"]);
        dt.per_dep.insert(name, sup);
        let mismatches =
            check_target_compatibility(Some("xcvp1202-vsva2785-3HP-e-S"), &dt);
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    #[test]
    fn target_compat_treats_empty_patterns_as_universal() {
        // @vw / @test — no [targets] means "we support anything."
        let mut dt = DepTargets::default();
        dt.per_dep.insert("vw".into(), Vec::new());
        let mismatches =
            check_target_compatibility(Some("xc7z020clg484-1"), &dt);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn target_compat_no_op_when_no_target_declared() {
        // Library workspaces have no target-part — check should
        // silently accept.
        let mut dt = DepTargets::default();
        let (name, patterns) = make_dep("clk-wizard", &["versal{xcvm3(.*)}"]);
        dt.per_dep.insert(name, patterns);
        let mismatches = check_target_compatibility(None, &dt);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn library_name_hyphens_become_underscores() {
        assert_eq!(library_name_for_dep("clk-wizard"), "clk_wizard");
        assert_eq!(library_name_for_dep("gtwiz-versal"), "gtwiz_versal");
        assert_eq!(library_name_for_dep("cpm5"), "cpm5");
    }

    #[test]
    fn vhdl_design_sources_empty_when_no_hdl_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // No `hdl/` yet — return empty, not error.
        let sources = vhdl_design_sources(&ws).unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn vhdl_dependency_sources_skips_deps_without_src() {
        // Regression guard: a path dep with no `src` field is
        // htcl-only and shouldn't contribute VHDL. Previously the
        // enumeration walked each dep's whole tree recursively,
        // scooping up e.g. `target/ip/*/wrapper.vhd` from an htcl
        // library's own generated artifacts.
        let tmp = tempfile::tempdir().unwrap();
        // Fake htcl-only dep: has a stray .vhd (like a generated
        // wrapper) but declares no `src`.
        let htcl_dep = tmp.path().join("htcl-only-dep");
        std::fs::create_dir_all(htcl_dep.join("target/ip/foo")).unwrap();
        std::fs::write(
            htcl_dep.join("target/ip/foo/wrapper.vhd"),
            "-- generated",
        )
        .unwrap();
        // Fake VHDL dep: declares `src = ["hdl"]`.
        let vhdl_dep = tmp.path().join("vhdl-dep");
        std::fs::create_dir_all(vhdl_dep.join("hdl")).unwrap();
        std::fs::write(vhdl_dep.join("hdl/mod.vhd"), "-- source").unwrap();

        // Entry workspace vw.toml referencing both.
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            format!(
                r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.htcl-only-dep]
path = "{}"

[dependencies.vhdl-dep]
path = "{}"
src = ["hdl"]
recursive = true
"#,
                htcl_dep.display(),
                vhdl_dep.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_dependency_sources(&ws_utf8).unwrap();
        // Only the vhdl-dep contributes. htcl-only-dep is skipped
        // even though its tree contains a .vhd file.
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].library, "vhdl_dep");
        assert!(
            sources[0].path.ends_with("hdl/mod.vhd"),
            "unexpected path {}",
            sources[0].path.display(),
        );
    }

    #[test]
    fn vhdl_dependency_sources_git_cache_is_flattened() {
        // Regression: git-dep caches under `~/.vw/deps/<name>-<sha>/`
        // are FLATTENED at copy time — `copy_vhdl_files_glob` strips
        // the source repo's `hdl/ip/vhd/synchronizers/` prefix off,
        // so files land directly at the cache root. Enumeration
        // must NOT re-apply the `src` pattern as a subdir join
        // (which would find nothing) — instead it walks the cache
        // root recursively.
        let tmp = tempfile::tempdir().unwrap();
        // Simulated cache — flat file layout.
        let cache = tmp.path().join("cache/quartz_sync-abc");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("meta_sync.vhd"), "").unwrap();
        std::fs::write(cache.join("bacd.vhd"), "").unwrap();

        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.quartz_sync]
repo = "https://example.invalid/quartz"
branch = "main"
src = ["hdl/ip/vhd/synchronizers"]
"#,
        )
        .unwrap();
        std::fs::write(
            ws.join("vw.lock"),
            format!(
                r#"
[dependencies.quartz_sync]
repo = "https://example.invalid/quartz"
commit = "abc"
path = "{}"
src = ["hdl/ip/vhd/synchronizers"]
recursive = false
sim_only = false
submodules = false
exclude = []
"#,
                cache.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_dependency_sources(&ws_utf8).unwrap();
        // Both files show up despite `src` pointing at a
        // subdirectory that doesn't exist in the flat cache.
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(sources.iter().all(|s| s.library == "quartz_sync"));
    }

    #[test]
    fn vhdl_dependency_sources_finds_git_dep_via_lockfile() {
        // Simulates the real workflow: a git dep declared in
        // vw.toml, resolved to a cache dir via vw.lock. The
        // lockfile's `path` is absolute so we don't need to
        // override `VW_DEPS_DIR`.
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache/quartz_sync-abc123");
        std::fs::create_dir_all(cache.join("hdl/ip/vhd/synchronizers"))
            .unwrap();
        std::fs::write(
            cache.join("hdl/ip/vhd/synchronizers/sync.vhd"),
            "-- synced",
        )
        .unwrap();

        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.quartz-sync]
repo = "https://example.invalid/quartz"
branch = "main"
src = ["hdl/ip/vhd/synchronizers"]
recursive = false
"#,
        )
        .unwrap();
        std::fs::write(
            ws.join("vw.lock"),
            format!(
                r#"
[dependencies.quartz-sync]
repo = "https://example.invalid/quartz"
commit = "abc123"
path = "{}"
src = ["hdl/ip/vhd/synchronizers"]
recursive = false
sim_only = false
submodules = false
exclude = []
"#,
                cache.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_dependency_sources(&ws_utf8).unwrap();
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].library, "quartz_sync");
        assert!(sources[0].path.ends_with("sync.vhd"));
    }

    #[test]
    fn vhdl_dependency_sources_resolves_relative_path_dep() {
        // Portable fixture pattern: a path dep whose `path`
        // is relative to the declaring workspace's vw.toml —
        // Cargo-parity. Same fixture works from any machine.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("fixtures/lib/hdl")).unwrap();
        std::fs::write(ws.join("fixtures/lib/hdl/a.vhd"), "").unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.lib]
path = "fixtures/lib"
src = ["hdl"]
recursive = true
"#,
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_dependency_sources(&ws_utf8).unwrap();
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].path.ends_with("hdl/a.vhd"));
    }

    #[test]
    fn get_access_credentials_for_workspace_only_scans_test_deps_when_asked() {
        // No netrc → both variants return None regardless of
        // dep-set — regression guard for the `include_test`
        // dispatch path. The bigger scenario (netrc HIT for a
        // git URL) is covered by the underlying
        // `get_access_credentials_from_netrc` test.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.g]
repo = "https://example.invalid/x"
branch = "main"

[test-dependencies.gt]
repo = "https://example.invalid/y"
branch = "main"
"#,
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        assert!(
            get_access_credentials_for_workspace(&ws_utf8, false).is_none(),
        );
        assert!(get_access_credentials_for_workspace(&ws_utf8, true).is_none(),);
    }

    #[test]
    fn unlocked_git_deps_detection() {
        // No git deps → never unlocked, regardless of lockfile.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.local]
path = "/tmp/somewhere"
"#,
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws.clone()).unwrap();
        assert!(!workspace_has_unlocked_git_deps(&ws_utf8, false).unwrap());
        assert!(!workspace_has_unlocked_git_deps(&ws_utf8, true).unwrap());

        // Git dep + missing lockfile → unlocked.
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.g]
repo = "https://example.invalid/x"
branch = "main"
src = ["hdl"]
"#,
        )
        .unwrap();
        assert!(workspace_has_unlocked_git_deps(&ws_utf8, false).unwrap());

        // Git dep + lockfile that has an entry for it → locked.
        std::fs::write(
            ws.join("vw.lock"),
            r#"
[dependencies.g]
repo = "https://example.invalid/x"
commit = "abc"
path = "/tmp/g"
src = ["hdl"]
recursive = false
sim_only = false
submodules = false
exclude = []
"#,
        )
        .unwrap();
        assert!(!workspace_has_unlocked_git_deps(&ws_utf8, false).unwrap());

        // Git test-dep, lockfile only has the regular dep → unlocked
        // for the with-test caller, locked for the plain caller.
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.g]
repo = "https://example.invalid/x"
branch = "main"

[test-dependencies.gt]
repo = "https://example.invalid/y"
branch = "main"
"#,
        )
        .unwrap();
        assert!(!workspace_has_unlocked_git_deps(&ws_utf8, false).unwrap());
        assert!(workspace_has_unlocked_git_deps(&ws_utf8, true).unwrap());
    }

    #[test]
    fn vhdl_dependency_sources_exclude_sim_only_flag() {
        // Two path deps: one flagged `sim_only = true` (mirrors
        // real deps like `unisim` / `xpm`) and one regular.
        // With the flag off both contribute; with it on only
        // the non-sim dep does.
        let tmp = tempfile::tempdir().unwrap();
        let sim_dep = tmp.path().join("sim");
        std::fs::create_dir_all(sim_dep.join("hdl")).unwrap();
        std::fs::write(sim_dep.join("hdl/sim.vhd"), "").unwrap();
        let real_dep = tmp.path().join("real");
        std::fs::create_dir_all(real_dep.join("hdl")).unwrap();
        std::fs::write(real_dep.join("hdl/real.vhd"), "").unwrap();

        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            format!(
                r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.sim]
path = "{}"
src = ["hdl"]
recursive = true
sim_only = true

[dependencies.real]
path = "{}"
src = ["hdl"]
recursive = true
"#,
                sim_dep.display(),
                real_dep.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();

        // Default (flag = false): both deps contribute.
        let all = vhdl_dependency_sources(&ws_utf8).unwrap();
        assert_eq!(all.len(), 2, "{all:?}");

        // Flag on: only the non-sim dep survives.
        let synth_clean =
            vhdl_dependency_sources_ext(&ws_utf8, false, true).unwrap();
        assert_eq!(synth_clean.len(), 1, "{synth_clean:?}");
        assert_eq!(synth_clean[0].library, "real");
    }

    #[test]
    fn vhdl_dependency_sources_include_test_flag() {
        // A test-only dep contributes iff include_test is set.
        let tmp = tempfile::tempdir().unwrap();
        let dep = tmp.path().join("dep");
        std::fs::create_dir_all(dep.join("hdl")).unwrap();
        std::fs::write(dep.join("hdl/tbutil.vhd"), "").unwrap();

        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            format!(
                r#"
[workspace]
name = "ws"
version = "0.1.0"

[test-dependencies.tbutil]
path = "{}"
src = ["hdl"]
recursive = true
"#,
                dep.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        // Production mode: test-dep hidden.
        assert!(vhdl_dependency_sources(&ws_utf8).unwrap().is_empty());
        // Test mode: test-dep visible.
        let with_test =
            vhdl_dependency_sources_with_test(&ws_utf8, true).unwrap();
        assert_eq!(with_test.len(), 1);
        assert_eq!(with_test[0].library, "tbutil");
    }

    #[test]
    fn vhdl_dependency_sources_honors_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let dep = tmp.path().join("dep");
        std::fs::create_dir_all(dep.join("hdl/sims")).unwrap();
        std::fs::write(dep.join("hdl/a.vhd"), "").unwrap();
        std::fs::write(dep.join("hdl/b_tb.vhd"), "").unwrap();
        std::fs::write(dep.join("hdl/sims/x.vhd"), "").unwrap();

        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            format!(
                r#"
[workspace]
name = "ws"
version = "0.1.0"

[dependencies.dep]
path = "{}"
src = ["hdl"]
recursive = true
exclude = ["**/sims/**", "**/*_tb.vhd"]
"#,
                dep.display(),
            ),
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_dependency_sources(&ws_utf8).unwrap();
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].path.ends_with("a.vhd"));
    }

    #[test]
    fn design_constraints_empty_when_no_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert!(design_constraints(&ws).unwrap().is_empty());
    }

    #[test]
    fn phase_scoped_constraints_isolate_per_subdir() {
        // Regression guard: `synth/` files must not leak into
        // `place/` or `route/`, and vice versa. Also verifies
        // the whole-tree `design_constraints` still returns
        // everything.
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let c = ws.join("constraints");
        std::fs::create_dir_all(c.join("synth")).unwrap();
        std::fs::create_dir_all(c.join("place")).unwrap();
        std::fs::create_dir_all(c.join("route")).unwrap();
        std::fs::write(c.join("global.xdc"), "").unwrap();
        std::fs::write(c.join("synth/only.xdc"), "").unwrap();
        std::fs::write(c.join("place/only.xdc"), "").unwrap();
        std::fs::write(c.join("route/only.xdc"), "").unwrap();

        let synth = design_synth_constraints(&ws).unwrap();
        assert_eq!(synth.len(), 1);
        assert!(synth[0].ends_with("synth/only.xdc"));

        let place = design_place_constraints(&ws).unwrap();
        assert_eq!(place.len(), 1);
        assert!(place[0].ends_with("place/only.xdc"));

        let route = design_route_constraints(&ws).unwrap();
        assert_eq!(route.len(), 1);
        assert!(route[0].ends_with("route/only.xdc"));

        // Aggregate walk returns everything under constraints/
        // regardless of subdir.
        let all = design_constraints(&ws).unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn phase_scoped_constraints_empty_when_subdir_missing() {
        // `constraints/` exists (some other subdir) but
        // `constraints/synth/` does not — expect empty, not error.
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let c = ws.join("constraints");
        std::fs::create_dir_all(c.join("place")).unwrap();
        std::fs::write(c.join("place/only.xdc"), "").unwrap();

        assert!(design_synth_constraints(&ws).unwrap().is_empty());
        assert!(design_route_constraints(&ws).unwrap().is_empty());
        assert_eq!(design_place_constraints(&ws).unwrap().len(), 1);
    }

    #[test]
    fn design_constraints_finds_xdc_and_sdc_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let c = ws.join("constraints");
        std::fs::create_dir_all(c.join("sub")).unwrap();
        std::fs::write(c.join("timing.xdc"), "").unwrap();
        std::fs::write(c.join("sub/pins.xdc"), "").unwrap();
        std::fs::write(c.join("sub/synopsys.sdc"), "").unwrap();
        // Non-constraint sibling — should be skipped.
        std::fs::write(c.join("readme.md"), "").unwrap();

        let files = design_constraints(&ws).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(names.contains(&"timing.xdc".to_string()));
        assert!(names.contains(&"pins.xdc".to_string()));
        assert!(names.contains(&"synopsys.sdc".to_string()));
        assert!(!names.contains(&"readme.md".to_string()));
    }

    #[test]
    fn vhdl_ip_sources_empty_when_no_target_ip_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert!(vhdl_ip_sources(&ws).unwrap().is_empty());
    }

    #[test]
    fn vhdl_ip_sources_walks_target_ip_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let ip = ws.join("target/ip");
        std::fs::create_dir_all(ip.join("clocky")).unwrap();
        std::fs::create_dir_all(ip.join("cips")).unwrap();
        std::fs::write(ip.join("clocky/wrapper.vhd"), "").unwrap();
        std::fs::write(ip.join("cips/wrapper.vhd"), "").unwrap();
        // Non-VHDL siblings shouldn't get pulled in.
        std::fs::write(ip.join("clocky/notes.md"), "").unwrap();

        let sources = vhdl_ip_sources(&ws).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| {
                let ip_name = p
                    .parent()
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                ip_name.to_string()
            })
            .collect();
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(names.contains(&"clocky".to_string()));
        assert!(names.contains(&"cips".to_string()));
    }

    /// Regression: `target/ip/bd/**`, `target/ip/xci/**`, and
    /// `target/vw-project/**` are the (legacy + on-disk) Vivado
    /// cache trees. The `.vhd` files under them are registered
    /// with the Vivado project via `read_bd` / `read_ip` /
    /// `synth_ip`, so `vhdl_ip_sources` must NOT list them
    /// (otherwise `synth`'s `read_vhdl` conflicts with the
    /// sub-design registration and Vivado emits `[filemgmt
    /// 20-1440]` CRITICAL WARNINGs).
    ///
    /// `target/vw-project/` is a sibling of `target/ip/` so the
    /// current walker rooted at `target/ip/` doesn't actually
    /// descend into it — the assertion here documents the
    /// invariant so a future walker refactor (rooting higher up,
    /// at `target/` for example) doesn't silently regress.
    #[test]
    fn vhdl_ip_sources_excludes_vivado_cache_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let ip = ws.join("target/ip");
        // Legit wrappers.
        std::fs::create_dir_all(ip.join("cips")).unwrap();
        std::fs::create_dir_all(ip.join("dcmac")).unwrap();
        std::fs::write(ip.join("cips/wrapper.vhd"), "").unwrap();
        std::fs::write(ip.join("dcmac/wrapper.vhd"), "").unwrap();
        // Legacy BD cache outputs — filter out.
        std::fs::create_dir_all(ip.join("bd/cips/synth")).unwrap();
        std::fs::create_dir_all(ip.join("bd/cips/sim")).unwrap();
        std::fs::create_dir_all(ip.join("bd/dcmac/synth")).unwrap();
        std::fs::write(ip.join("bd/cips/synth/cips.vhd"), "").unwrap();
        std::fs::write(ip.join("bd/cips/sim/cips.vhd"), "").unwrap();
        std::fs::write(ip.join("bd/dcmac/synth/dcmac.vhd"), "").unwrap();
        // Legacy XCI cache outputs — filter out.
        std::fs::create_dir_all(ip.join("xci/primary_clock")).unwrap();
        std::fs::write(ip.join("xci/primary_clock/primary_clock.vhd"), "")
            .unwrap();
        // On-disk Vivado project sibling — invariant guard.
        let vw_proj_gen = ws.join(
            "target/vw-project/metroid/metroid.gen/sources_1/bd/cips/synth",
        );
        std::fs::create_dir_all(&vw_proj_gen).unwrap();
        std::fs::write(vw_proj_gen.join("cips.vhd"), "").unwrap();

        let sources = vhdl_ip_sources(&ws).unwrap();
        assert_eq!(sources.len(), 2, "{sources:?}");
        for p in &sources {
            let s = p.to_string_lossy();
            assert!(
                !s.contains("/target/ip/bd/"),
                "bd cache file leaked into ip_sources: {s}"
            );
            assert!(
                !s.contains("/target/ip/xci/"),
                "xci cache file leaked into ip_sources: {s}"
            );
            assert!(
                !s.contains("/target/vw-project/"),
                "vw-project file leaked into ip_sources: {s}"
            );
            assert!(s.ends_with("wrapper.vhd"), "unexpected file: {s}");
        }
    }

    // `render_vhdl_ls_config` composes design + wrappers + BD RTL
    // + deps into a single VhdlLsConfig. Confirms every source
    // lands in the right library.
    #[test]
    fn render_vhdl_ls_config_populates_expected_libraries() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // Design source.
        std::fs::create_dir_all(ws.join("hdl")).unwrap();
        std::fs::write(ws.join("hdl/top.vhd"), "").unwrap();
        // Empty vw.toml so workspace enumeration succeeds
        // without complaining about missing config.
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n[dependencies]\n\
             [test-dependencies]\n",
        )
        .unwrap();
        // IP wrapper.
        std::fs::create_dir_all(ws.join("target/ip/dcmac")).unwrap();
        std::fs::write(ws.join("target/ip/dcmac/wrapper.vhd"), "").unwrap();
        // BD-generated RTL — only top-level wrappers survive the
        // walker's filter (see `keep_vivado_generated_path`).
        let bd_root = ws.join(
            "target/vw-project/scratch/scratch.gen/sources_1/bd/dcmac/hdl",
        );
        std::fs::create_dir_all(&bd_root).unwrap();
        std::fs::write(bd_root.join("dcmac_wrapper.vhd"), "").unwrap();

        let cfg = render_vhdl_ls_config(&ws, None, false).unwrap();

        assert!(
            cfg.libraries.contains_key("defaultlib"),
            "missing defaultlib: {:?}",
            cfg.libraries.keys().collect::<Vec<_>>()
        );
        assert!(
            cfg.libraries.contains_key("ip"),
            "missing ip: {:?}",
            cfg.libraries.keys().collect::<Vec<_>>()
        );
        assert!(
            cfg.libraries.contains_key("xil_defaultlib"),
            "missing xil_defaultlib: {:?}",
            cfg.libraries.keys().collect::<Vec<_>>()
        );

        let xil = &cfg.libraries["xil_defaultlib"];
        assert_eq!(xil.files.len(), 1);
        assert!(
            xil.files[0]
                .to_string_lossy()
                .ends_with("dcmac_wrapper.vhd"),
            "unexpected xil_defaultlib path: {:?}",
            xil.files[0]
        );
        assert_eq!(xil.is_third_party, Some(true));

        let ip = &cfg.libraries["ip"];
        assert_eq!(ip.files.len(), 1);
        assert!(ip.files[0].to_string_lossy().ends_with("wrapper.vhd"));

        let design = &cfg.libraries["defaultlib"];
        assert_eq!(design.files.len(), 1);
        assert!(design.files[0].to_string_lossy().ends_with("top.vhd"));
    }

    /// The renderer must produce output that vhdl_lang's TOML
    /// parser can consume — this pins the contract that a
    /// round-trip through `Config::from_str` succeeds.
    #[test]
    fn render_vhdl_lang_config_round_trips_through_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n[dependencies]\n\
             [test-dependencies]\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.join("hdl")).unwrap();
        std::fs::write(ws.join("hdl/top.vhd"), "").unwrap();
        let cfg = render_vhdl_lang_config(&ws, None).unwrap();
        // The lang config should carry the same library we
        // populated in the LS config; iterate files to confirm.
        // vhdl_lang's Config has no public library iterator, so
        // the round-trip succeeding is itself the check.
        drop(cfg);
    }

    #[test]
    fn vhdl_design_sources_walks_recursive_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let hdl = ws.join("hdl");
        std::fs::create_dir_all(hdl.join("sub")).unwrap();
        // Files under both root and a subdir; one non-VHDL to
        // prove the extension filter kicks in.
        std::fs::write(hdl.join("b.vhd"), "").unwrap();
        std::fs::write(hdl.join("a.vhd"), "").unwrap();
        std::fs::write(hdl.join("sub").join("c.vhdl"), "").unwrap();
        std::fs::write(hdl.join("readme.md"), "").unwrap();

        let sources = vhdl_design_sources(&ws).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Sorted absolute paths → `a.vhd` before `b.vhd`, and
        // `sub/c.vhdl` lands where its full path sorts. Not
        // asserting exact order across subdirs — just checking
        // both extensions and the recursion picked up the sub.
        assert!(names.contains(&"a.vhd".to_string()));
        assert!(names.contains(&"b.vhd".to_string()));
        assert!(names.contains(&"c.vhdl".to_string()));
        assert!(!names.contains(&"readme.md".to_string()));
    }

    fn make_variant_ws(tmp: &tempfile::TempDir) -> Utf8PathBuf {
        // Layout:
        //   hdl/shared.vhd        (in no variant's exclusive → always included)
        //   hdl/ethernet-vpk120.vhd   (owned by vpk120)
        //   hdl/ethernet-metro.vhd    (owned by metro)
        let ws = tmp.path().to_path_buf();
        let hdl = ws.join("hdl");
        std::fs::create_dir_all(&hdl).unwrap();
        std::fs::write(hdl.join("shared.vhd"), "").unwrap();
        std::fs::write(hdl.join("ethernet-vpk120.vhd"), "").unwrap();
        std::fs::write(hdl.join("ethernet-metro.vhd"), "").unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[[workspace.variants]]
name = "vpk120"
part = "xcvp1202-vsva2785-2MHP-e-S"
default = true
exclusive = ["hdl/ethernet-vpk120.vhd"]

[[workspace.variants]]
name = "metro"
part = "xcvp1202-vsva2785-3HP-e-S"
exclusive = ["hdl/ethernet-metro.vhd"]
"#,
        )
        .unwrap();
        Utf8PathBuf::from_path_buf(ws).unwrap()
    }

    #[test]
    fn design_sources_filter_by_active_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_variant_ws(&tmp);

        // Active variant vpk120 → keeps shared + vpk120 file,
        // excludes metro's.
        let sources =
            vhdl_design_sources_for_variant(&ws, Some("vpk120")).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"shared.vhd".to_string()));
        assert!(names.contains(&"ethernet-vpk120.vhd".to_string()));
        assert!(!names.contains(&"ethernet-metro.vhd".to_string()));

        // Flip to metro.
        let sources =
            vhdl_design_sources_for_variant(&ws, Some("metro")).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"shared.vhd".to_string()));
        assert!(names.contains(&"ethernet-metro.vhd".to_string()));
        assert!(!names.contains(&"ethernet-vpk120.vhd".to_string()));
    }

    #[test]
    fn design_sources_no_active_variant_still_filters_out_owned_files() {
        // When active_variant is None but the workspace declares
        // variants, ALL exclusive files are dropped — otherwise
        // we'd spuriously pull every variant's owned files into
        // one giant surface (which is the exact bug variants
        // exist to solve).
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_variant_ws(&tmp);
        let sources = vhdl_design_sources_for_variant(&ws, None).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["shared.vhd"]);
    }

    #[test]
    fn design_sources_no_variants_declared_returns_all() {
        // A workspace without any variants keeps the pre-variants
        // behavior — every `.vhd` under `hdl/` shows up.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join("hdl")).unwrap();
        std::fs::write(ws.join("hdl/a.vhd"), "").unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"
"#,
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources = vhdl_design_sources_for_variant(&ws_utf8, None).unwrap();
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn design_sources_variant_exclusive_supports_globs() {
        // `exclusive = ["hdl/board-vpk120/**/*.vhd"]` scopes an
        // entire subtree to a variant.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::fs::create_dir_all(ws.join("hdl/board-vpk120/sub")).unwrap();
        std::fs::create_dir_all(ws.join("hdl/board-metro")).unwrap();
        std::fs::write(ws.join("hdl/shared.vhd"), "").unwrap();
        std::fs::write(ws.join("hdl/board-vpk120/top.vhd"), "").unwrap();
        std::fs::write(ws.join("hdl/board-vpk120/sub/x.vhd"), "").unwrap();
        std::fs::write(ws.join("hdl/board-metro/top.vhd"), "").unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            r#"
[workspace]
name = "ws"
version = "0.1.0"

[[workspace.variants]]
name = "vpk120"
part = "xcvp1202-vsva2785-2MHP-e-S"
default = true
exclusive = ["hdl/board-vpk120/**/*.vhd"]

[[workspace.variants]]
name = "metro"
part = "xcvp1202-vsva2785-3HP-e-S"
exclusive = ["hdl/board-metro/**/*.vhd"]
"#,
        )
        .unwrap();
        let ws_utf8 = Utf8PathBuf::from_path_buf(ws).unwrap();
        let sources =
            vhdl_design_sources_for_variant(&ws_utf8, Some("vpk120")).unwrap();
        let names: Vec<String> = sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // shared + both vpk120 files, no metro files.
        assert_eq!(sources.len(), 3, "{names:?}");
        assert!(names.iter().any(|n| n == "shared.vhd"));
        assert!(
            names.iter().filter(|n| n.as_str() == "top.vhd").count() == 1,
            "expected exactly one top.vhd (vpk120's), got {names:?}",
        );
    }

    /// Minimal workspace scaffold for `synth_needs_update` tests:
    /// a `vw.toml`, empty `vw.lock`, one hdl file, one synth XDC,
    /// and one workspace htcl file. Returned as `Utf8PathBuf` so
    /// the caller can hand it to the enumerator directly.
    fn make_synth_ws(tmp: &tempfile::TempDir) -> Utf8PathBuf {
        let ws = tmp.path().to_path_buf();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"snws\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(ws.join("vw.lock"), "{}\n").unwrap();
        let hdl = ws.join("hdl");
        std::fs::create_dir_all(&hdl).unwrap();
        std::fs::write(hdl.join("top.vhd"), "-- vhdl\n").unwrap();
        let xdc = ws.join("constraints").join("synth");
        std::fs::create_dir_all(&xdc).unwrap();
        std::fs::write(xdc.join("timing.xdc"), "# xdc\n").unwrap();
        std::fs::write(ws.join("design.htcl"), "# htcl\n").unwrap();
        Utf8PathBuf::from_path_buf(ws).unwrap()
    }

    /// A missing checkpoint file is always stale — the whole point
    /// of the cache-check is to gate a first-time synth on this
    /// condition.
    #[test]
    fn synth_needs_update_true_when_checkpoint_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        assert!(synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// Checkpoint present but no manifest → stale (either
    /// pre-manifest era or a manually-copied checkpoint from
    /// another workspace). Forces the next synth to write both,
    /// which is the safe recovery path.
    #[test]
    fn synth_needs_update_true_when_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        std::fs::write(&cp, "").unwrap();
        assert!(synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// Manifest matches current fingerprint → fresh. Simulates
    /// the post-`vw::synth` state on an unchanged tree — no
    /// resynthesis required.
    #[test]
    fn synth_needs_update_false_when_manifest_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        std::fs::write(&cp, "").unwrap();
        write_synth_checkpoint_manifest(&ws, cp.as_std_path(), None).unwrap();
        assert!(!synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// After a checkpoint+manifest pair, rewriting a source with
    /// NEW bytes invalidates the fingerprint. This is the primary
    /// invalidation path — a real edit to a tracked file.
    #[test]
    fn synth_needs_update_true_when_source_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        std::fs::write(&cp, "").unwrap();
        write_synth_checkpoint_manifest(&ws, cp.as_std_path(), None).unwrap();
        std::fs::write(ws.join("hdl/top.vhd"), "-- vhdl updated\n").unwrap();
        assert!(synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// The regression this whole switch to content hashing fixes:
    /// rewriting a tracked file with IDENTICAL bytes must NOT
    /// invalidate the manifest. Simulates `make_wrapper`
    /// regenerating `target/ip/*/wrapper.vhd` on every design.htcl
    /// run — same stripped-header body, fresh mtime.
    #[test]
    fn synth_needs_update_false_after_identical_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        std::fs::write(&cp, "").unwrap();
        write_synth_checkpoint_manifest(&ws, cp.as_std_path(), None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Overwrite with the same bytes the fixture wrote. Fresh
        // mtime, unchanged content. Under the old mtime-based
        // check this returned `true` (stale); under content
        // hashing it stays `false`.
        std::fs::write(ws.join("hdl/top.vhd"), "-- vhdl\n").unwrap();
        assert!(!synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// Editing a workspace `.htcl` invalidates the manifest.
    /// Exercises `list_workspace_htcl_files` — a source
    /// enumerator distinct from the VHDL / XDC paths.
    #[test]
    fn synth_needs_update_true_when_htcl_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_synth_ws(&tmp);
        let cp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        std::fs::write(&cp, "").unwrap();
        write_synth_checkpoint_manifest(&ws, cp.as_std_path(), None).unwrap();
        std::fs::write(ws.join("design.htcl"), "# htcl updated\n").unwrap();
        assert!(synth_needs_update(&ws, cp.as_std_path(), None).unwrap());
    }

    /// Minimal workspace scaffold for `project_needs_wipe` tests:
    /// a `vw.toml` and an `ip/module.htcl` (+ one submodule so
    /// the recursive walk has something to cover).
    fn make_project_ws(tmp: &tempfile::TempDir) -> Utf8PathBuf {
        let ws = tmp.path().to_path_buf();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"prws\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let ip = ws.join("ip");
        std::fs::create_dir_all(&ip).unwrap();
        std::fs::write(
            ip.join("module.htcl"),
            "namespace eval ip { proc configure {} unit {} }\n",
        )
        .unwrap();
        std::fs::write(ip.join("cips.htcl"), "# cips\n").unwrap();
        Utf8PathBuf::from_path_buf(ws).unwrap()
    }

    /// Materialize `<project_dir>/<name>/<name>.xpr` as an empty
    /// placeholder — `project_needs_wipe` short-circuits when
    /// the `.xpr` is missing, so tests that want to exercise the
    /// manifest branch need one present.
    fn touch_placeholder_xpr(ws: &Utf8Path, name: &str) -> PathBuf {
        let project_dir = vw_project_dir(ws);
        let inner = project_dir.join(name);
        std::fs::create_dir_all(inner.as_std_path()).unwrap();
        let xpr = inner.join(format!("{name}.xpr"));
        std::fs::write(xpr.as_std_path(), "").unwrap();
        project_dir.into_std_path_buf()
    }

    /// A lockfile serialized twice from equal contents must produce identical
    /// bytes.
    ///
    /// This is not a style preference. `vw.lock` is rewritten whenever
    /// dependencies are fetched, and its bytes feed the synth checkpoint's
    /// source fingerprint — so a map that iterates in a different order each
    /// process meant every fetch silently invalidated the checkpoint and threw
    /// away an hour of synthesis. Nobody saw it locally, because a warm
    /// `~/.vw/deps` means the file is never rewritten; CI re-fetches on every
    /// job and hit it every time.
    #[test]
    fn a_lockfile_serializes_the_same_way_every_time() {
        let dep = |n: &str| LockedDependency {
            repo: format!("oxidecomputer/{n}"),
            branch: Some("main".to_owned()),
            commit: "0f7c1d2e".to_owned(),
            src: Vec::new(),
            path: PathBuf::from(format!("/deps/{n}")),
            recursive: false,
            sim_only: false,
            submodules: false,
            exclude: Vec::new(),
        };
        // Enough entries that a randomized order would almost certainly differ
        // between two builds of the same map.
        let names = [
            "ipe",
            "anodizer",
            "rust_hdl",
            "vivado-cmd",
            "clk-wizard",
            "cips",
            "dcmac",
            "cpm5",
            "gtwiz-versal",
            "vw",
            "vhdl-dungeon",
            "redhawk",
        ];

        let build = || LockFile {
            dependencies: names
                .iter()
                .map(|n| ((*n).to_owned(), dep(n)))
                .collect(),
        };

        let first = toml::to_string_pretty(&build()).expect("serialize");
        let second = toml::to_string_pretty(&build()).expect("serialize");

        assert_eq!(first, second, "same contents must give the same bytes");

        // And the order is the sorted one, so it is stable across processes
        // and not merely stable within this one.
        let order: Vec<&str> = first
            .lines()
            .filter_map(|l| l.strip_prefix("[dependencies."))
            .filter_map(|l| l.strip_suffix(']'))
            .collect();
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "entries should be in sorted order");
    }

    /// Each way a checkpoint can fail to be reused has to be told apart from
    /// the others. They are indistinguishable from outside the process, which
    /// is the whole reason for reporting them: "synth ran again" has four
    /// causes and only one of them means the sources actually changed.
    #[test]
    fn every_reason_a_checkpoint_is_not_reused_is_distinguishable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let dcp = dir.join("top.dcp");
        let manifest = dir.join("top.dcp.manifest");

        // Nothing there at all.
        assert_eq!(
            checkpoint_status(dcp.as_std_path(), 42),
            CheckpointStatus::Missing,
        );

        // Written but never marked — the state a stage leaves behind when it
        // produced a checkpoint and did not record what it was built from.
        std::fs::write(dcp.as_std_path(), "checkpoint").unwrap();
        assert_eq!(
            checkpoint_status(dcp.as_std_path(), 42),
            CheckpointStatus::Unmarked,
        );

        // Marked with something that is not a fingerprint.
        std::fs::write(manifest.as_std_path(), "not a number").unwrap();
        assert_eq!(
            checkpoint_status(dcp.as_std_path(), 42),
            CheckpointStatus::Unreadable,
        );

        // Marked against different sources.
        std::fs::write(manifest.as_std_path(), "7").unwrap();
        assert_eq!(
            checkpoint_status(dcp.as_std_path(), 42),
            CheckpointStatus::Stale {
                stored: 7,
                current: 42
            },
        );

        // And the one case where the stage gets skipped.
        std::fs::write(manifest.as_std_path(), "42").unwrap();
        assert_eq!(
            checkpoint_status(dcp.as_std_path(), 42),
            CheckpointStatus::Fresh,
        );
    }

    /// The bool the flow acts on has to keep agreeing with the reason, or the
    /// report would explain a decision that was not the one taken.
    #[test]
    fn the_reported_reason_agrees_with_the_decision() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let dcp = dir.join("top.dcp");

        for (fp, setup) in [
            (42u64, None),
            (42, Some(("42", true))),
            (42, Some(("7", true))),
        ] {
            if let Some((stored, present)) = setup {
                if present {
                    std::fs::write(dcp.as_std_path(), "checkpoint").unwrap();
                }
                std::fs::write(
                    dir.join("top.dcp.manifest").as_std_path(),
                    stored,
                )
                .unwrap();
            }
            let status = checkpoint_status(dcp.as_std_path(), fp);
            let needs =
                checkpoint_needs_update_with_fingerprint(dcp.as_std_path(), fp);
            assert_eq!(
                needs,
                status != CheckpointStatus::Fresh,
                "reason {status} disagrees with needs_update={needs}",
            );
        }
    }

    /// Missing `.xpr` → always needs wipe (first-time-project
    /// bootstrap). The manifest presence is irrelevant when the
    /// `.xpr` isn't there — nothing to open.
    #[test]
    fn project_needs_wipe_true_when_xpr_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = vw_project_dir(&ws);
        assert!(
            project_needs_wipe(&ws, project_dir.as_std_path(), "prws").unwrap()
        );
    }

    /// `.xpr` present but manifest missing → wipe. This is the
    /// "someone deleted the sidecar" or "pre-manifest-era
    /// project" recovery path.
    #[test]
    fn project_needs_wipe_true_when_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        assert!(project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// Fresh manifest matching current fingerprint → do NOT
    /// wipe. Simulates the post-`vw::configure_ip` state on an
    /// unchanged tree.
    #[test]
    fn project_needs_wipe_false_when_manifest_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        write_project_manifest(&ws, &project_dir, "prws").unwrap();
        assert!(!project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// Editing any `.htcl` under `<ws>/ip/` invalidates. Key
    /// test — the invalidation trigger the user actually
    /// controls (adding an IP, tweaking a configure_* parameter).
    #[test]
    fn project_needs_wipe_true_when_ip_htcl_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        write_project_manifest(&ws, &project_dir, "prws").unwrap();
        std::fs::write(ws.join("ip/cips.htcl"), "# cips updated\n").unwrap();
        assert!(project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// Editing `vw.toml` (target-part, deps list, etc.) also
    /// invalidates — those changes usually mean the project
    /// itself needs a fresh `create_project -part ...`.
    #[test]
    fn project_needs_wipe_true_when_vw_toml_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        write_project_manifest(&ws, &project_dir, "prws").unwrap();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"prws\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        assert!(project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// Editing a workspace htcl OUTSIDE ip/ (e.g. design.htcl)
    /// must NOT invalidate the on-disk project. Design-level
    /// changes are the synth checkpoint's concern; the project
    /// scope stays narrow so we don't nuke the expensive BD/IP
    /// state on every design edit.
    #[test]
    fn project_needs_wipe_false_when_non_ip_htcl_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        std::fs::write(ws.join("design.htcl"), "# design\n").unwrap();
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        write_project_manifest(&ws, &project_dir, "prws").unwrap();
        std::fs::write(ws.join("design.htcl"), "# design updated\n").unwrap();
        assert!(!project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// The regression parallel to the synth case: rewriting an
    /// `ip/*.htcl` with IDENTICAL bytes must not invalidate.
    /// Content-hash based, not mtime.
    #[test]
    fn project_needs_wipe_false_after_identical_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = make_project_ws(&tmp);
        let project_dir = touch_placeholder_xpr(&ws, "prws");
        write_project_manifest(&ws, &project_dir, "prws").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(ws.join("ip/cips.htcl"), "# cips\n").unwrap();
        assert!(!project_needs_wipe(&ws, &project_dir, "prws").unwrap());
    }

    /// Minimal workspace scaffold for `place_needs_update` tests:
    /// a `vw.toml`, one place-scoped XDC, and a stand-in synth DCP
    /// that the fingerprint folds in as a proxy for "everything
    /// synth depended on".
    fn make_place_ws(
        tmp: &tempfile::TempDir,
    ) -> (Utf8PathBuf, PathBuf, PathBuf) {
        let ws = tmp.path().to_path_buf();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"plws\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let xdc = ws.join("constraints").join("place");
        std::fs::create_dir_all(&xdc).unwrap();
        std::fs::write(xdc.join("place.xdc"), "# place xdc\n").unwrap();
        let synth_dcp = ws.join("target/synth/top.dcp");
        std::fs::create_dir_all(synth_dcp.parent().unwrap()).unwrap();
        std::fs::write(&synth_dcp, "").unwrap();
        let place_dcp = ws.join("target/place/top.dcp");
        (
            Utf8PathBuf::from_path_buf(ws).unwrap(),
            place_dcp,
            synth_dcp,
        )
    }

    /// Missing checkpoint → stale. Same first-place rule the
    /// synth/ip caches use.
    #[test]
    fn place_needs_update_true_when_checkpoint_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, place_dcp, synth_dcp) = make_place_ws(&tmp);
        assert!(place_needs_update(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path()
        )
        .unwrap());
    }

    /// Fresh manifest → not stale. Verifies the write+check
    /// round-trip on the place scope (place XDCs + synth DCP).
    #[test]
    fn place_needs_update_false_when_manifest_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, place_dcp, synth_dcp) = make_place_ws(&tmp);
        std::fs::create_dir_all(place_dcp.parent().unwrap()).unwrap();
        std::fs::write(&place_dcp, "").unwrap();
        write_place_checkpoint_manifest(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path(),
        )
        .unwrap();
        assert!(!place_needs_update(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path()
        )
        .unwrap());
    }

    /// Editing a place XDC invalidates. Trigger the user
    /// controls most directly (tweak a place constraint,
    /// re-place should fire).
    #[test]
    fn place_needs_update_true_when_place_xdc_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, place_dcp, synth_dcp) = make_place_ws(&tmp);
        std::fs::create_dir_all(place_dcp.parent().unwrap()).unwrap();
        std::fs::write(&place_dcp, "").unwrap();
        write_place_checkpoint_manifest(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(
            ws.join("constraints/place/place.xdc"),
            "# place xdc updated\n",
        )
        .unwrap();
        assert!(place_needs_update(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path()
        )
        .unwrap());
    }

    /// Synth re-ran → synth DCP content changed → place
    /// invalidates. The synth DCP is intentionally folded into
    /// the place fingerprint as a proxy for "everything synth
    /// depended on".
    #[test]
    fn place_needs_update_true_when_synth_checkpoint_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, place_dcp, synth_dcp) = make_place_ws(&tmp);
        std::fs::create_dir_all(place_dcp.parent().unwrap()).unwrap();
        std::fs::write(&place_dcp, "").unwrap();
        write_place_checkpoint_manifest(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(&synth_dcp, "different bytes").unwrap();
        assert!(place_needs_update(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path()
        )
        .unwrap());
    }

    /// Non-place workspace file changes must NOT invalidate the
    /// place cache (design.htcl, hdl/, etc. are captured by the
    /// synth stage; place scope is narrower).
    #[test]
    fn place_needs_update_false_when_non_place_htcl_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, place_dcp, synth_dcp) = make_place_ws(&tmp);
        std::fs::write(ws.join("design.htcl"), "# design\n").unwrap();
        std::fs::create_dir_all(place_dcp.parent().unwrap()).unwrap();
        std::fs::write(&place_dcp, "").unwrap();
        write_place_checkpoint_manifest(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(ws.join("design.htcl"), "# design updated\n").unwrap();
        assert!(!place_needs_update(
            &ws,
            place_dcp.as_path(),
            synth_dcp.as_path()
        )
        .unwrap());
    }

    /// Minimal workspace scaffold for `route_needs_update` tests:
    /// a `vw.toml`, one route-scoped XDC, and a stand-in place DCP
    /// that the fingerprint folds in as a proxy for "everything
    /// place depended on".
    fn make_route_ws(
        tmp: &tempfile::TempDir,
    ) -> (Utf8PathBuf, PathBuf, PathBuf) {
        let ws = tmp.path().to_path_buf();
        std::fs::write(
            ws.join("vw.toml"),
            "[workspace]\nname = \"rtws\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let xdc = ws.join("constraints").join("route");
        std::fs::create_dir_all(&xdc).unwrap();
        std::fs::write(xdc.join("route.xdc"), "# route xdc\n").unwrap();
        let place_dcp = ws.join("target/place/top.dcp");
        std::fs::create_dir_all(place_dcp.parent().unwrap()).unwrap();
        std::fs::write(&place_dcp, "").unwrap();
        let route_dcp = ws.join("target/route/top.dcp");
        (
            Utf8PathBuf::from_path_buf(ws).unwrap(),
            route_dcp,
            place_dcp,
        )
    }

    /// Missing checkpoint → stale. Same first-place rule the
    /// place / synth caches use.
    #[test]
    fn route_needs_update_true_when_checkpoint_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, route_dcp, place_dcp) = make_route_ws(&tmp);
        assert!(route_needs_update(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path()
        )
        .unwrap());
    }

    /// Fresh manifest → not stale. Verifies the write+check
    /// round-trip on the route scope (route XDCs + place DCP).
    #[test]
    fn route_needs_update_false_when_manifest_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, route_dcp, place_dcp) = make_route_ws(&tmp);
        std::fs::create_dir_all(route_dcp.parent().unwrap()).unwrap();
        std::fs::write(&route_dcp, "").unwrap();
        write_route_checkpoint_manifest(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path(),
        )
        .unwrap();
        assert!(!route_needs_update(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path()
        )
        .unwrap());
    }

    /// Editing a route XDC invalidates. Trigger the user
    /// controls most directly (tweak a route constraint,
    /// re-route should fire).
    #[test]
    fn route_needs_update_true_when_route_xdc_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, route_dcp, place_dcp) = make_route_ws(&tmp);
        std::fs::create_dir_all(route_dcp.parent().unwrap()).unwrap();
        std::fs::write(&route_dcp, "").unwrap();
        write_route_checkpoint_manifest(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(
            ws.join("constraints/route/route.xdc"),
            "# route xdc updated\n",
        )
        .unwrap();
        assert!(route_needs_update(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path()
        )
        .unwrap());
    }

    /// Place re-ran → place DCP content changed → route
    /// invalidates. The place DCP is intentionally folded into
    /// the route fingerprint as a proxy for "everything place
    /// depended on".
    #[test]
    fn route_needs_update_true_when_place_checkpoint_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, route_dcp, place_dcp) = make_route_ws(&tmp);
        std::fs::create_dir_all(route_dcp.parent().unwrap()).unwrap();
        std::fs::write(&route_dcp, "").unwrap();
        write_route_checkpoint_manifest(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(&place_dcp, "different bytes").unwrap();
        assert!(route_needs_update(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path()
        )
        .unwrap());
    }

    /// Non-route workspace file changes must NOT invalidate the
    /// route cache — synth / place XDC edits belong to those
    /// stages (and reach route via the DCP-proxy chain).
    #[test]
    fn route_needs_update_false_when_non_route_xdc_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (ws, route_dcp, place_dcp) = make_route_ws(&tmp);
        let place_xdc_dir = ws.join("constraints").join("place");
        std::fs::create_dir_all(&place_xdc_dir).unwrap();
        std::fs::write(place_xdc_dir.join("place.xdc"), "# place\n").unwrap();
        std::fs::create_dir_all(route_dcp.parent().unwrap()).unwrap();
        std::fs::write(&route_dcp, "").unwrap();
        write_route_checkpoint_manifest(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path(),
        )
        .unwrap();
        std::fs::write(place_xdc_dir.join("place.xdc"), "# place updated\n")
            .unwrap();
        assert!(!route_needs_update(
            &ws,
            route_dcp.as_path(),
            place_dcp.as_path()
        )
        .unwrap());
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;

    /// Build a repository whose default branch does NOT contain the commit
    /// being pinned, and return `(url, pinned, tip)`.
    ///
    /// A stand-in for the real failure, which cannot be staged locally:
    /// libgit2 ignores shallow depth over the local transport, so a `depth(1)`
    /// clone of a `file://` remote quietly brings the whole history and the
    /// truncation never happens. Putting the pin off the default branch
    /// instead makes cloning-then-searching miss it for a different reason,
    /// which exercises the same difference — asking the remote for the commit
    /// by name rather than hoping a cloned branch happens to contain it.
    fn repo_with_pin_off_the_default_branch(
        root: &Utf8Path,
    ) -> (String, String, String) {
        let repo = root.join("origin");
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "vw")
                .env("GIT_AUTHOR_EMAIL", "vw@example.invalid")
                .env("GIT_COMMITTER_NAME", "vw")
                .env("GIT_COMMITTER_EMAIL", "vw@example.invalid")
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };

        git(&["init", "--quiet", "--initial-branch=main", "."]);
        fs::write(repo.join("root.htcl"), "## root\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "root"]);

        // The pinned revision, on a branch that is then removed: the object
        // survives, but nothing under refs/heads leads to it.
        git(&["checkout", "--quiet", "-b", "pinned"]);
        fs::write(repo.join("module.htcl"), "## pinned\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "pinned"]);
        let pinned = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/pins/held", &pinned]);

        git(&["checkout", "--quiet", "main"]);
        fs::write(repo.join("later.htcl"), "## later\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "later"]);
        let tip = git(&["rev-parse", "HEAD"]);
        git(&["branch", "--quiet", "-D", "pinned"]);

        (format!("file://{repo}"), pinned, tip)
    }

    /// What lands in the cache is the pinned revision, not whatever the
    /// branch has moved on to.
    ///
    /// Note what this does NOT establish. The failure it was written for --
    /// a `depth(1)` clone bringing only the branch tip, so the pinned commit
    /// is missing -- cannot be staged against a local remote at all: libgit2
    /// serves `file://` through its local transport, which copies the whole
    /// object database and ignores the requested depth, so every commit is
    /// present however the download asks for it. Confirmed by putting the old
    /// implementation back and watching this pass unchanged. The truncation
    /// itself is covered by
    /// [`a_commit_behind_the_branch_tip_downloads_from_a_real_remote`], which
    /// needs the network and is ignored by default.
    #[tokio::test]
    async fn the_pinned_revision_is_what_lands() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let (url, first, tip) = repo_with_pin_off_the_default_branch(root);
        assert_ne!(first, tip);

        let dest = root.join("cache");
        download_dependency(
            &url,
            &first,
            &[],
            dest.as_std_path(),
            false,
            &[],
            false,
            None,
            None,
        )
        .await
        .expect("an older commit is still fetchable");

        assert!(
            dest.join("module.htcl").exists(),
            "the pinned tree is there"
        );
        assert!(
            !dest.join("later.htcl").exists(),
            "and it is the pinned revision, not the tip"
        );
    }

    /// A pin that no longer exists upstream — force-pushed away, or simply
    /// mistyped — is the one case that cannot be satisfied, and the message
    /// has to say which dependency and what to do rather than "object not
    /// found ... class=Odb".
    #[tokio::test]
    async fn a_commit_that_is_not_there_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let (url, _, _) = repo_with_pin_off_the_default_branch(root);

        let e = download_dependency(
            &url,
            "0000000000000000000000000000000000000000",
            &[],
            root.join("cache").as_std_path(),
            false,
            &[],
            false,
            None,
            None,
        )
        .await
        .expect_err("a commit that is not there cannot be downloaded");

        let message = e.to_string();
        assert!(
            message.contains("vw update")
                || message.contains("Failed to fetch"),
            "expected an actionable message, got: {message}"
        );
    }

    /// The regression, against a real remote: a dependency pinned behind its
    /// branch tip still downloads.
    ///
    /// Ignored because it needs the network, and run deliberately:
    ///
    /// ```text
    /// cargo test -p vw-lib -- --ignored
    /// ```
    ///
    /// The fixture is the failure as reported -- `quartz` pinned at a commit
    /// that was its `main` when `vw.lock` was written and was several commits
    /// back by the time anybody fetched it. A public repository, so no
    /// credentials are needed; if it is ever rewritten this test starts
    /// failing for a reason that is not a regression, and the pin below is
    /// what to update.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn a_commit_behind_the_branch_tip_downloads_from_a_real_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let dest = root.join("cache");

        download_dependency(
            "https://github.com/oxidecomputer/quartz",
            "c13739a3a61455fb323ed914d7685cdf40671f04",
            &["hdl/ip/vhd/synchronizers".to_owned()],
            dest.as_std_path(),
            false,
            &[],
            false,
            None,
            None,
        )
        .await
        .expect("a commit behind the tip is still fetchable");

        assert!(
            dest.join("meta_sync.vhd").exists(),
            "the pinned revision's sources are in the cache"
        );
    }
}
