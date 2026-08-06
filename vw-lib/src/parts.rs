//! Enumerate FPGA parts from a local Vivado install without launching
//! Vivado (which takes ~30s just to start up).
//!
//! The plaintext catalog lives at
//! `<install>/data/parts/xilinx/<archdir>/public/ibis/FileMap.txt`
//! for every architecture Xilinx has published this file for
//! (7-series, Zynq-7000/UltraScale+, all UltraScale+, and Versal in
//! 2025.1 — pre-7-series legacy families ship only encrypted
//! `DeviceParts.xml`). Each FileMap.txt has a `pkg-file-mapping
//! { ... }` block with rows shaped `<part_id_underscored>  <package>.pkg`.
//! Translating the id from underscores to dashes yields the canonical
//! Vivado part id (`xcvp1202_vsva2785_2MP_e_S_` →
//! `xcvp1202-vsva2785-2MP-e-S`; `xa7s100_fgga484_1I` →
//! `xa7s100-fgga484-1I`). UltraScale+/Versal ids carry a trailing
//! separator underscore that must be stripped; 7-series ids don't.
//! Rows whose package column is the literal `In_Development` are
//! placeholders and get filtered out.

use camino::Utf8PathBuf;
use std::path::PathBuf;

/// Broad device series used for filtering in the picker chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartSeries {
    Versal,
    KintexUP,
    VirtexUP,
    ArtixUP,
    SpartanUP,
    /// Zynq UltraScale+ MPSoC and RFSoC — same `zu` prefix, both
    /// live under this bucket. Users needing MPSoC vs RFSoC narrow
    /// via fuzzy search (`eg`/`ev` = MPSoC, `dr` = RFSoC).
    ZynqUP,
    Artix7,
    Kintex7,
    Virtex7,
    Spartan7,
    Zynq7,
    Other,
}

impl PartSeries {
    pub fn label(self) -> &'static str {
        match self {
            Self::Versal => "Versal",
            Self::KintexUP => "Kintex UP",
            Self::VirtexUP => "Virtex UP",
            Self::ArtixUP => "Artix UP",
            Self::SpartanUP => "Spartan UP",
            Self::ZynqUP => "Zynq UP",
            Self::Artix7 => "Artix-7",
            Self::Kintex7 => "Kintex-7",
            Self::Virtex7 => "Virtex-7",
            Self::Spartan7 => "Spartan-7",
            Self::Zynq7 => "Zynq-7000",
            Self::Other => "Other",
        }
    }

    /// Every variant, in the order the picker's Tab chip cycles
    /// through them. Newer/bigger families first so common cases
    /// need fewer Tab presses.
    pub fn all() -> [PartSeries; 12] {
        [
            Self::Versal,
            Self::ZynqUP,
            Self::KintexUP,
            Self::VirtexUP,
            Self::ArtixUP,
            Self::SpartanUP,
            Self::Artix7,
            Self::Kintex7,
            Self::Virtex7,
            Self::Spartan7,
            Self::Zynq7,
            Self::Other,
        ]
    }

    /// Classify a canonical part id (dashed form) into a series
    /// based on its `x[caq][r?]<letters>` prefix. Vivado embeds the
    /// series in the first 3-4 chars of every part id.
    fn from_part_id(part_id: &str) -> Self {
        let bytes = part_id.as_bytes();
        if bytes.len() < 3 || bytes[0] != b'x' {
            return Self::Other;
        }
        // Strip the grade prefix (`xa`/`xc`/`xq`/`xqr`) to leave
        // just the family suffix. `xqr` (radiation-tolerant) comes
        // first so `xq` doesn't shadow it.
        let after = if bytes[1] == b'q' && bytes.get(2) == Some(&b'r') {
            &bytes[3..]
        } else if matches!(bytes[1], b'a' | b'c' | b'q') {
            &bytes[2..]
        } else {
            return Self::Other;
        };
        // UltraScale+ / Versal — 2-letter family code.
        if let Some(&a) = after.first() {
            if let Some(&b) = after.get(1) {
                let series = match (a, b) {
                    (b'v', b'p' | b'c' | b'm' | b'e' | b'h' | b'r' | b'n') => {
                        Some(Self::Versal)
                    }
                    (b'k', b'u') => Some(Self::KintexUP),
                    (b'v', b'u') => Some(Self::VirtexUP),
                    (b'a', b'u') => Some(Self::ArtixUP),
                    (b's', b'u') => Some(Self::SpartanUP),
                    (b'z', b'u') => Some(Self::ZynqUP),
                    // `xcv80` is a Versal outlier — device-name-only
                    // classification, no 2-letter family shortcut.
                    _ => None,
                };
                if let Some(s) = series {
                    return s;
                }
            }
            // 7-series — `7<a/k/v/s/z>` pattern after the grade.
            if a == b'7' {
                return match after.get(1) {
                    Some(&b'a') => Self::Artix7,
                    Some(&b'k') => Self::Kintex7,
                    Some(&b'v') => Self::Virtex7,
                    Some(&b's') => Self::Spartan7,
                    Some(&b'z') => Self::Zynq7,
                    _ => Self::Other,
                };
            }
        }
        // `xcv80…` (Versal outlier with 1-letter code).
        if after.starts_with(b"v80") {
            return Self::Versal;
        }
        Self::Other
    }

    /// Fallback classifier used when `from_part_id` returns `Other`.
    /// Every FileMap.txt lives under a Xilinx architecture directory
    /// name that identifies the family authoritatively — new part
    /// prefixes (Kria SOMs `xck2*`, Versal Series 2 `xc2v*`, etc.)
    /// still get classified correctly without needing the prefix
    /// table to keep up.
    fn from_arch_dir(dir: &str) -> Option<Self> {
        match dir {
            "versal" => Some(Self::Versal),
            "kintexuplus" => Some(Self::KintexUP),
            "virtexuplus" | "virtexuplus58g" | "virtexuplusHBM" => {
                Some(Self::VirtexUP)
            }
            "spartanuplus" => Some(Self::SpartanUP),
            "zynquplus" | "zynquplusRFSOC" => Some(Self::ZynqUP),
            "artix7" => Some(Self::Artix7),
            "kintex7" => Some(Self::Kintex7),
            "virtex7" => Some(Self::Virtex7),
            "spartan7" => Some(Self::Spartan7),
            "zynq" => Some(Self::Zynq7),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEntry {
    /// Canonical Vivado part id (e.g. `xcvp1202-vsva2785-2MP-e-S`).
    pub id: String,
    pub series: PartSeries,
}

/// Resolve the Vivado install directory (containing `bin/`, `data/`,
/// `settings64.sh`). Preference order matches Xilinx tooling: env
/// override first, then whatever `vivado` on `$PATH` resolves to.
pub fn find_vivado_install() -> Option<Utf8PathBuf> {
    if let Ok(env) = std::env::var("XILINX_VIVADO") {
        let p = Utf8PathBuf::from(env);
        if p.join("data/parts").exists() {
            return Some(p);
        }
    }
    let output = std::process::Command::new("which")
        .arg("vivado")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    let resolved = std::fs::canonicalize(&path).ok()?;
    // `<install>/bin/vivado` → `<install>`
    let install = resolved.parent()?.parent()?;
    Utf8PathBuf::from_path_buf(install.to_path_buf()).ok()
}

/// Walk every plaintext `FileMap.txt` under the install and pull
/// out every shipping part id. Returns them deduplicated and sorted
/// by canonical id — a part may appear in multiple FileMaps when
/// architectures overlap (Artix UP lives in the `kintexuplus` dir
/// despite having its own series prefix).
pub fn enumerate_parts(install: &Utf8PathBuf) -> Vec<PartEntry> {
    let arch_root = install.join("data/parts/xilinx");
    let Ok(entries) = std::fs::read_dir(&arch_root) else {
        return Vec::new();
    };
    let mut all: Vec<PartEntry> = Vec::new();
    for entry in entries.flatten() {
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();
        let filemap = entry.path().join("public/ibis/FileMap.txt");
        let Ok(contents) = std::fs::read_to_string(&filemap) else {
            continue;
        };
        let dir_fallback = PartSeries::from_arch_dir(&dir_name);
        for mut part in parse_filemap(&contents) {
            if part.series == PartSeries::Other {
                if let Some(fallback) = dir_fallback {
                    part.series = fallback;
                }
            }
            all.push(part);
        }
    }
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all.dedup_by(|a, b| a.id == b.id);
    all
}

/// Parse the `pkg-file-mapping { ... }` block out of a FileMap.txt
/// string. Handles both UltraScale+/Versal id shape (trailing
/// separator underscore that gets stripped) and 7-series shape
/// (no trailing underscore).
pub fn parse_filemap(contents: &str) -> Vec<PartEntry> {
    let mut out = Vec::new();
    let mut in_pkg_block = false;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if !in_pkg_block {
            if trimmed.starts_with("pkg-file-mapping") {
                in_pkg_block = true;
            }
            continue;
        }
        if trimmed.starts_with('}') {
            break;
        }
        // Row shape: `  <ident>\s+<package>.pkg`. Placeholder rows
        // use `In_Development` as the package name — skip those.
        let mut it = trimmed.split_whitespace();
        let (Some(ident), Some(pkg)) = (it.next(), it.next()) else {
            continue;
        };
        if pkg == "In_Development" {
            continue;
        }
        let id = ident.strip_suffix('_').unwrap_or(ident).replace('_', "-");
        if id.is_empty() {
            continue;
        }
        let series = PartSeries::from_part_id(&id);
        out.push(PartEntry { id, series });
    }
    out
}

/// Sub-string, case-insensitive fuzzy filter used by the picker.
/// Splits the query on whitespace; every token must appear in the
/// part id (in any order, any position) for the entry to match.
/// Empty query returns everything.
pub fn matches_query(id: &str, query: &str) -> bool {
    let hay = id.to_ascii_lowercase();
    query
        .split_whitespace()
        .all(|tok| hay.contains(&tok.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ULTRASCALE_PLUS: &str = "\
# Header comment
#
ibs-file-mapping {
  irrelevant  ignored.ibs
}

pkg-file-mapping {
  xcvp1202_vsva2785_2MP_e_S_               xcvp1202_vsva2785.pkg
  xcvp1202_vsva2785_1LP_i_L_               xcvp1202_vsva2785.pkg
  xcau10p_ffvb676_1_e_                     xcau10p_ffvb676.pkg
  xcku15p_CIV_ffva1156_2_e_                In_Development
  xcvu9p_flga2104_2_e_                     xcvu9p_flga2104.pkg
  xcsu35p_swra1493_1_e_                    xcsu35p_swra1493.pkg
  xczu9eg_ffvc900_2_e_                     xczu9eg_ffvc900.pkg
  xczu21dr_ffvd1156_2_e_                   xczu21dr_ffvd1156.pkg
  xqku15p_ffva1156_2_e_                    xqku15p_ffva1156.pkg
  malformed_row_no_pkg
}

some-other-block {
  xzz_should_not_appear  foo.pkg
}
";

    // 7-series: no trailing `_` on identifiers, speed grade like `_1I`.
    const FIXTURE_7SERIES: &str = "\
pkg-file-mapping {
  xc7a100t_csg324_1     xc7a100t_csg324.pkg
  xa7a100t_csg324_1I    xc7a100t_csg324.pkg
  xc7k325t_ffg676_2     xc7k325t_ffg676.pkg
  xc7v2000t_flg1925_1   xc7v2000t_flg1925.pkg
  xc7s100_fgga484_1     xc7s100_fgga484.pkg
  xc7z020_clg484_1      xc7z020_clg484.pkg
}
";

    #[test]
    fn parses_pkg_block_and_translates_ids_ultrascale_plus() {
        let parts = parse_filemap(FIXTURE_ULTRASCALE_PLUS);
        let ids: Vec<&str> = parts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "xcvp1202-vsva2785-2MP-e-S",
                "xcvp1202-vsva2785-1LP-i-L",
                "xcau10p-ffvb676-1-e",
                "xcvu9p-flga2104-2-e",
                "xcsu35p-swra1493-1-e",
                "xczu9eg-ffvc900-2-e",
                "xczu21dr-ffvd1156-2-e",
                "xqku15p-ffva1156-2-e",
            ]
        );
    }

    #[test]
    fn parses_pkg_block_and_translates_ids_7series() {
        let parts = parse_filemap(FIXTURE_7SERIES);
        let ids: Vec<&str> = parts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "xc7a100t-csg324-1",
                "xa7a100t-csg324-1I",
                "xc7k325t-ffg676-2",
                "xc7v2000t-flg1925-1",
                "xc7s100-fgga484-1",
                "xc7z020-clg484-1",
            ]
        );
    }

    #[test]
    fn skips_in_development_and_out_of_block_rows() {
        let parts = parse_filemap(FIXTURE_ULTRASCALE_PLUS);
        assert!(parts.iter().all(|p| !p.id.contains("CIV")));
        assert!(parts.iter().all(|p| !p.id.contains("xzz")));
    }

    #[test]
    fn classifies_series() {
        let cases = [
            ("xcvp1202-vsva2785-2MP-e-S", PartSeries::Versal),
            ("xcvc1902-vsva2197-2MP-e-S", PartSeries::Versal),
            ("xcvm1502-vsva2197-2MP-e-S", PartSeries::Versal),
            ("xcvh1782-vsva3697-2MP-e-S", PartSeries::Versal),
            ("xave2002-nsvg1369-2LP-e-S", PartSeries::Versal),
            ("xqrvc1902-vsva2197-1MP-i-L", PartSeries::Versal),
            ("xcv80-lsva4737-2LHP-i-S", PartSeries::Versal),
            ("xcku15p-ffva1156-2-e", PartSeries::KintexUP),
            ("xqku15p-ffva1156-2-e", PartSeries::KintexUP),
            ("xcvu9p-flga2104-2-e", PartSeries::VirtexUP),
            ("xcau10p-ffvb676-1-e", PartSeries::ArtixUP),
            ("xaau10p-ffvb676-1-e", PartSeries::ArtixUP),
            ("xcsu35p-swra1493-1-e", PartSeries::SpartanUP),
            ("xczu9eg-ffvc900-2-e", PartSeries::ZynqUP),
            ("xczu21dr-ffvd1156-2-e", PartSeries::ZynqUP),
            ("xc7a100t-csg324-1", PartSeries::Artix7),
            ("xa7a100t-csg324-1I", PartSeries::Artix7),
            ("xq7k325t-ffg676-2I", PartSeries::Kintex7),
            ("xc7v2000t-flg1925-1", PartSeries::Virtex7),
            ("xc7s100-fgga484-1", PartSeries::Spartan7),
            ("xc7z020-clg484-1", PartSeries::Zynq7),
            ("bogus", PartSeries::Other),
        ];
        for (id, expected) in cases {
            assert_eq!(
                PartSeries::from_part_id(id),
                expected,
                "misclassified {id}"
            );
        }
    }

    #[test]
    fn fuzzy_matches_across_tokens_case_insensitive() {
        assert!(matches_query("xcvp1202-vsva2785-2MP-e-S", "vp1202"));
        assert!(matches_query("xcvp1202-vsva2785-2MP-e-S", "VSVA 2mp"));
        assert!(matches_query("xcvp1202-vsva2785-2MP-e-S", ""));
        assert!(!matches_query("xcvp1202-vsva2785-2MP-e-S", "vp1202 nope"));
    }
}
