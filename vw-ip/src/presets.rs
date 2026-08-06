// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Out-of-band parameter value sources for IP-XACT components.
//!
//! Some Xilinx IPs (notably CIPS / CPM5) ship the bulk of their
//! parameter enumerations *outside* the IP-XACT XML, in
//! `cpm_preset*.xml` files Vivado bundles under `data/versal/ps_pmc/`.
//! Without them, parameters like `CPM_PCIE1_PF0_BASE_CLASS_MENU` would
//! only carry their declared default in the generated `@enum(...)`,
//! and there's no other principled signal to recover the legal values
//! from. This module reads those files into a flat map the generator
//! can merge against the IP-XACT `<choice>` lists.
//!
//! The XML shape is uniform across the files I've seen:
//!
//! ```xml
//! <presets>
//!   <preset param="CPM_PCIE1_MODE_SELECTION" name="Basic"/>
//!   <preset param="CPM_PCIE1_MODE_SELECTION" name="Advanced"/>
//!   ...
//! </presets>
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading preset file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing preset file {path}: {source}")]
    Xml {
        path: PathBuf,
        #[source]
        source: quick_xml::DeError,
    },
}

/// `param_name → set of valid values`. `BTreeSet` keeps the iteration
/// order stable so generated `@enum(...)` lists are deterministic.
pub type PresetMap = HashMap<String, BTreeSet<String>>;

#[derive(Debug, Default, Deserialize)]
struct Root {
    #[serde(default, rename = "preset")]
    entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "@param")]
    param: String,
    #[serde(rename = "@name")]
    name: String,
}

/// Load one preset XML file into a fresh map.
pub fn load_file(path: &Path) -> Result<PresetMap, Error> {
    let xml = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let root: Root =
        quick_xml::de::from_str(&xml).map_err(|source| Error::Xml {
            path: path.to_path_buf(),
            source,
        })?;
    let mut map = PresetMap::new();
    for e in root.entries {
        map.entry(e.param).or_default().insert(e.name);
    }
    Ok(map)
}

/// Load several preset XML files and merge their entries into one map.
pub fn load_files<I>(paths: I) -> Result<PresetMap, Error>
where
    I: IntoIterator,
    I::Item: AsRef<Path>,
{
    let mut merged = PresetMap::new();
    for p in paths {
        let map = load_file(p.as_ref())?;
        for (param, values) in map {
            merged.entry(param).or_default().extend(values);
        }
    }
    Ok(merged)
}

/// Try to find sibling preset files for the IP whose
/// `component.xml` lives at `component_path`.
///
/// Walks up from the component file looking for a Vivado-style
/// `data/` ancestor directory and then peeks at
/// `data/versal/ps_pmc/<ip-name>/`. Any `*preset*.xml` found there
/// (recursively) is returned. Returns an empty vector — not an error —
/// when the layout doesn't match; the caller should treat it as a
/// best-effort hint.
pub fn discover_for(component_path: &Path) -> Vec<PathBuf> {
    let Some(data_root) = data_root_of(component_path) else {
        return Vec::new();
    };
    let ip_name = ip_name_from(component_path);
    let Some(ip_name) = ip_name else {
        return Vec::new();
    };
    let ip_dir = data_root.join("versal").join("ps_pmc").join(&ip_name);
    if !ip_dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_preset_files(&ip_dir, &mut out);
    out.sort();
    out
}

/// Recurse through `dir` collecting any `*preset*.xml` file paths.
fn collect_preset_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_preset_files(&path, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.contains("preset") && name.ends_with(".xml") {
            out.push(path);
        }
    }
}

/// Walk up `component_path`'s ancestors looking for a directory
/// literally named `data` (Vivado's install root convention).
fn data_root_of(component_path: &Path) -> Option<PathBuf> {
    for ancestor in component_path.ancestors() {
        if ancestor.file_name().and_then(|s| s.to_str()) == Some("data") {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

/// Recover an IP's short name from a Vivado-style versioned directory
/// (`cpm5_v1_0` → `cpm5`, `axi_dma_v7_1` → `axi_dma`). The trailing
/// `_v<N>_<N>` suffix is the convention Xilinx uses across IPs.
fn ip_name_from(component_path: &Path) -> Option<String> {
    let ip_dir = component_path.parent()?;
    let name = ip_dir.file_name()?.to_str()?;
    Some(strip_version_suffix(name).to_string())
}

fn strip_version_suffix(name: &str) -> &str {
    // Find a trailing `_v<digits>_<digits>` and trim it.
    let bytes = name.as_bytes();
    let mut end = bytes.len();
    // Trailing digits (minor)
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    if end == 0 || bytes[end - 1] != b'_' {
        return name;
    }
    let after_minor = end;
    end -= 1; // skip the `_`
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let after_major_digits = end;
    if end < 1 || &bytes[end.saturating_sub(2)..end] != b"_v" {
        // Doesn't end in `_v<N>_<N>` — leave as-is.
        return name;
    }
    let _ = after_minor;
    let _ = after_major_digits;
    &name[..end - 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_preset_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.xml");
        fs::write(
            &path,
            r#"<presets>
                 <preset param="A" name="x"/>
                 <preset param="A" name="y"/>
                 <preset param="B" name="z"/>
               </presets>"#,
        )
        .unwrap();
        let m = load_file(&path).unwrap();
        let a: Vec<&str> = m["A"].iter().map(String::as_str).collect();
        assert_eq!(a, vec!["x", "y"]);
        assert!(m["B"].contains("z"));
    }

    #[test]
    fn merges_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().join("a.xml");
        fs::write(&p1, r#"<presets><preset param="K" name="1"/></presets>"#)
            .unwrap();
        let p2 = dir.path().join("b.xml");
        fs::write(&p2, r#"<presets><preset param="K" name="2"/></presets>"#)
            .unwrap();
        let m = load_files(&[p1, p2]).unwrap();
        let v: Vec<&str> = m["K"].iter().map(String::as_str).collect();
        assert_eq!(v, vec!["1", "2"]);
    }

    #[test]
    fn strips_xilinx_version_suffix() {
        assert_eq!(strip_version_suffix("cpm5_v1_0"), "cpm5");
        assert_eq!(strip_version_suffix("axi_dma_v7_1"), "axi_dma");
        // No version → unchanged.
        assert_eq!(strip_version_suffix("foo_bar"), "foo_bar");
        // Almost-but-not version → unchanged.
        assert_eq!(strip_version_suffix("foo_v1"), "foo_v1");
    }

    #[test]
    fn discovers_under_data_layout() {
        let dir = tempfile::tempdir().unwrap();
        // Mimic Xilinx layout: data/ip/xilinx/<ip>_v1_0/component.xml
        let data = dir.path().join("data");
        let ip = data.join("ip").join("xilinx").join("widget_v2_3");
        fs::create_dir_all(&ip).unwrap();
        let component = ip.join("component.xml");
        fs::write(&component, "<dummy/>").unwrap();
        // And sibling: data/versal/ps_pmc/widget/p.xml
        let preset_dir = data.join("versal").join("ps_pmc").join("widget");
        fs::create_dir_all(&preset_dir).unwrap();
        let preset = preset_dir.join("my_preset.xml");
        fs::write(&preset, "<presets/>").unwrap();
        // Unrelated file shouldn't be picked up.
        fs::write(preset_dir.join("README.md"), "ignored").unwrap();

        let found = discover_for(&component);
        assert_eq!(found, vec![preset]);
    }

    #[test]
    fn discovery_empty_when_layout_doesnt_match() {
        let dir = tempfile::tempdir().unwrap();
        let component = dir.path().join("loose.xml");
        fs::write(&component, "<dummy/>").unwrap();
        assert!(discover_for(&component).is_empty());
    }
}
