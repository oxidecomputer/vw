// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Schema loader for Xilinx's `structured_tcldict` IP-XACT parameters.
//!
//! Some IP-XACT parameters in CIPS-family components are declared with
//! `<xilinx:parameterType>structured_tcldict</xilinx:parameterType>`:
//! the IP-XACT value is just an opaque space-separated `KEY VAL …`
//! dict string. The real schema for those inner fields lives in
//! out-of-band data files Vivado ships:
//!
//! - `versal/flows/automation/cipsToPsWiz_Porting/csv_files/`
//!     - `param_mapping_direct.csv` — `(KEY, {DEFAULT}, …)` per row.
//!     - `param_mapping_presets.csv` — preset-bundle layout for
//!       `mode`-style selector fields like `CLOCK_MODE`, `BOOT_MODE`.
//! - `versal/cips_hip/<domain>/guidata/ParamInfo.xml` — per-field
//!   `<displayName>` text, used as a doc comment.
//! - `versal/cips_hip/<domain>/global/global_preset*.xml` and
//!   `versal/cips_hip/<domain>/presets/**/*.xml` — `<preset param=
//!   name=/>` entries used to widen `@enum(…)` lists, same format
//!   already parsed by [`crate::presets`].
//!
//! We deliberately ignore the deprecated
//! `flows/automation/deprecated/cips_pswiz_key_and_value.csv` — its
//! content is a subset of the two supported CSVs above.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DictSchema {
    pub fields: Vec<DictField>,
}

#[derive(Debug, Clone)]
pub struct DictField {
    /// IP-XACT-style upper-snake name, e.g. `PCIE_APERTURES_DUAL_ENABLE`.
    pub name: String,
    /// Default value as recorded in the supporting data files. May be
    /// empty when no default is known (rare); the generator treats it
    /// the same as any other defaultless arg.
    pub default: String,
    /// Display-name / one-line description from `ParamInfo.xml`, when
    /// present.
    pub description: Option<String>,
    /// `@enum(…)` choices we were able to recover from preset files
    /// or from `param_mapping_presets.csv`. Empty when no enum data
    /// was found.
    pub enum_values: BTreeSet<String>,
}

/// Returns the schema for each `structured_tcldict` parameter we can
/// find data for. Keys are the IP-XACT parameter names
/// (`PS_PMC_CONFIG`, …); the matching `_INTERNAL` variants point at
/// the same schema.
///
/// Empty when the Xilinx `data/` ancestor can't be located.
pub fn load_schemas(component_path: &Path) -> HashMap<String, DictSchema> {
    let mut out = HashMap::new();
    let Some(data_root) = find_data_root(component_path) else {
        return out;
    };
    if let Some(schema) = load_ps_pmc_schema(&data_root) {
        out.insert("PS_PMC_CONFIG".to_string(), schema.clone());
        out.insert("PS_PMC_CONFIG_INTERNAL".to_string(), schema);
    }
    out
}

/// Inputs scoped to a CIPS `PS_PMC_CONFIG`.
fn load_ps_pmc_schema(data_root: &Path) -> Option<DictSchema> {
    let pspmc = data_root.join("versal/cips_hip/pspmc");
    let csv_dir =
        data_root.join("versal/flows/automation/cipsToPsWiz_Porting/csv_files");
    if !pspmc.is_dir() || !csv_dir.is_dir() {
        return None;
    }

    let mut fields: HashMap<String, DictField> = HashMap::new();
    parse_direct_csv(&csv_dir.join("param_mapping_direct.csv"), &mut fields);
    parse_presets_csv(&csv_dir.join("param_mapping_presets.csv"), &mut fields);

    // Drop keys that belong to a different `structured_tcldict`.
    fields.retain(|name, _| {
        !name.starts_with("CPM_")
            && !name.starts_with("XRAM_")
            && !is_cips_toplevel(name)
    });
    if fields.is_empty() {
        return None;
    }

    layer_param_info(&pspmc.join("guidata/ParamInfo.xml"), &mut fields);
    layer_presets(&pspmc, &mut fields);

    let mut sorted: Vec<DictField> = fields.into_values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    Some(DictSchema { fields: sorted })
}

/// Names of `<spirit:parameter>` entries that live at the top level of
/// the CIPS IP-XACT — we don't want to re-emit them as inner dict
/// fields. (Recovered by `vw ip generate` separately, but easier to
/// hard-code the small list than to thread the IP-XACT through here.)
fn is_cips_toplevel(name: &str) -> bool {
    matches!(
        name,
        "AURORA_LINE_RATE_GPBS"
            | "BOOT_SECONDARY_PCIE_ENABLE"
            | "Component_Name"
            | "GT_REFCLK_MHZ"
            | "PMC_REF_CLK_FREQMHZ"
            | "PS_PMC_CONFIG"
            | "PS_PMC_CONFIG_INTERNAL"
            | "PS_PMC_CONFIG_APPLIED"
            | "CPM_CONFIG"
            | "CPM_CONFIG_INTERNAL"
            | "XRAM_CONFIG"
            | "XRAM_CONFIG_INTERNAL"
    )
}

/// `param_mapping_direct.csv` layout: `<CIPS_KEY>,{CIPS_DEFAULT},<PSWIZ_KEY>,{PSWIZ_DEFAULT}`.
/// The `{…}` value cells routinely contain commas (Tcl list syntax),
/// so we tokenize comma-separated columns at brace depth 0 rather than
/// splitting on every comma. Rows whose value has unbalanced braces
/// (the Xilinx CSV does ship a handful of those — line-wrapped or
/// truncated by the vendor) keep the field name but no default.
fn parse_direct_csv(path: &Path, fields: &mut HashMap<String, DictField>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    for line in text.lines() {
        let cols = split_brace_aware(line);
        let (Some(key), Some(raw_default)) = (
            cols.first().map(|s| s.trim()),
            cols.get(1).map(|s| s.trim()),
        ) else {
            continue;
        };
        if key.is_empty() || !is_safe_key(key) {
            continue;
        }
        let stripped = unwrap_one_brace(raw_default);
        let default = if braces_balanced(stripped) {
            stripped.to_string()
        } else {
            String::new()
        };
        fields.entry(key.to_string()).or_insert_with(|| DictField {
            name: key.to_string(),
            default,
            description: None,
            enum_values: BTreeSet::new(),
        });
    }
}

/// Split a CSV row on commas at brace depth 0. Treats `{` and `}` as
/// Tcl-style grouping characters so that `KEY,{a,b,c},…` splits into
/// three columns rather than five.
fn split_brace_aware(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

fn braces_balanced(s: &str) -> bool {
    let mut depth: i32 = 0;
    for b in s.bytes() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// `param_mapping_presets.csv` layout: a single header row lists
/// preset-selector field names (e.g. `BOOT_MODE,CLOCK_MODE,…`); each
/// data row has the selector name in column 0 and one valid value in
/// column 1. The CSV repeats the header row between sections — we
/// detect those repeats and skip them so the header names don't get
/// mistakenly recorded as values of each other.
fn parse_presets_csv(path: &Path, fields: &mut HashMap<String, DictField>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header_line) = lines.next() else {
        return;
    };
    let headers: BTreeSet<String> = header_line
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && is_safe_key(s))
        .map(str::to_string)
        .collect();

    let mut by_key: HashMap<String, BTreeSet<String>> = HashMap::new();
    for line in lines {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let Some(first) = cols.first().filter(|c| !c.is_empty()) else {
            continue;
        };
        // Skip repeated header rows: every non-empty cell is itself a
        // declared header name.
        let is_header_row =
            cols.iter().all(|c| c.is_empty() || headers.contains(*c));
        if is_header_row {
            continue;
        }
        if !headers.contains(*first) {
            continue;
        }
        let Some(val) = cols.get(1).filter(|v| !v.is_empty()) else {
            continue;
        };
        by_key
            .entry((*first).to_string())
            .or_default()
            .insert(unwrap_one_brace(val).to_string());
    }

    for name in headers {
        let mut enums = by_key.remove(&name).unwrap_or_default();
        // Vivado UI convention: preset-selector fields always offer
        // `Custom` as the "configure each inner field manually" choice
        // even when the CSV doesn't enumerate it. It's also the most
        // useful default — picking a preset bundle locks the inner
        // fields, picking `Custom` lets the user override them.
        enums.insert("Custom".to_string());
        let default = "Custom".to_string();
        let f = fields.entry(name.clone()).or_insert_with(|| DictField {
            name: name.clone(),
            default: default.clone(),
            description: None,
            enum_values: BTreeSet::new(),
        });
        for v in enums {
            f.enum_values.insert(v);
        }
    }
}

/// Strip one layer of Tcl-style braces from a value if present:
/// `{0}` → `0`, `{{ENABLE 0}}` → `{ENABLE 0}`. Leaves unbalanced or
/// unbraced inputs alone.
fn unwrap_one_brace(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('{') && s.ends_with('}') {
        // Verify the outer braces actually pair with each other (i.e.
        // depth reaches 0 only at the final `}`).
        let mut depth: i32 = 0;
        for (i, b) in s.bytes().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 && i != s.len() - 1 {
                        return s; // not a single outer pair
                    }
                }
                _ => {}
            }
        }
        return &s[1..s.len() - 1];
    }
    s
}

/// Conservative IP-XACT-style identifier check: starts with a letter
/// or `_`, then alphanumerics / `_`. Anything else is data we don't
/// understand and should ignore (rather than mistake for a field).
fn is_safe_key(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(c0) = chars.next() else {
        return false;
    };
    if !(c0.is_ascii_alphabetic() || c0 == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Layer `<displayName>X</displayName>` text from a `ParamInfo.xml`
/// onto matching field descriptions. Uses a tiny line-oriented scan
/// — the schema is too irregular to demand a full XML parser.
fn layer_param_info(path: &Path, fields: &mut HashMap<String, DictField>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(start) = line.find("<parameter name=\"") {
            let after = &line[start + "<parameter name=\"".len()..];
            if let Some(end) = after.find('"') {
                current = Some(after[..end].to_string());
            }
        } else if let Some(name) = &current {
            if let Some(start) = line.find("<displayName>") {
                let after = &line[start + "<displayName>".len()..];
                if let Some(end) = after.find("</displayName>") {
                    let text = after[..end].trim();
                    if !text.is_empty() {
                        if let Some(f) = fields.get_mut(name) {
                            f.description = Some(text.to_string());
                        }
                    }
                }
            }
            if line.contains("</parameter>") {
                current = None;
            }
        }
    }
}

/// Layer enum values from preset XML files onto matching fields.
/// We only consume `<preset param="K" name="V"/>` entries — those are
/// genuine selector-style enumerations (BOOT_MODE, SMON_ALARMS, …)
/// where Vivado's UI offers a fixed dropdown. The XMLs also contain
/// `<set param="K" value="V"/>` entries, but those are concrete
/// values *applied* by a parent preset; they're not an exhaustive
/// list of valid values. For a numeric field like
/// `PMC_CRP_PL0_REF_CTRL_FREQMHZ` the user can supply any frequency
/// the clock generator can synthesize (e.g. `250`, `195`), so
/// treating `<set>` values as an `@enum` would wrongly reject those.
fn layer_presets(pspmc_dir: &Path, fields: &mut HashMap<String, DictField>) {
    let mut paths = Vec::new();
    paths.push(pspmc_dir.join("global/global_preset.xml"));
    paths.push(pspmc_dir.join("global/global_presetForNonPS.xml"));
    walk_for_xml(&pspmc_dir.join("presets"), &mut paths);

    for p in paths {
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        for line in text.lines() {
            if let Some((param, val)) =
                extract_two_attrs(line, "<preset", "param", "name")
            {
                if let Some(f) = fields.get_mut(param) {
                    f.enum_values.insert(val.to_string());
                }
            }
        }
    }
}

/// Pull the values of two named attributes (in order) from an XML
/// tag on a single line. Returns `None` if the tag or either attribute
/// is missing. Tolerant of arbitrary whitespace between `tag` and
/// attribute name (`<set  param="…" value="…"/>` works).
fn extract_two_attrs<'a>(
    line: &'a str,
    tag: &str,
    attr_a: &str,
    attr_b: &str,
) -> Option<(&'a str, &'a str)> {
    let tag_idx = line.find(tag)?;
    let after_tag = &line[tag_idx + tag.len()..];
    let (a, rest) = scan_attr(after_tag, attr_a)?;
    let (b, _) = scan_attr(rest, attr_b)?;
    Some((a, b))
}

fn scan_attr<'a>(s: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let needle_eq = format!("{name}=\"");
    let idx = s.find(&needle_eq)?;
    let after = &s[idx + needle_eq.len()..];
    let end = after.find('"')?;
    Some((&after[..end], &after[end + 1..]))
}

fn walk_for_xml(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk_for_xml(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("xml") {
            out.push(path);
        }
    }
}

/// Walk up `start` looking for an ancestor literally named `data`.
fn find_data_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.file_name().and_then(|s| s.to_str()) == Some("data") {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_brace_aware_respects_tcl_groups() {
        assert_eq!(split_brace_aware("a,b,c"), vec!["a", "b", "c"]);
        // Commas inside `{…}` stay attached to that cell.
        assert_eq!(
            split_brace_aware("KEY,{a,b,c},NEXT"),
            vec!["KEY", "{a,b,c}", "NEXT"]
        );
        // Nested braces.
        assert_eq!(
            split_brace_aware("K,{{x,y} {z,w}},end"),
            vec!["K", "{{x,y} {z,w}}", "end"]
        );
    }

    #[test]
    fn parse_direct_csv_drops_unbalanced_brace_defaults() {
        use std::io::Write;
        let f = tempfile::NamedTempFile::new().unwrap();
        // First row is well-formed; second has an unterminated brace
        // group (matches a real bug in Xilinx's vendor CSV).
        writeln!(&f, "GOOD_KEY,{{0}},GOOD_KEY,{{0}}").unwrap();
        writeln!(&f, "BAD_KEY,{{a,b,c").unwrap();
        let mut m: HashMap<String, DictField> = HashMap::new();
        parse_direct_csv(f.path(), &mut m);
        assert_eq!(m["GOOD_KEY"].default, "0");
        // BAD_KEY is recorded as a field but with no default.
        assert!(m.contains_key("BAD_KEY"));
        assert_eq!(m["BAD_KEY"].default, "");
    }

    #[test]
    fn unwrap_one_brace_strips_outer_pair_only() {
        assert_eq!(unwrap_one_brace("{0}"), "0");
        assert_eq!(unwrap_one_brace("{{ENABLE 0}}"), "{ENABLE 0}");
        assert_eq!(unwrap_one_brace("Custom"), "Custom");
        // Unbalanced — leave alone.
        assert_eq!(unwrap_one_brace("{a"), "{a");
        // Two adjacent groups — not a single outer pair.
        assert_eq!(unwrap_one_brace("{a}{b}"), "{a}{b}");
    }

    #[test]
    fn safe_key_rejects_anything_with_braces_or_special_chars() {
        assert!(is_safe_key("PS_USE_PMCPL_CLK0"));
        assert!(is_safe_key("_misc"));
        assert!(!is_safe_key("{0}"));
        assert!(!is_safe_key("0_LEADS_WITH_DIGIT"));
        assert!(!is_safe_key(""));
        assert!(!is_safe_key("has space"));
    }

    #[test]
    fn discovery_returns_empty_outside_xilinx_layout() {
        let dir = tempfile::tempdir().unwrap();
        let loose = dir.path().join("component.xml");
        std::fs::write(&loose, "").unwrap();
        assert!(load_schemas(&loose).is_empty());
    }

    /// Build a tempdir mimicking the Xilinx layout closely enough to
    /// exercise the loader end-to-end without a Vivado install.
    #[test]
    fn loads_minimum_viable_schema_from_synthetic_layout() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");

        let pspmc = data.join("versal/cips_hip/pspmc");
        let csvs =
            data.join("versal/flows/automation/cipsToPsWiz_Porting/csv_files");
        let global = pspmc.join("global");
        let guidata = pspmc.join("guidata");
        let presets = pspmc.join("presets");
        std::fs::create_dir_all(&csvs).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&guidata).unwrap();
        std::fs::create_dir_all(&presets).unwrap();

        std::fs::write(
            csvs.join("param_mapping_direct.csv"),
            "\
PCIE_APERTURES_DUAL_ENABLE,{0},PCIE_APERTURES_DUAL_ENABLE,{0}
PS_PCIE_RESET,{{ENABLE 0}},PS_PCIE_RESET,{ENABLE 0 IO PS_MIO_18:19}
SMON_ALARMS,{Set_Alarms_On},SMON_ALARMS,{Set_Alarms_On}
CPM_PCIE0_MODES,{None},CPM_PCIE0_MODES,{None}
",
        )
        .unwrap();
        std::fs::write(
            csvs.join("param_mapping_presets.csv"),
            "\
BOOT_MODE,CLOCK_MODE
BOOT_MODE,JTAG Boot
BOOT_MODE,Master Mode
CLOCK_MODE,Custom
CLOCK_MODE,REF CLK 33.33 MHz
",
        )
        .unwrap();
        std::fs::write(
            guidata.join("ParamInfo.xml"),
            r#"<?xml version="1.0"?>
<ParameterInfo>
    <parameter name="SMON_ALARMS">
        <displayName>What do you want to do with Alarms?</displayName>
    </parameter>
</ParameterInfo>
"#,
        )
        .unwrap();
        std::fs::write(
            presets.join("sysmon.xml"),
            r#"<presets>
  <preset param="SMON_ALARMS" name="Set_Alarms_On"/>
  <preset param="SMON_ALARMS" name="Set_Alarms_Off"/>
</presets>
"#,
        )
        .unwrap();
        // Empty global presets so the loader still finds the file.
        std::fs::write(global.join("global_preset.xml"), "<presets/>").unwrap();

        let component = data.join("ip/xilinx/versal_cips_v3_4/component.xml");
        std::fs::create_dir_all(component.parent().unwrap()).unwrap();
        std::fs::write(&component, "<dummy/>").unwrap();

        let schemas = load_schemas(&component);
        assert!(
            schemas.contains_key("PS_PMC_CONFIG"),
            "schemas: {schemas:?}"
        );
        assert!(schemas.contains_key("PS_PMC_CONFIG_INTERNAL"));
        let s = &schemas["PS_PMC_CONFIG"];
        let by_name: HashMap<&str, &DictField> =
            s.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        // From direct.csv (with CPM_ filtered out):
        assert!(by_name.contains_key("PCIE_APERTURES_DUAL_ENABLE"));
        assert_eq!(by_name["PCIE_APERTURES_DUAL_ENABLE"].default, "0");
        assert_eq!(
            by_name["PS_PCIE_RESET"].default, "{ENABLE 0}",
            "should strip one brace layer"
        );
        // CPM_ keys are filtered out.
        assert!(!by_name.contains_key("CPM_PCIE0_MODES"));
        // From ParamInfo: description present for SMON_ALARMS.
        assert_eq!(
            by_name["SMON_ALARMS"].description.as_deref(),
            Some("What do you want to do with Alarms?")
        );
        // From presets: enum widened.
        assert!(by_name["SMON_ALARMS"].enum_values.contains("Set_Alarms_On"));
        assert!(by_name["SMON_ALARMS"]
            .enum_values
            .contains("Set_Alarms_Off"));
        // From presets.csv: CLOCK_MODE present with "Custom" as default.
        assert!(by_name.contains_key("CLOCK_MODE"));
        assert_eq!(by_name["CLOCK_MODE"].default, "Custom");
        assert!(by_name["CLOCK_MODE"].enum_values.contains("Custom"));
        // BOOT_MODE's CSV row only lists "JTAG Boot" and "Master Mode" —
        // we should still inject `Custom` automatically (Vivado convention).
        assert_eq!(by_name["BOOT_MODE"].default, "Custom");
        assert!(by_name["BOOT_MODE"].enum_values.contains("Custom"));
        assert!(by_name["BOOT_MODE"].enum_values.contains("JTAG Boot"));
    }
}
