// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Derive parameter groups from naming conventions.
//!
//! IP-XACT components published by Xilinx carry no machine-readable
//! grouping for their configuration parameters. The xgui Tcl scripts
//! that drive the GUI grouping are encrypted, so they're not a source
//! we can use. What we *can* use is the strong prefix structure of the
//! parameter names themselves: in CPM5, 4200 parameters start with
//! `CPM_PCIE0_`, another 4200 with `CPM_PCIE1_`, 136 with `CPM_CCIX_`,
//! and so on. That structure is the right grain for a sub-proc.
//!
//! The grouping strategy:
//!
//! 1. Split each parameter name on `_`.
//! 2. Take the first N segments as the group key. We pick N to balance
//!    group cardinality vs. group size — small enough that there are
//!    few groups (so each becomes a manageable proc), big enough that
//!    no single group is so huge it's just a flat dump.
//! 3. Parameters with no underscore, or whose only group would be a
//!    singleton, fall into a catch-all `_misc` group.

use std::collections::BTreeMap;

use ipxact::Parameter;

#[derive(Clone, Debug)]
pub struct ParameterGroup<'a> {
    /// Key used as the group name (e.g. `CPM_PCIE0`).
    pub key: String,
    /// Parameters in this group, in input order.
    pub parameters: Vec<&'a Parameter>,
}

/// Group parameters by their leading underscore-separated segments.
/// `prefix_segments` controls how many leading segments form the key:
/// 1 = `CPM`, 2 = `CPM_PCIE0`, etc. 2 is the right default for Xilinx's
/// big IPs; their first segment is a coarse domain (`CPM`, `PS`, `PMC`)
/// and the second names the controller / subsystem.
pub fn group_parameters<'a, I>(
    parameters: I,
    prefix_segments: usize,
) -> Vec<ParameterGroup<'a>>
where
    I: IntoIterator<Item = &'a Parameter>,
{
    // BTreeMap keeps groups in a stable, readable order.
    let mut groups: BTreeMap<String, Vec<&'a Parameter>> = BTreeMap::new();
    for p in parameters {
        let key = prefix_key(&p.name, prefix_segments);
        groups.entry(key).or_default().push(p);
    }
    groups
        .into_iter()
        .map(|(key, parameters)| ParameterGroup { key, parameters })
        .collect()
}

/// First `n` underscore-separated segments of `name`. If `name` has
/// fewer than `n` segments (or no underscores), returns the whole name.
/// Empty names map to the literal `_misc`.
fn prefix_key(name: &str, n: usize) -> String {
    if name.is_empty() {
        return "_misc".into();
    }
    let mut out = String::new();
    for (i, seg) in name.split('_').enumerate().take(n) {
        if i > 0 {
            out.push('_');
        }
        out.push_str(seg);
    }
    // If the name has fewer than `n` segments, we end up with the full
    // name as the key — that's fine; it just means the group is named
    // after the parameter itself. Singletons coalesce later if we want.
    out
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
    fn groups_by_two_prefix_segments() {
        let params = [
            p("CPM_PCIE0_FOO"),
            p("CPM_PCIE0_BAR"),
            p("CPM_PCIE1_BAZ"),
            p("CPM_CCIX_QUX"),
        ];
        let groups = group_parameters(&params, 2);
        let by_key: Vec<_> = groups
            .iter()
            .map(|g| (g.key.clone(), g.parameters.len()))
            .collect();
        assert_eq!(
            by_key,
            vec![
                ("CPM_CCIX".to_string(), 1),
                ("CPM_PCIE0".to_string(), 2),
                ("CPM_PCIE1".to_string(), 1),
            ]
        );
    }

    #[test]
    fn names_with_fewer_segments_become_their_own_key() {
        let params = [p("FOO"), p("FOO_BAR_BAZ")];
        let groups = group_parameters(&params, 2);
        let keys: Vec<_> = groups.iter().map(|g| g.key.as_str()).collect();
        // "FOO" stays "FOO" (only one segment), "FOO_BAR_BAZ" becomes "FOO_BAR".
        assert!(keys.contains(&"FOO"));
        assert!(keys.contains(&"FOO_BAR"));
    }
}
