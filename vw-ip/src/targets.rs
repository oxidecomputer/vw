// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `[targets]` extraction for the generated `vw.toml`.
//!
//! Reads `<xilinx:supportedFamilies>` out of an IP's `component.xml`,
//! CATEGORIZES each entry by its `xilinx:lifeCycle` attribute, and
//! returns two brace-form pattern lists ready to be written into a
//! workspace `vw.toml` under `[targets]`:
//!
//! - `supported = [...]`  — entries whose lifeCycle is `Production`,
//!   `Beta`, or `Pre-Production`. Xilinx has blessed the IP for the
//!   listed parts.
//! - `not-supported = [...]` — entries with `lifeCycle="Not-Supported"`.
//!   Xilinx has attested the IP is NOT usable on those parts.
//!
//! The split matters because Vivado's IP catalog is not gated by
//! `<xilinx:supportedFamilies>` — `get_ipdefs` returns an IP even
//! for families not in the list. So a static "reject if the target
//! isn't listed" rule produces false positives (e.g. clk_wizard_v1_0
//! has EVERY entry marked Not-Supported yet works fine on `xcvp1202`
//! in real projects). The list is a lifeCycle-tagged compatibility
//! matrix, not a filter — `vw check` uses it to distinguish
//! definitively-forbidden combinations (error) from merely-unblessed
//! ones (warning). See `vw_lib::TargetMismatchKind` for the check
//! side.
//!
//! Normalization rules per entry:
//! - Entries already in brace form (`versal{xcvm3(.*)}`) pass through
//!   verbatim.
//! - Bare-family entries (`artix7`) get widened to `<family>{.+}` — a
//!   permissive placeholder that says "we support any part in this
//!   family, but we haven't narrowed the pattern here." A future
//!   extension can query Vivado at `vw ip generate` time to derive
//!   precise regexes for each legacy family.
//!
//! The extractor is regex-driven rather than XML-parsed to avoid
//! adding an XML dep to `vw-ip`. The `<xilinx:family>` elements have a
//! tightly constrained shape in every component.xml we've observed;
//! the regex covers each cleanly.

use std::path::Path;

/// The two `[targets]` lists produced by [`extract_targets`], each
/// already normalized to brace form and ready for TOML upsert.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtractedTargets {
    /// `Production` / `Beta` / `Pre-Production` entries.
    pub supported: Vec<String>,
    /// `Not-Supported` entries.
    pub not_supported: Vec<String>,
}

/// Read the `<xilinx:family>` entries from `component_path` and
/// split them by lifeCycle. Returns empty lists when the file has
/// no `<xilinx:supportedFamilies>` section (older or
/// non-family-aware component.xml). I/O errors and malformed XML
/// both surface as an empty result — the generator wraps this
/// with a warning when both lists are empty on an IP that clearly
/// should have families.
pub fn extract_targets(component_path: &Path) -> ExtractedTargets {
    let Ok(xml) = std::fs::read_to_string(component_path) else {
        return ExtractedTargets::default();
    };
    extract_targets_from_xml(&xml)
}

/// Same as [`extract_targets`] but takes the XML text directly —
/// used by unit tests to feed known snippets without disk I/O.
pub fn extract_targets_from_xml(xml: &str) -> ExtractedTargets {
    // `<xilinx:family ...>versal{xcvm3(.*)}</xilinx:family>`
    // (with optional attributes on the opening tag). We care both
    // about the attribute cluster (to sniff out `xilinx:lifeCycle`)
    // and the interior text.
    let entry_re = regex::Regex::new(
        r#"(?s)<xilinx:family([^>]*)>\s*([^<]*?)\s*</xilinx:family>"#,
    )
    .expect("family entry regex must compile");
    let lifecycle_re =
        regex::Regex::new(r#"(?i)xilinx:lifeCycle\s*=\s*"([^"]*)""#)
            .expect("lifecycle regex must compile");
    let mut out = ExtractedTargets::default();
    for cap in entry_re.captures_iter(xml) {
        let attrs = &cap[1];
        let raw = cap[2].trim();
        if raw.is_empty() {
            continue;
        }
        let normalized = normalize_family_entry(raw);
        let lifecycle = lifecycle_re
            .captures(attrs)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        // Anything explicitly "Not-Supported" goes into the ban
        // list. Every other value — Production, Beta,
        // Pre-Production, or missing — is treated as blessed.
        // Missing lifeCycle is the common case for older /
        // non-annotated component.xml files; blessing is the safer
        // default since a NON-match on the blessed list produces
        // a warning, not an error.
        if lifecycle.eq_ignore_ascii_case("Not-Supported") {
            out.not_supported.push(normalized);
        } else {
            out.supported.push(normalized);
        }
    }
    out
}

/// Normalize one raw `<xilinx:family>` payload into
/// `<family>{<regex>}` form. Entries already in brace form pass
/// through verbatim.
fn normalize_family_entry(raw: &str) -> String {
    if raw.contains('{') && raw.ends_with('}') {
        return raw.to_string();
    }
    // Bare family — widen to `family{.+}`. Any part name matches;
    // the check effectively becomes "does the consumer target
    // string exist at all," which is a strict subset of what
    // Vivado would allow but never rejects a genuinely-supported
    // part.
    format!("{raw}{{.+}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_form_without_lifecycle_is_blessed() {
        // No `xilinx:lifeCycle` attribute → treat as blessed
        // (`supported`), since a missing lifecycle is the common
        // shape in older component.xml files.
        let out = extract_targets_from_xml(
            r#"<xilinx:family>versal{xcvm3(.*)}</xilinx:family>"#,
        );
        assert_eq!(out.supported, vec!["versal{xcvm3(.*)}"]);
        assert!(out.not_supported.is_empty());
    }

    #[test]
    fn production_lifecycle_lands_in_supported() {
        let out = extract_targets_from_xml(
            r#"<xilinx:family xilinx:lifeCycle="Production">versal{xcvm3(.*)}</xilinx:family>"#,
        );
        assert_eq!(out.supported, vec!["versal{xcvm3(.*)}"]);
        assert!(out.not_supported.is_empty());
    }

    #[test]
    fn not_supported_lifecycle_lands_in_ban_list() {
        let out = extract_targets_from_xml(
            r#"<xilinx:family xilinx:lifeCycle="Not-Supported">versal{xcvm3(.*)}</xilinx:family>"#,
        );
        assert_eq!(out.not_supported, vec!["versal{xcvm3(.*)}"]);
        assert!(out.supported.is_empty());
    }

    #[test]
    fn bare_family_widens_to_placeholder_regex() {
        let out = extract_targets_from_xml(
            r#"<xilinx:family>artix7</xilinx:family>"#,
        );
        assert_eq!(out.supported, vec!["artix7{.+}"]);
    }

    #[test]
    fn mixed_lifecycles_are_split() {
        let out = extract_targets_from_xml(
            r#"
            <xilinx:supportedFamilies>
              <xilinx:family xilinx:lifeCycle="Production">versal{xcvm3(.*)}</xilinx:family>
              <xilinx:family xilinx:lifeCycle="Not-Supported">artix7{xc7a35t(.*)}</xilinx:family>
              <xilinx:family xilinx:lifeCycle="Beta">versal{xcvp1202(.*)}</xilinx:family>
            </xilinx:supportedFamilies>
            "#,
        );
        assert_eq!(
            out.supported,
            vec!["versal{xcvm3(.*)}", "versal{xcvp1202(.*)}"],
        );
        assert_eq!(out.not_supported, vec!["artix7{xc7a35t(.*)}"]);
    }

    #[test]
    fn missing_section_returns_empty() {
        let out =
            extract_targets_from_xml("<component>no families here</component>");
        assert!(out.supported.is_empty());
        assert!(out.not_supported.is_empty());
    }
}
