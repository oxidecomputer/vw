// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Per-IP TOML overrides for typed-constructor field emission.
//!
//! The generator's default source of field metadata is the IP-XACT
//! `<spirit:value>` paired-list default — good for populating
//! `@default(...)` but silent on bounded vocabularies (Vivado wants
//! `RX_PAM_SEL` to be one of `NRZ` / `PAM4`, but the XML default is
//! just a bare string). An `overrides.toml` file colocated with each
//! IP's `regenerate.sh` refines the emitted surface where the XML
//! is silent: attach `@enum(…)` restrictions to specific fields,
//! override the XML default, etc.
//!
//! Discovery: the CLI accepts `--overrides <path>` and threads the
//! parsed [`OverridesFile`] through `GenerateOptions`. When no
//! override file exists, the generator falls back to XML-only
//! defaults (schema shape derived entirely from `<spirit:value>`).
//!
//! File shape:
//!
//! ```toml
//! [shapes."intf::gt_settings::lr0_settings"]
//! fields.rx_pam_sel       = { enum = ["NRZ", "PAM4"] }
//! fields.rx_refclk_source = { enum = ["R0", "R1", "R2", "R3", "R4", "R5", "ERR"] }
//! fields.rx_line_rate     = { default = "10.3125" }
//! ```
//!
//! `shape_path` uses `::`-separated segments matching the emitted
//! proc's namespace path *below* the IP name — i.e. how a caller
//! writes `gtwiz_versal::intf::gt_settings::lr0_settings` refers to
//! shape `"intf::gt_settings::lr0_settings"`. Per-N sub-newtypes
//! (`intf0` vs `intf1`) share their shape's overrides — the emitter
//! matches on the stem (`intf`), not the indexed instance.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Errors produced by [`OverridesFile::load_from`].
#[derive(Debug, thiserror::Error)]
pub enum OverridesError {
    /// The path exists but couldn't be read (permissions, IO error).
    #[error("reading overrides file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The file exists but doesn't parse as valid TOML matching the
    /// override schema.
    #[error("parsing overrides file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

/// The parsed overrides file. `shapes["intf::gt_settings::lr0_settings"]`
/// looks up refinements for one emitted proc's field list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverridesFile {
    #[serde(default)]
    pub shapes: HashMap<String, ShapeOverrides>,
}

impl OverridesFile {
    /// Load an overrides file from `path`. Missing file is not an
    /// error — returns an empty overrides object so callers can pass
    /// the result through unconditionally.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, OverridesError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| {
            OverridesError::Read {
                path: path.display().to_string(),
                source,
            }
        })?;
        toml::from_str(&text).map_err(|source| OverridesError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Look up a field's override in `shape_path`. `None` when the
    /// shape isn't listed, or the shape is listed but the field
    /// isn't. Callers merge with the XML-derived default when present.
    ///
    /// Field names are matched exactly as the emitter writes them
    /// (lowercase form of the XML key — e.g. XML `RX_PAM_SEL`
    /// becomes field key `rx_pam_sel`).
    pub fn field(
        &self,
        shape_path: &str,
        field_name: &str,
    ) -> Option<&FieldOverride> {
        self.shapes.get(shape_path)?.fields.get(field_name)
    }

    /// True when the shape at `shape_path` sets shape-wide
    /// `baseline = true`. Missing shape → false. Callers use this
    /// to force `@baseline(...)` emission on every field in the
    /// shape, regardless of per-field configuration.
    pub fn shape_baseline(&self, shape_path: &str) -> bool {
        self.shapes.get(shape_path).is_some_and(|s| s.baseline)
    }

    /// True when this override set is empty — no shapes registered.
    /// Callers use this to skip the whole override-application pass
    /// when there's nothing to do (fast path for IPs without an
    /// override file).
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }
}

/// Overrides scoped to one emitted proc / shape.
///
/// `baseline` at this level is the shape-wide switch: when true,
/// every field in the shape gets `@baseline(...)` instead of
/// `@default(...)`. Use it when the containing constructor takes a
/// `-preset` (or other mutation source) that can shift ANY of the
/// shape's fields — enumerating each field with per-field
/// `baseline = true` would be tedious and easy to leave stale as
/// new fields are generated. Per-field `baseline = false` cannot
/// override the shape-level `true` (there's no "opt-out" — if the
/// shape is baseline-wide, individual fields are too).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShapeOverrides {
    #[serde(default)]
    pub fields: HashMap<String, FieldOverride>,
    /// Shape-wide baseline switch — see the type-level docs.
    #[serde(default)]
    pub baseline: bool,
}

/// Per-field refinement applied on top of the XML-derived schema.
///
/// - `enum_values`: attach `@enum(v1, v2, …)` to the emitted arg,
///   restricting valid callsite values. Overrides absent → no
///   enum annotation.
/// - `default`: replace the XML default. Rare — the XML is usually
///   authoritative, but Xilinx sometimes ships defaults that are
///   sentinels rather than useful values (e.g. `NA NA`). Overrides
///   absent → use XML value.
/// - `baseline`: emit `@baseline(<value>)` instead of `@default(<value>)`.
///   The field's runtime shape is unchanged (the wrapper uses the same
///   `if kw_set { dict set }` gate either way), but the annotation
///   documents that the value is the ParamInfo baseline — the value a
///   FRESH IP would report, which a preset / board_interface / prior
///   configuration can shift out from under omission. Callers that
///   explicitly pass the baseline value are NOT flagged by the
///   redundant-default lint under this variant. Use for scalar knobs
///   downstream of a `-preset`-carrying constructor where omitting the
///   flag doesn't pin the value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FieldOverride {
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub baseline: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn missing_file_yields_empty() {
        let path =
            std::path::Path::new("/nonexistent-vw-ip-test-overrides.toml");
        assert!(!path.exists());
        let ov = OverridesFile::load_from(path).unwrap();
        assert!(ov.is_empty());
    }

    #[test]
    fn empty_file_yields_empty_shapes_map() {
        let f = write_tmp("");
        let ov = OverridesFile::load_from(f.path()).unwrap();
        assert!(ov.is_empty());
    }

    #[test]
    fn single_shape_single_field_enum() {
        let f = write_tmp(
            r#"
[shapes."intf::gt_settings::lr0_settings"]
fields.rx_pam_sel = { enum = ["NRZ", "PAM4"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        let field = ov
            .field("intf::gt_settings::lr0_settings", "rx_pam_sel")
            .expect("field present");
        assert_eq!(
            field.enum_values.as_deref(),
            Some(&["NRZ".to_string(), "PAM4".to_string()][..])
        );
        assert!(field.default.is_none());
    }

    #[test]
    fn field_default_override() {
        let f = write_tmp(
            r#"
[shapes."intf::lr0_settings"]
fields.rx_line_rate = { default = "10.3125" }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        let field = ov
            .field("intf::lr0_settings", "rx_line_rate")
            .expect("field present");
        assert_eq!(field.default.as_deref(), Some("10.3125"));
        assert!(field.enum_values.is_none());
    }

    #[test]
    fn field_baseline_flag_parses() {
        let f = write_tmp(
            r#"
[shapes."intf::gt_settings::lr0_settings"]
fields.rx_refclk_frequency = { baseline = true }
fields.tx_refclk_frequency = { baseline = true, enum = ["156.25", "161.13"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        let rx = ov
            .field("intf::gt_settings::lr0_settings", "rx_refclk_frequency")
            .expect("rx field present");
        assert!(rx.baseline);
        assert!(rx.default.is_none());
        assert!(rx.enum_values.is_none());
        let tx = ov
            .field("intf::gt_settings::lr0_settings", "tx_refclk_frequency")
            .expect("tx field present");
        assert!(tx.baseline);
        assert_eq!(
            tx.enum_values.as_deref(),
            Some(&["156.25".to_string(), "161.13".to_string()][..])
        );
    }

    #[test]
    fn shape_level_baseline_flag_parses() {
        // The shape-wide baseline switch flips every field in the
        // shape without needing per-field entries. Useful when the
        // containing constructor takes a `-preset` that can shift
        // any of the shape's fields.
        let f = write_tmp(
            r#"
[shapes."intf::lr0_settings"]
baseline = true
fields.rx_pam_sel = { enum = ["NRZ", "PAM4"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        assert!(ov.shape_baseline("intf::lr0_settings"));
        assert!(!ov.shape_baseline("intf::other_shape"));
        // Per-field override still parses alongside the shape flag.
        let field = ov.field("intf::lr0_settings", "rx_pam_sel").unwrap();
        assert!(field.enum_values.is_some());
    }

    #[test]
    fn shape_baseline_defaults_to_false() {
        let f = write_tmp(
            r#"
[shapes."intf::lr0_settings"]
fields.rx_pam_sel = { enum = ["NRZ", "PAM4"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        assert!(!ov.shape_baseline("intf::lr0_settings"));
    }

    #[test]
    fn baseline_defaults_to_false() {
        // Existing override files that don't set `baseline` continue
        // to emit `@default(...)` unchanged.
        let f = write_tmp(
            r#"
[shapes."intf::lr0_settings"]
fields.rx_pam_sel = { enum = ["NRZ", "PAM4"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        let field = ov.field("intf::lr0_settings", "rx_pam_sel").unwrap();
        assert!(!field.baseline);
    }

    #[test]
    fn missing_shape_returns_none() {
        let f = write_tmp(
            r#"[shapes."intf::gt_settings"]
fields.dummy = { enum = ["A", "B"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        assert!(ov.field("intf::channel_map", "dummy").is_none());
    }

    #[test]
    fn missing_field_within_present_shape_returns_none() {
        let f = write_tmp(
            r#"[shapes."intf::gt_settings"]
fields.dummy = { enum = ["A", "B"] }
"#,
        );
        let ov = OverridesFile::load_from(f.path()).unwrap();
        assert!(ov.field("intf::gt_settings", "not_dummy").is_none());
    }

    #[test]
    fn malformed_toml_errors_with_path_context() {
        let f = write_tmp("this is not [ valid toml");
        let err = OverridesFile::load_from(f.path()).unwrap_err();
        // The error message should mention the path so a user seeing
        // it in `vw` output can find the file that needs fixing.
        assert!(
            err.to_string().contains(&f.path().display().to_string()),
            "err: {err}"
        );
    }
}
