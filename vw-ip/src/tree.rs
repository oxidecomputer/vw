// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Recursive prefix tree over parameter names.
//!
//! Big Xilinx IPs encode their configuration hierarchy in parameter
//! names, not in IP-XACT structure: `CPM_PCIE1_PF0_BAR0_64BIT` lives
//! under PCIE1 → PF0 → BAR0, but that's all conveyed by underscores.
//! A flat depth-1 grouping leaves PCIE1 with ~4200 args, which is
//! useless in an LSP. We recurse: at each depth, partition by the next
//! segment; subgroups bigger than `min_split_size` become children
//! that recurse again; everything smaller absorbs into the current
//! node as direct parameters. Generation walks the tree and emits one
//! proc per node, which keeps every proc small enough to navigate by
//! flag completion.

use std::collections::BTreeMap;

use ipxact::Parameter;

#[derive(Clone, Debug)]
pub struct TreeOptions {
    /// Don't split a subgroup into its own child node unless it has at
    /// least this many parameters. Smaller subgroups stay as direct
    /// args of the parent — keeps singleton segments from becoming
    /// their own procs.
    pub min_split_size: usize,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self { min_split_size: 8 }
    }
}

#[derive(Clone, Debug)]
pub struct Node<'a> {
    /// Full underscore-joined prefix that names this node
    /// (e.g. `CPM_PCIE1_PF0`). Empty for the root.
    pub label: String,
    /// Underscore-separated depth: 0 at root, 1 for `CPM`, 2 for
    /// `CPM_PCIE1`, 3 for `CPM_PCIE1_PF0`, ...
    pub depth: usize,
    /// Parameters whose proc-level args belong on *this* node. Their
    /// arg names are derived by stripping the node's prefix.
    pub direct: Vec<&'a Parameter>,
    /// Child nodes, keyed by their additional segment.
    pub children: Vec<Node<'a>>,
}

impl<'a> Node<'a> {
    /// Total parameters reachable from this node, including children.
    pub fn total_params(&self) -> usize {
        self.direct.len()
            + self.children.iter().map(Node::total_params).sum::<usize>()
    }

    /// Number of nodes in this subtree, including self.
    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(Node::node_count).sum::<usize>()
    }

    /// Pre-order walk: visit self, then each child recursively.
    pub fn walk(&self, f: &mut impl FnMut(&Node<'a>)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }

    /// Collect references to every node in this subtree in pre-order.
    /// Used by code-gen, which needs to iterate the tree twice (once
    /// for the header summary, once to emit procs) without re-walking
    /// through a closure that can't escape `&Node` references.
    pub fn collect<'t>(&'t self) -> Vec<&'t Node<'a>> {
        let mut out = Vec::new();
        self.collect_into(&mut out);
        out
    }

    fn collect_into<'t>(&'t self, out: &mut Vec<&'t Node<'a>>) {
        out.push(self);
        for c in &self.children {
            c.collect_into(out);
        }
    }
}

/// Build the prefix tree from a flat parameter list.
pub fn build_tree<'a, I>(params: I, opts: &TreeOptions) -> Node<'a>
where
    I: IntoIterator<Item = &'a Parameter>,
{
    build_node(0, String::new(), params.into_iter().collect(), opts)
}

fn build_node<'a>(
    depth: usize,
    label: String,
    params: Vec<&'a Parameter>,
    opts: &TreeOptions,
) -> Node<'a> {
    let mut direct: Vec<&'a Parameter> = Vec::new();
    let mut subgroups: BTreeMap<String, Vec<&'a Parameter>> = BTreeMap::new();

    for p in params {
        let segs: Vec<&str> = p.name.split('_').collect();
        if depth + 1 >= segs.len() {
            // No further segments to split on — this parameter belongs
            // to the current node directly.
            direct.push(p);
        } else {
            // Group by the segment at position `depth` — the next one
            // not yet absorbed into the label.
            subgroups
                .entry(segs[depth].to_string())
                .or_default()
                .push(p);
        }
    }

    let mut children = Vec::new();
    for (seg, group) in subgroups {
        // A subgroup that's smaller than the split threshold isn't
        // worth its own proc — keep its parameters at this level.
        if group.len() < opts.min_split_size {
            direct.extend(group);
            continue;
        }
        let child_label = if label.is_empty() {
            seg.clone()
        } else {
            format!("{label}_{seg}")
        };
        children.push(build_node(depth + 1, child_label, group, opts));
    }

    Node {
        label,
        depth,
        direct,
        children,
    }
}

/// Return the portion of `param_name` after the node's `label_prefix`
/// (and the underscore separating them). Used so arg names inside a
/// node's proc don't redundantly repeat the prefix.
pub fn strip_prefix<'a>(param_name: &'a str, label_prefix: &str) -> &'a str {
    if label_prefix.is_empty() {
        return param_name;
    }
    if let Some(rest) = param_name.strip_prefix(label_prefix) {
        rest.strip_prefix('_').unwrap_or(rest)
    } else {
        param_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> Parameter {
        Parameter {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_input_returns_empty_root() {
        let tree =
            build_tree(Vec::<&Parameter>::new(), &TreeOptions::default());
        assert_eq!(tree.label, "");
        assert_eq!(tree.direct.len(), 0);
        assert_eq!(tree.children.len(), 0);
    }

    #[test]
    fn singletons_stay_at_root() {
        let params = [p("A"), p("B"), p("C")];
        let opts = TreeOptions::default();
        let tree = build_tree(params.iter(), &opts);
        // Each is one segment, no subgroups; all direct at root.
        assert_eq!(tree.direct.len(), 3);
        assert!(tree.children.is_empty());
    }

    #[test]
    fn splits_when_subgroup_exceeds_threshold() {
        let mut params: Vec<Parameter> =
            (0..10).map(|i| p(&format!("CPM_PCIE1_FIELD{i}"))).collect();
        params.extend((0..10).map(|i| p(&format!("CPM_PCIE0_FIELD{i}"))));
        let opts = TreeOptions { min_split_size: 5 };
        let tree = build_tree(params.iter(), &opts);
        // Root has one child `CPM`; under `CPM`, children `PCIE0` and
        // `PCIE1`, each with 10 direct params.
        assert_eq!(tree.children.len(), 1);
        let cpm = &tree.children[0];
        assert_eq!(cpm.label, "CPM");
        assert_eq!(cpm.children.len(), 2);
        for c in &cpm.children {
            assert_eq!(c.direct.len(), 10);
        }
    }

    #[test]
    fn nested_hierarchy_splits_recursively() {
        // Mimic PCIE1's structure: a bunch of PF0/PF1/PF2 sub-trees,
        // each with BARs and CAPs.
        let mut params: Vec<Parameter> = Vec::new();
        for pf in 0..3 {
            for bar in 0..6 {
                for f in 0..10 {
                    params.push(p(&format!(
                        "CPM_PCIE1_PF{pf}_BAR{bar}_FIELD{f}"
                    )));
                }
            }
            for cap in 0..3 {
                for f in 0..5 {
                    params.push(p(&format!(
                        "CPM_PCIE1_PF{pf}_CAP{cap}_FIELD{f}"
                    )));
                }
            }
        }
        let opts = TreeOptions { min_split_size: 5 };
        let tree = build_tree(params.iter(), &opts);
        // Drill into the tree: root → CPM → PCIE1 → PF0/PF1/PF2.
        let cpm = &tree.children[0];
        let pcie1 = &cpm.children[0];
        assert_eq!(pcie1.label, "CPM_PCIE1");
        assert_eq!(pcie1.children.len(), 3); // PF0, PF1, PF2
        let pf0 = &pcie1.children[0];
        // PF0 should have BAR0..BAR5 + CAP0..CAP2 as children.
        let bar_count = pf0
            .children
            .iter()
            .filter(|c| c.label.contains("BAR"))
            .count();
        assert_eq!(bar_count, 6, "{pf0:#?}");
    }

    #[test]
    fn strip_prefix_returns_local_name() {
        assert_eq!(
            strip_prefix("CPM_PCIE1_PF0_BAR0_ENABLED", "CPM_PCIE1_PF0_BAR0"),
            "ENABLED"
        );
        // No prefix: returns the name unchanged.
        assert_eq!(strip_prefix("FOO", ""), "FOO");
        // Prefix doesn't match: returns unchanged (defensive).
        assert_eq!(strip_prefix("FOO_BAR", "BAZ"), "FOO_BAR");
    }
}
