// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Detect **indexed families** among sibling tree nodes.
//!
//! An indexed family is a set of sibling nodes whose labels differ
//! only by a trailing digit run — e.g. `MAC_PORT0`, `MAC_PORT1`, …,
//! `MAC_PORT5` — AND whose per-node parameter shapes are identical
//! once the digit is stripped. When present, the family's N sibling
//! nodes collapse into one constructor proc plus N kwargs on the
//! parent, and the parent's body emits ONE atomic
//! `set_property -dict` across everything.
//!
//! One guardrail keeps the detection safe:
//!
//! - **Strict direct-param shape match.** After stripping the
//!   `<STEM><N>_` prefix from each direct param, the resulting
//!   name-set must be identical across members, and each name's
//!   `default_value` / `choice_ref` / `is_user_configurable` triple
//!   must agree. Any divergence falls back to per-N.
//!
//! Detection walks the whole tree — the family need not sit at
//! root level, because intermediate grouping nodes (e.g. DCMAC's
//! `MAC` node aggregating `MAC_PORT0..5`) don't emit their own proc.
//! Detected families ALWAYS land as kwargs on the top-level
//! `<ip>::create` proc; the sibling sub-procs that today set the
//! direct params of each family member (`<ip>::mac_port0..5`)
//! disappear in favor of the composed constructor.
//!
//! Sub-nodes UNDER family members (e.g. `MAC_PORT0_RX`,
//! `MAC_PORT0_TX` — each their own tree node with their own direct
//! params) continue to emit as per-N sub-procs. They set properties
//! deeper in the CONFIG namespace than the family constructor
//! covers, so leaving them per-N is safe — the atomicity bug only
//! affects the family's direct-param slice.
//!
//! Callers can opt out further via [`DetectOptions::excluded_stems`]
//! when a specific stem needs to stay per-N.

use std::collections::{BTreeMap, HashMap};

use ipxact::Parameter;

use crate::tree::{strip_prefix, Node};

/// Options for [`detect_families`]. Every knob defaults to "detect
/// everything the guardrails allow"; the caller can subtract from
/// there via `excluded_stems` when a specific family causes trouble.
#[derive(Clone, Debug, Default)]
pub struct DetectOptions {
    /// Family stems (e.g. `"MAC_PORT"`) to leave per-N even when
    /// the guardrails would otherwise collapse them. Populated by
    /// the CLI's `--no-collapse=STEM,STEM,…` flag. Empty by default.
    pub excluded_stems: Vec<String>,
}

/// A detected indexed family — enough information for the emitter
/// to produce a constructor and weave the kwargs into the parent
/// proc.
#[derive(Clone, Debug)]
pub struct IndexedFamily<'a> {
    /// The common label prefix with trailing digits stripped
    /// (`"MAC_PORT"`, `"GT_CH"`, `"FEC_SLICE"`, …). Used to name
    /// the emitted `<ns>::<stem_lower>` constructor and the
    /// `<ns>::<StemProps>` newtype.
    pub stem: String,
    /// The concrete digit indices present in the source, in
    /// ascending order (e.g. `[0, 1, 2, 3, 4, 5]` for DCMAC's
    /// MAC_PORT family). Members can be non-contiguous.
    pub indices: Vec<u32>,
    /// Direct-params from the FIRST member. The emitter treats
    /// these as canonical shape; enum values are still unioned
    /// across all members (see [`Self::members_direct`]) so per-
    /// member enum-choice differences don't leak into the
    /// constructor's arg constraints.
    pub shape: Vec<&'a Parameter>,
    /// All members' direct-param lists, in index order (parallel
    /// to [`Self::indices`]). Emission uses this to union enum
    /// values across members — some IPs give the same logical
    /// field different `choiceRef` sets per index (DCMAC's
    /// MAC_PORT0 has 27 CONFIG_C0 choices while MAC_PORT1 has 12),
    /// and the collapsed constructor needs to accept the superset
    /// so callers can populate any port they want.
    pub members_direct: Vec<Vec<&'a Parameter>>,
    /// Parent node's label (empty for root children). Used to
    /// place the family's kwargs on the correct parent proc.
    pub parent_label: String,
    /// Original label of the shape-carrier member (e.g.
    /// `"MAC_PORT0"`). Used to strip param-name prefixes at
    /// emission time via [`strip_prefix`].
    pub shape_member_label: String,
    /// Parallel to [`Self::indices`] — each member's own label
    /// (e.g. `["MAC_PORT0", "MAC_PORT1", …, "MAC_PORT5"]`).
    /// Emission uses this to strip the correct prefix from each
    /// member's params when unioning enum values.
    pub member_labels: Vec<String>,
}

/// Walk `root`'s subtree, collecting every indexed family whose
/// members' direct params shape-match AND whose enclosing parent
/// chain contains no already-indexed node. Sub-nodes UNDER family
/// members are ignored by this pass — they keep emitting per-N.
///
/// The "no indexed ancestor" rule is what the user's "follow suit
/// for nested" instruction means in practice: DCMAC's
/// `MAC_PORT0..5` collapse (parent chain is root → `MAC`,
/// neither indexed) but CPM5's `PF0_BAR0..N` don't (parent chain
/// runs through `PCIE0` / `PF0`, both digit-indexed themselves).
pub fn detect_families<'a>(
    root: &Node<'a>,
    opts: &DetectOptions,
) -> Vec<IndexedFamily<'a>> {
    let mut out = Vec::new();
    detect_at(root, opts, /* indexed_ancestor */ false, &mut out);
    out
}

fn detect_at<'a>(
    node: &Node<'a>,
    opts: &DetectOptions,
    indexed_ancestor: bool,
    out: &mut Vec<IndexedFamily<'a>>,
) {
    // Group this node's children by stem.
    let mut by_stem: BTreeMap<String, Vec<(u32, &Node<'a>)>> = BTreeMap::new();
    for child in &node.children {
        let Some((stem, idx)) = split_trailing_digits(&child.label) else {
            continue;
        };
        by_stem.entry(stem).or_default().push((idx, child));
    }
    // Try to collapse each stem-group. Members that don't pass the
    // guardrails fall through untouched — the emitter keeps
    // producing per-N procs for them.
    for (stem, mut members) in by_stem {
        if members.len() < 2 {
            continue;
        }
        if opts.excluded_stems.iter().any(|s| s == &stem) {
            continue;
        }
        if indexed_ancestor {
            // Nested-under-indexed context. Follow-suit rule:
            // keep emitting per-N.
            continue;
        }
        // Shape match on direct params: same name-set across
        // members + same (default, is_user_configurable) per
        // field. `choice_ref` intentionally NOT compared —
        // per-member enum-choice differences are semantically
        // OK for the family constructor as long as we union
        // them at emit time (see IndexedFamily::members_direct).
        members.sort_by_key(|(idx, _)| *idx);
        if !shapes_match(&members) {
            continue;
        }
        let (_, first) = members[0];
        // Skip empty-shape families — nothing to hoist into the
        // constructor. Their direct-params list is empty because
        // all their params live in sub-nodes; the per-N sub-procs
        // handle those and there's no atomicity benefit to
        // emitting a stub constructor.
        if first.direct.is_empty() {
            continue;
        }
        out.push(IndexedFamily {
            stem: stem.clone(),
            indices: members.iter().map(|(idx, _)| *idx).collect(),
            shape: first.direct.clone(),
            members_direct: members
                .iter()
                .map(|(_, n)| n.direct.clone())
                .collect(),
            parent_label: node.label.clone(),
            shape_member_label: first.label.clone(),
            member_labels: members
                .iter()
                .map(|(_, n)| n.label.clone())
                .collect(),
        });
    }
    // Recurse — a family may live under an intermediate grouping
    // node (DCMAC's `MAC` node aggregates `MAC_PORT0..5`).
    for child in &node.children {
        // Once we cross into an indexed node, everything below
        // inherits "nested" status and stays per-N.
        let child_indexed =
            indexed_ancestor || split_trailing_digits(&child.label).is_some();
        detect_at(child, opts, child_indexed, out);
    }
}

/// If `label` ends in a run of ASCII digits, return `(stem, index)`.
/// Empty stem or non-digit-suffix labels return `None` — those aren't
/// indexed family members.
fn split_trailing_digits(label: &str) -> Option<(String, u32)> {
    let bytes = label.as_bytes();
    let mut cut = bytes.len();
    while cut > 0 && bytes[cut - 1].is_ascii_digit() {
        cut -= 1;
    }
    if cut == bytes.len() || cut == 0 {
        // No trailing digits OR digits are the whole label
        // (e.g. `"0"` alone — not a family we can name after a stem).
        return None;
    }
    let stem = label[..cut].to_string();
    let idx: u32 = label[cut..].parse().ok()?;
    Some((stem, idx))
}

/// Compare all members' direct-param sets against member[0]'s.
/// True when every member has the same set of index-stripped param
/// names AND each name's triple matches.
fn shapes_match(members: &[(u32, &Node<'_>)]) -> bool {
    let mut per_member: Vec<HashMap<String, ShapeSlot<'_>>> =
        Vec::with_capacity(members.len());
    for (_, n) in members {
        let mut map = HashMap::new();
        for p in &n.direct {
            let short = strip_prefix(&p.name, &n.label).to_string();
            map.insert(
                short,
                ShapeSlot {
                    default: p.value.default_value(),
                    user_config: p.value.is_user_configurable(),
                },
            );
        }
        per_member.push(map);
    }
    let first = &per_member[0];
    per_member.iter().skip(1).all(|m| {
        m.len() == first.len() && first.iter().all(|(k, v)| m.get(k) == Some(v))
    })
}

/// The subset of a `Parameter`'s state that a family constructor
/// treats as the source of truth. Comparing these tuples across
/// members is our shape-equality check.
///
/// Deliberately NOT included:
/// - `description` — human-authored prose that may reasonably vary
///   per member ("port 0 config" vs "port 1 config") without
///   changing the semantic shape.
/// - `choice_ref` — some IPs give the same logical field
///   different enum-choice sets per index (DCMAC's MAC_PORT0 vs
///   MAC_PORT1..5 CONFIG_C0). The atomicity-fix goal requires
///   allowing these to collapse; per-member choice_refs are
///   unioned at emit time via [`IndexedFamily::members_direct`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShapeSlot<'a> {
    default: &'a str,
    user_config: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{build_tree, TreeOptions};

    fn p(name: &str, default: &str, choice_ref: Option<&str>) -> Parameter {
        Parameter {
            name: name.into(),
            value: ipxact::ParamValue {
                text: default.into(),
                choice_ref: choice_ref.map(Into::into),
                resolve: Some("user".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn strips_trailing_digits() {
        assert_eq!(
            split_trailing_digits("MAC_PORT0"),
            Some(("MAC_PORT".into(), 0))
        );
        assert_eq!(
            split_trailing_digits("MAC_PORT12"),
            Some(("MAC_PORT".into(), 12))
        );
        assert_eq!(split_trailing_digits("MAC_PORT"), None);
        assert_eq!(split_trailing_digits("0"), None);
        assert_eq!(split_trailing_digits(""), None);
    }

    /// Six sibling nodes with matching shape collapse to one family.
    /// Uses 3-segment param names so the recursive tree lands the
    /// per-port params directly on `MAC_PORT<N>` (no sub-children),
    /// which is the shape DCMAC's real params produce.
    #[test]
    fn dcmac_like_leaf_family_collapses() {
        let mut params = Vec::new();
        for n in 0..6 {
            params.push(p(&format!("MAC_PORT{n}_CONFIG"), "200GAUI-4", None));
            params.push(p(&format!("MAC_PORT{n}_ENABLE"), "0", None));
            params.push(p(&format!("MAC_PORT{n}_MODE"), "static", None));
        }
        let opts = TreeOptions { min_split_size: 2 };
        let tree = build_tree(params.iter(), &opts);
        let families = detect_families(&tree, &DetectOptions::default());
        assert_eq!(families.len(), 1, "{families:#?}");
        let fam = &families[0];
        assert_eq!(fam.stem, "MAC_PORT");
        assert_eq!(fam.indices, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(fam.shape.len(), 3);
    }

    /// Nested siblings (each member has its own children) do NOT
    /// collapse — the leaf-only rule keeps them per-N.
    #[test]
    fn nested_family_does_not_collapse() {
        let mut params = Vec::new();
        // Two BARs, each with enough sub-params under `_BRIDGE` and
        // `_QDMA` to trigger sub-nodes.
        for n in 0..2 {
            for kind in ["BRIDGE", "QDMA"] {
                for i in 0..5 {
                    params.push(p(
                        &format!("PF0_BAR{n}_{kind}_FIELD{i}"),
                        "0",
                        None,
                    ));
                }
            }
        }
        let opts = TreeOptions { min_split_size: 2 };
        let tree = build_tree(params.iter(), &opts);
        let families = detect_families(&tree, &DetectOptions::default());
        assert!(
            families.iter().all(|f| f.stem != "PF0_BAR"),
            "{families:#?}"
        );
    }

    /// Sibling shapes that disagree (one has an extra field) fall
    /// back to per-N.
    #[test]
    fn shape_mismatch_falls_back_to_per_n() {
        // port 0 has 3 fields; port 1 has only 2 (extra `_C` missing).
        let params = [
            p("MAC_PORT0_A", "0", None),
            p("MAC_PORT0_B", "0", None),
            p("MAC_PORT0_C", "0", None),
            p("MAC_PORT1_A", "0", None),
            p("MAC_PORT1_B", "0", None),
        ];
        let opts = TreeOptions { min_split_size: 2 };
        let tree = build_tree(params.iter(), &opts);
        let families = detect_families(&tree, &DetectOptions::default());
        assert!(families.is_empty(), "{families:#?}");
    }

    /// Sibling shapes with different defaults for the same field
    /// also fall back to per-N — the collapsed constructor would
    /// mis-report a shared default.
    #[test]
    fn different_defaults_prevent_collapse() {
        let params = [
            p("MAC_PORT0_CONFIG", "200GAUI-4", None),
            p("MAC_PORT1_CONFIG", "400GAUI-8", None),
        ];
        let opts = TreeOptions { min_split_size: 1 };
        let tree = build_tree(params.iter(), &opts);
        let families = detect_families(&tree, &DetectOptions::default());
        assert!(families.is_empty(), "{families:#?}");
    }

    /// Different `choice_ref` values across members SHOULD still
    /// collapse — the constructor unions enum values at emit time.
    /// (See ShapeSlot's doc comment for the rationale.)
    #[test]
    fn different_choice_refs_do_collapse() {
        let mut params = Vec::new();
        for n in 0..3 {
            params.push(p(
                &format!("MAC_PORT{n}_CONFIG"),
                "100CAUI-4",
                Some(&format!("port{n}_choices")),
            ));
            params.push(p(&format!("MAC_PORT{n}_ENABLE"), "0", None));
        }
        let opts = TreeOptions { min_split_size: 2 };
        let tree = build_tree(params.iter(), &opts);
        let families = detect_families(&tree, &DetectOptions::default());
        assert_eq!(families.len(), 1, "{families:#?}");
    }

    /// Single-shape IPs (no sibling groups at all) produce zero
    /// families.
    #[test]
    fn single_shape_ip_produces_no_families() {
        let params = [
            p("ONE", "0", None),
            p("TWO", "0", None),
            p("THREE", "0", None),
        ];
        let tree = build_tree(params.iter(), &TreeOptions::default());
        let families = detect_families(&tree, &DetectOptions::default());
        assert!(families.is_empty(), "{families:#?}");
    }

    /// `excluded_stems` skips detection for named stems even when
    /// the guardrails would collapse them.
    #[test]
    fn excluded_stem_stays_per_n() {
        let mut params = Vec::new();
        for n in 0..3 {
            params.push(p(&format!("MAC_PORT{n}_A"), "0", None));
            params.push(p(&format!("MAC_PORT{n}_B"), "0", None));
        }
        let opts = TreeOptions { min_split_size: 2 };
        let tree = build_tree(params.iter(), &opts);
        let opts = DetectOptions {
            excluded_stems: vec!["MAC_PORT".into()],
        };
        let families = detect_families(&tree, &opts);
        assert!(families.is_empty(), "{families:#?}");
    }
}
