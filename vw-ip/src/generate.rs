// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Emit an htcl wrapper proc for an IP-XACT component.
//!
//! Two shapes, picked by `split_threshold`:
//!
//! - **Single-proc** (small IPs like CIPS with 19 params): one
//!   `create_<name>` proc whose structured args mirror the IP's
//!   parameters. Each arg gets `@default(<value>)` from the IP-XACT
//!   default; `@enum(...)` when the parameter has a `choiceRef`. The
//!   body emits `set_property -dict [list ...]` mapping each arg back
//!   to its `CONFIG.<NAME>` key.
//!
//! - **Split** (large IPs like CPM5 with ~8700 params): a top
//!   `create_<name>` proc that just creates the bd_cell and returns
//!   its handle, plus one `create_<name>_<group>` sub-proc per
//!   parameter prefix group. Each sub-proc takes the cell handle as
//!   its first arg, then its own group's parameters. Small groups
//!   (< `min_group_size`) collapse into a `_misc` sub-proc so we end
//!   up with a manageable handful rather than dozens of singletons.
//!
//!   Call-site composition:
//!   ```tcl
//!   set cps [create_cpm5 cps]
//!   create_cpm5_pcie1 $cps -max_link_speed "32.0_GT/s" -modes PCIE
//!   ```

use std::fmt::Write;

use ipxact::{Component, Parameter};
use vw_htcl::emit::{Command, Doc, Item, Word};

use crate::tree::{build_tree, strip_prefix, Node, TreeOptions};

#[derive(Clone, Debug)]
pub struct GenerateOptions {
    /// Emit `## <description>` doc comments for parameters that have a
    /// description in IP-XACT.
    pub include_descriptions: bool,
    /// Skip auto-resolve parameters; emit only user-configurable ones.
    pub user_configurable_only: bool,
    /// When the parameter count exceeds this, the generator emits a
    /// hierarchy of procs instead of one. Tuned so CIPS (19) stays
    /// single and CPM5 (8673) splits.
    pub split_threshold: usize,
    /// Don't split a subgroup into its own child proc unless it has at
    /// least this many parameters. Smaller subgroups stay as direct
    /// args of the parent so we don't get a long tail of singleton
    /// procs.
    pub min_split_size: usize,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            include_descriptions: true,
            user_configurable_only: true,
            split_threshold: 100,
            min_split_size: 8,
        }
    }
}

/// Generate the htcl wrapper text for `component`.
///
/// `presets` carries supplementary parameter-value information loaded
/// from out-of-band sources (e.g. Vivado's `cpm_preset*.xml`); pass an
/// empty map when there are none. Values from `presets` are merged
/// with the IP-XACT `<choice>` entries when emitting `@enum(...)`.
pub fn generate(
    component: &Component,
    presets: &crate::presets::PresetMap,
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
    opts: &GenerateOptions,
) -> String {
    let parameters: Vec<&Parameter> = component
        .component_parameters()
        .filter(|p| {
            !opts.user_configurable_only || p.value.is_user_configurable()
        })
        .collect();
    let mut out = if parameters.len() > opts.split_threshold {
        generate_split(component, presets, &parameters, opts)
    } else {
        generate_single(component, presets, &parameters, opts)
    };
    if !dict_schemas.is_empty() {
        append_dict_sub_procs(&mut out, component, dict_schemas, opts);
    }
    out
}

/// Append `create_<ip>_<param>` sub-procs — one for each IP-XACT
/// `structured_tcldict` parameter we have a schema for. Each builds a
/// Tcl dict-list of `KEY VALUE` pairs the user passes back into the
/// top proc's `-<param>` argument.
fn append_dict_sub_procs(
    out: &mut String,
    component: &Component,
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
    opts: &GenerateOptions,
) {
    let ip_name = sanitize_ident(&component.name);
    let top_proc = format!("create_{ip_name}");
    let mut keys: Vec<&String> = dict_schemas.keys().collect();
    keys.sort();
    for param_name in keys {
        let schema = &dict_schemas[param_name];
        if schema.fields.is_empty() {
            continue;
        }
        writeln!(out).unwrap();
        emit_dict_sub_proc(out, &top_proc, param_name, schema, opts);
    }
}

fn emit_dict_sub_proc(
    out: &mut String,
    top_proc: &str,
    param_name: &str,
    schema: &crate::DictSchema,
    opts: &GenerateOptions,
) {
    let suffix = sanitize_ident(&param_name.to_ascii_lowercase());
    let sub_name = format!("{top_proc}_{suffix}");

    let mut doc = Doc::new();
    doc.push(Item::DocComment(format!(
        "Apply a `CONFIG.{param_name}` value to a block-design cell.",
    )));
    doc.push(Item::DocComment(format!(
        "Pass the cell handle returned by `{top_proc}`.",
    )));
    doc.push(Item::Blank);
    doc.push(Item::DocComment(
        "Block-design cell to set the property on.".into(),
    ));
    doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words: vec![Word::Bare("cell".into())],
        body: None,
    }));
    if !schema.fields.is_empty() {
        doc.push(Item::Blank);
    }
    for f in &schema.fields {
        emit_dict_field_arg(&mut doc, f, opts);
    }

    let mut body = String::new();
    writeln!(body, "set_property -dict [list \\").unwrap();
    writeln!(body, "  CONFIG.{param_name} [list \\").unwrap();
    let n = schema.fields.len();
    for (i, f) in schema.fields.iter().enumerate() {
        let arg = lowercase_ident(&f.name);
        let cont = if i + 1 == n { "" } else { " \\" };
        writeln!(body, "    {} ${arg}{cont}", f.name).unwrap();
    }
    writeln!(body, "  ] \\").unwrap();
    writeln!(body, "] $cell").unwrap();
    emit_proc(out, &sub_name, &doc, &body);
}

fn emit_dict_field_arg(
    doc: &mut Doc,
    f: &crate::DictField,
    opts: &GenerateOptions,
) {
    if opts.include_descriptions {
        if let Some(desc) = f.description.as_deref().filter(|s| !s.is_empty()) {
            for line in desc.lines() {
                doc.push(Item::DocComment(line.trim_end().into()));
            }
        }
    }
    let mut words = Vec::new();
    if !f.enum_values.is_empty() {
        let formatted: Vec<String> = f
            .enum_values
            .iter()
            .map(|v| format_attribute_value(v))
            .collect();
        words.push(Word::Raw(format!("@enum({})", formatted.join(", "))));
    }
    // Dict fields are always optional: Vivado treats an unset
    // inner key as "use the IP's implicit default", so make every
    // arg defaultable. When the Xilinx CSV didn't yield a default —
    // either the row was missing one or the value had unbalanced
    // braces and was rejected — fall back to an empty string so the
    // user can omit the arg and let Vivado decide.
    words.push(Word::Raw(format!(
        "@default({})",
        format_attribute_value(&f.default)
    )));
    let lowered = lowercase_ident(&f.name);
    words.push(Word::Bare(lowered));
    doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words,
        body: None,
    }));
}

// ---------------------------------------------------------------------------
// Single-proc shape.
// ---------------------------------------------------------------------------

fn generate_single(
    component: &Component,
    presets: &crate::presets::PresetMap,
    parameters: &[&Parameter],
    opts: &GenerateOptions,
) -> String {
    let vlnv = component.vlnv();
    let proc_name = format!("create_{}", sanitize_ident(&component.name));

    let mut out = String::new();
    emit_file_header(&mut out, component, &vlnv);
    writeln!(
        out,
        "## ({} configurable parameter{})",
        parameters.len(),
        if parameters.len() == 1 { "" } else { "s" }
    )
    .unwrap();

    let mut proc_doc = Doc::new();
    proc_doc.push(Item::DocComment(
        "Instance name in the block design.".into(),
    ));
    proc_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    if !parameters.is_empty() {
        proc_doc.push(Item::Blank);
    }
    for p in parameters {
        emit_arg_decl(&mut proc_doc, component, presets, p, opts, "");
    }

    let body = build_single_body(&vlnv, parameters);
    emit_proc(&mut out, &proc_name, &proc_doc, &body);
    out
}

fn build_single_body(vlnv: &str, parameters: &[&Parameter]) -> String {
    let mut out = String::new();
    writeln!(out, "set cell [create_bd_cell -type ip -vlnv {vlnv} $name]")
        .unwrap();
    if parameters.is_empty() {
        return out;
    }
    write_set_property_dict(&mut out, parameters, "");
    out
}

// ---------------------------------------------------------------------------
// Split shape: top proc + one sub-proc per prefix group.
// ---------------------------------------------------------------------------

fn generate_split(
    component: &Component,
    presets: &crate::presets::PresetMap,
    parameters: &[&Parameter],
    opts: &GenerateOptions,
) -> String {
    let vlnv = component.vlnv();
    let ip_name = sanitize_ident(&component.name);
    let top_proc = format!("create_{ip_name}");

    let tree = build_tree(
        parameters.iter().copied(),
        &TreeOptions {
            min_split_size: opts.min_split_size,
        },
    );

    // Collect every node that will emit a proc — the root for the
    // top-level `create_<ip>` and every non-root node that has at least
    // one direct parameter to configure.
    let all_nodes = tree.collect();
    let emit_nodes: Vec<&Node> = all_nodes
        .iter()
        .copied()
        .filter(|n| n.label.is_empty() || !n.direct.is_empty())
        .collect();

    let mut out = String::new();
    emit_file_header(&mut out, component, &vlnv);
    writeln!(
        out,
        "## {} configurable parameter{} across {} proc{}.",
        parameters.len(),
        if parameters.len() == 1 { "" } else { "s" },
        emit_nodes.len(),
        if emit_nodes.len() == 1 { "" } else { "s" }
    )
    .unwrap();
    writeln!(out, "##").unwrap();
    writeln!(out, "## Usage:").unwrap();
    writeln!(out, "##   set cell [{top_proc} <name>]").unwrap();
    writeln!(
        out,
        "##   <sub-proc> $cell -<flag> <value> ...   ;# tab-complete by prefix"
    )
    .unwrap();

    // Top proc: creates the cell and returns it. Any tree-root direct
    // params live here too — though for IPs whose params all share a
    // common first segment (CPM5, CIPS), the root has none.
    let mut top_doc = Doc::new();
    top_doc.push(Item::DocComment(
        "Instance name in the block design.".into(),
    ));
    top_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    if !tree.direct.is_empty() {
        top_doc.push(Item::Blank);
        for p in &tree.direct {
            emit_arg_decl(&mut top_doc, component, presets, p, opts, "");
        }
    }
    let mut top_body =
        format!("set cell [create_bd_cell -type ip -vlnv {vlnv} $name]\n");
    if !tree.direct.is_empty() {
        write_set_property_dict(&mut top_body, &tree.direct, "");
    }
    writeln!(top_body, "return $cell").unwrap();
    emit_proc(&mut out, &top_proc, &top_doc, &top_body);

    // One proc per non-root node that has direct parameters.
    for n in emit_nodes.iter().filter(|n| !n.label.is_empty()) {
        writeln!(out).unwrap();
        let suffix = sanitize_ident(&n.label.to_ascii_lowercase());
        let sub_name = format!("{top_proc}_{suffix}");

        let mut sub_doc = Doc::new();
        sub_doc.push(Item::DocComment(format!(
            "Block-design cell handle returned by `{top_proc}`.",
        )));
        sub_doc.push(Item::Command(Command::call(
            "cell",
            std::iter::empty::<Word>(),
        )));
        if !n.direct.is_empty() {
            sub_doc.push(Item::Blank);
        }
        for p in &n.direct {
            emit_arg_decl(&mut sub_doc, component, presets, p, opts, &n.label);
        }

        let mut body = String::new();
        write_set_property_dict(&mut body, &n.direct, &n.label);
        emit_proc(&mut out, &sub_name, &sub_doc, &body);
    }

    out
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

fn emit_file_header(out: &mut String, component: &Component, vlnv: &str) {
    if let Some(desc) =
        component.description.as_deref().filter(|s| !s.is_empty())
    {
        for line in desc.lines() {
            writeln!(out, "## {}", line.trim_end()).unwrap();
        }
        writeln!(out, "##").unwrap();
    }
    writeln!(out, "## Source IP-XACT: {vlnv}").unwrap();
}

/// Emit `proc <name> { <args> } { <body> }` with the args and body
/// indented two spaces each.
fn emit_proc(out: &mut String, name: &str, args: &Doc, body: &str) {
    let args_text = args.to_string();
    writeln!(out, "proc {name} {{").unwrap();
    for line in args_text.lines() {
        if line.is_empty() {
            writeln!(out).unwrap();
        } else {
            writeln!(out, "  {line}").unwrap();
        }
    }
    writeln!(out, "}} {{").unwrap();
    for line in body.lines() {
        if line.is_empty() {
            writeln!(out).unwrap();
        } else {
            writeln!(out, "  {line}").unwrap();
        }
    }
    writeln!(out, "}}").unwrap();
}

/// Emit `set_property -dict [list \ … ]` for `parameters`. Arg names
/// are built by stripping `prefix_to_strip` from each parameter's full
/// IP-XACT name (so a `CPM_PCIE1_PF0_BAR0_ENABLED` parameter inside a
/// proc scoped at `CPM_PCIE1_PF0_BAR0` reads back as `$enabled`),
/// while the `CONFIG.<NAME>` key keeps the full name Vivado expects.
fn write_set_property_dict(
    out: &mut String,
    parameters: &[&Parameter],
    prefix_to_strip: &str,
) {
    writeln!(out, "set_property -dict [list \\").unwrap();
    for p in parameters {
        let arg = lowercase_ident(strip_prefix(&p.name, prefix_to_strip));
        writeln!(out, "  CONFIG.{} ${arg} \\", p.name).unwrap();
    }
    writeln!(out, "] $cell").unwrap();
}

fn emit_arg_decl(
    doc: &mut Doc,
    component: &Component,
    presets: &crate::presets::PresetMap,
    p: &Parameter,
    opts: &GenerateOptions,
    prefix_to_strip: &str,
) {
    if opts.include_descriptions {
        if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
            for line in desc.lines() {
                doc.push(Item::DocComment(line.trim_end().into()));
            }
        }
    }
    let mut words = Vec::new();
    let enum_values = enum_values_for(component, presets, p);
    if !enum_values.is_empty() {
        let formatted: Vec<String> = enum_values
            .iter()
            .map(|v| format_attribute_value(v))
            .collect();
        words.push(Word::Raw(format!("@enum({})", formatted.join(", "))));
    }
    let default = p.value.default_value();
    if !default.is_empty() {
        words.push(Word::Raw(format!(
            "@default({})",
            format_attribute_value(default)
        )));
    }
    let lowered = lowercase_ident(strip_prefix(&p.name, prefix_to_strip));
    words.push(Word::Bare(lowered));
    doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words,
        body: None,
    }));
}

/// Union of the parameter's IP-XACT `<choice>` values and any
/// presets, in insertion order. Order is *IP-XACT first* (preserving
/// the vendor's intended ordering when both sources agree) followed
/// by preset-only values; duplicates are filtered.
fn enum_values_for(
    component: &Component,
    presets: &crate::presets::PresetMap,
    p: &Parameter,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    if let Some(choice) = p
        .value
        .choice_ref
        .as_deref()
        .and_then(|name| component.find_choice(name))
    {
        for e in &choice.enumerations {
            if seen.insert(e.value.clone()) {
                out.push(e.value.clone());
            }
        }
    }
    if let Some(extra) = presets.get(&p.name) {
        for v in extra {
            if seen.insert(v.clone()) {
                out.push(v.clone());
            }
        }
    }
    out
}

/// Lowercase an IP-XACT parameter name into a valid htcl argument
/// name. The htcl proc-arg grammar is `/[a-zA-Z_][a-zA-Z0-9_]*/`, so
/// an empty result or a digit-leading result (which prefix-stripping
/// can produce — e.g. `64BIT` after stripping `CPM_PCIE1_PF0_BAR0_`)
/// gets a leading underscore to land back in the grammar.
fn lowercase_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 1);
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let needs_leading_underscore = out
        .as_bytes()
        .first()
        .map(|b| b.is_ascii_digit())
        .unwrap_or(true);
    if needs_leading_underscore {
        out.insert(0, '_');
    }
    out
}

/// Sanitize an arbitrary string for use as an htcl identifier.
fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Render an IP-XACT default value as it should appear inside an
/// `@default(...)` attribute. The htcl proc-args grammar accepts only
/// three attribute-value forms — `integer_literal`, `attribute_value_ident`
/// (`[a-zA-Z_][a-zA-Z0-9_]*`), and double-quoted strings. Anything that
/// isn't a clean ident or integer is double-quoted (with `"` escaped).
fn format_attribute_value(s: &str) -> String {
    if is_integer_literal(s) || is_attribute_ident(s) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn is_integer_literal(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

fn is_attribute_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipxact::{
        Choice, Choices, Component, Enumeration, ParamValue, Parameter,
        Parameters,
    };

    fn mk_component() -> Component {
        Component {
            vendor: "acme".into(),
            library: "ip".into(),
            name: "demo".into(),
            version: "1.0".into(),
            description: Some("A demo IP.".into()),
            parameters: Some(Parameters {
                entries: vec![
                    Parameter {
                        name: "BUS_WIDTH".into(),
                        description: Some("Bus width in bits.".into()),
                        value: ParamValue {
                            text: "32".into(),
                            resolve: Some("user".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    Parameter {
                        name: "MODE".into(),
                        value: ParamValue {
                            text: "FAST".into(),
                            resolve: Some("user".into()),
                            choice_ref: Some("mode_choices".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ],
            }),
            choices: Some(Choices {
                entries: vec![Choice {
                    name: "mode_choices".into(),
                    enumerations: vec![
                        Enumeration {
                            value: "FAST".into(),
                            ..Default::default()
                        },
                        Enumeration {
                            value: "SLOW".into(),
                            ..Default::default()
                        },
                    ],
                }],
            }),
            ..Default::default()
        }
    }

    fn mk_split_component(n_per_group: usize) -> Component {
        // Build a component with two big groups and a smattering of
        // small ones, all above the split threshold.
        let mut entries = Vec::new();
        for i in 0..n_per_group {
            entries.push(Parameter {
                name: format!("BIG_ONE_FIELD{i}"),
                value: ParamValue {
                    text: "0".into(),
                    resolve: Some("user".into()),
                    ..Default::default()
                },
                ..Default::default()
            });
            entries.push(Parameter {
                name: format!("BIG_TWO_FIELD{i}"),
                value: ParamValue {
                    text: "1".into(),
                    resolve: Some("user".into()),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        // A pair of tiny groups that should be collapsed into _misc.
        for name in ["TINY_A_ONE", "TINY_B_ONE", "TINY_C_ONE", "STRAY_THING"] {
            entries.push(Parameter {
                name: name.into(),
                value: ParamValue {
                    text: "x".into(),
                    resolve: Some("user".into()),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        Component {
            vendor: "acme".into(),
            library: "ip".into(),
            name: "wide".into(),
            version: "1.0".into(),
            parameters: Some(Parameters { entries }),
            ..Default::default()
        }
    }

    #[test]
    fn single_mode_below_threshold() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        let n_procs =
            out.matches("\nproc ").count() + out.starts_with("proc ") as usize;
        assert_eq!(n_procs, 1, "{out}");
        assert!(out.contains("proc create_demo"));
    }

    #[test]
    fn split_mode_emits_top_and_sub_procs() {
        let component = mk_split_component(60); // 60 * 2 + 4 = 124 params > 100
        let out = generate(
            &component,
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        eprintln!("--- generated ---\n{out}\n--- end ---");
        assert!(out.contains("proc create_wide "));
        assert!(out.contains("proc create_wide_big_one "));
        assert!(out.contains("proc create_wide_big_two "));
        let parsed = vw_htcl::parse(&out);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let diags = vw_htcl::validate(&parsed.document, &out);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == vw_htcl::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn split_sub_procs_take_cell_as_first_arg() {
        let component = mk_split_component(60);
        let out = generate(
            &component,
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        // Sub-proc args block starts with the `cell` arg.
        assert!(out.contains(
            "proc create_wide_big_one {\n  ## Block-design cell handle"
        ));
        assert!(out.contains("cell\n"));
    }

    #[test]
    fn tiny_groups_land_on_the_parent_proc() {
        let component = mk_split_component(60);
        let out = generate(
            &component,
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        // None of the tiny prefix groups becomes its own proc...
        for name in [
            "create_wide_tiny_a ",
            "create_wide_tiny_b ",
            "create_wide_stray ",
        ] {
            assert!(!out.contains(name), "unexpected {name} in:\n{out}");
        }
        // ...and the params instead appear as args on the top proc.
        let top_block = out
            .split_once("proc create_wide_big_one")
            .map(|(top, _)| top.to_string())
            .unwrap_or_else(|| out.clone());
        for arg in ["tiny_a_one", "tiny_b_one", "tiny_c_one", "stray_thing"] {
            assert!(
                top_block.contains(arg),
                "{arg} missing from top: {top_block}"
            );
        }
    }

    #[test]
    fn arg_name_strips_node_prefix() {
        // Two big groups whose internal arg names should be the
        // segments *after* the group prefix, not the full name.
        let entries = (0..10)
            .flat_map(|i| {
                [
                    Parameter {
                        name: format!("GROUP_A_FIELD{i}"),
                        value: ParamValue {
                            text: "0".into(),
                            resolve: Some("user".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    Parameter {
                        name: format!("GROUP_B_FIELD{i}"),
                        value: ParamValue {
                            text: "0".into(),
                            resolve: Some("user".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ]
            })
            .collect();
        let component = Component {
            vendor: "acme".into(),
            library: "ip".into(),
            name: "demo".into(),
            version: "1.0".into(),
            parameters: Some(Parameters { entries }),
            ..Default::default()
        };
        let opts = GenerateOptions {
            split_threshold: 5,
            ..GenerateOptions::default()
        };
        let out = generate(
            &component,
            &Default::default(),
            &::std::collections::HashMap::new(),
            &opts,
        );
        // Inside the GROUP_A proc, the arg names should be `field0`,
        // not `group_a_field0`.
        assert!(out.contains("@default(0) field0\n"), "{out}");
        assert!(!out.contains("group_a_field0"), "{out}");
        // The CONFIG.<NAME> mapping keeps the full IP-XACT name.
        assert!(out.contains("CONFIG.GROUP_A_FIELD0 $field0"), "{out}");
    }

    #[test]
    fn generated_output_parses_back() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        let parsed = vw_htcl::parse(&out);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
    }

    #[test]
    fn includes_description_as_doc_comment() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        assert!(out.contains("## A demo IP."), "{out}");
        assert!(out.contains("## Bus width in bits."), "{out}");
    }

    #[test]
    fn emits_default_and_enum_attributes() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        assert!(out.contains("@default(32) bus_width"), "{out}");
        assert!(out.contains("@enum(FAST, SLOW)"), "{out}");
        assert!(out.contains("@default(FAST) mode"), "{out}");
    }

    #[test]
    fn emits_set_property_for_each_param() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        );
        assert!(out.contains("CONFIG.BUS_WIDTH $bus_width"), "{out}");
        assert!(out.contains("CONFIG.MODE $mode"), "{out}");
    }
}
