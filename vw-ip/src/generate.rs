// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Emit an htcl wrapper proc for an IP-XACT component.
//!
//! Two shapes, picked by `split_threshold`. Every emitted proc lives
//! in the IP's own namespace (`<ip>::…`) so name collisions across
//! IPs are structural and callers reach helpers by tab-completing
//! `<ip>::`.
//!
//! - **Single-proc** (small IPs like CIPS with 19 params): one
//!   `<ip>::create` proc whose structured args mirror the IP's
//!   parameters. Each arg gets `@default(<value>)` from the IP-XACT
//!   default; `@enum(...)` when the parameter has a `choiceRef`. The
//!   body emits `set_property -dict [list ...]` mapping each arg back
//!   to its `CONFIG.<NAME>` key.
//!
//! - **Split** (large IPs like CPM5 with ~8700 params): a top
//!   `<ip>::create` proc that just creates the bd_cell and returns
//!   its handle, plus one `<ip>::<group>` sub-proc per parameter
//!   prefix group. Each sub-proc takes the cell handle as its first
//!   arg, then its own group's parameters. Small groups
//!   (< `min_group_size`) collapse into a `_misc` sub-proc so we end
//!   up with a manageable handful rather than dozens of singletons.
//!
//!   Call-site composition:
//!   ```tcl
//!   set cps [cpm5::create cps]
//!   cpm5::pcie1 $cps -max_link_speed "32.0_GT/s" -modes PCIE
//!   ```

use std::fmt::Write;

use ipxact::{Component, Parameter};
use vw_htcl::emit::{Command, Doc, Item, Word};

use crate::family::{detect_families, DetectOptions, IndexedFamily};
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
    /// Indexed-family stems to leave per-N (skip the composed-
    /// constructor collapse). Passed through to
    /// [`crate::family::DetectOptions::excluded_stems`]. Empty by
    /// default; populated from the CLI's `--no-collapse=STEM,STEM,…`
    /// flag when a specific stem needs to opt out.
    pub no_collapse: Vec<String>,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            include_descriptions: true,
            user_configurable_only: true,
            split_threshold: 100,
            min_split_size: 8,
            no_collapse: Vec::new(),
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
        generate_split(component, presets, &parameters, opts, dict_schemas)
    } else {
        generate_single(component, presets, &parameters, opts, dict_schemas)
    };
    if !dict_schemas.is_empty() {
        append_dict_sub_procs(&mut out, component, dict_schemas, opts);
    }
    out
}

/// Emit one compositional-value constructor per IP-XACT
/// `structured_tcldict` parameter. Each schema gets a newtype
/// prelude (`namespace eval`, `type <T> = Properties`,
/// `::repr`/`::from`/`::to`/`::empty`) plus a pure `<ip>::<param>`
/// constructor that returns the typed value. The top proc's
/// matching kwarg (e.g. `-ps_pmc_config`) then takes the newtype
/// and unwraps it into the atomic `set_property -dict` — no
/// separate mutator call.
fn append_dict_sub_procs(
    out: &mut String,
    component: &Component,
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
    opts: &GenerateOptions,
) {
    let ip_name = sanitize_ident(&component.name);
    let mut keys: Vec<&String> = dict_schemas.keys().collect();
    keys.sort();
    for param_name in keys {
        let schema = &dict_schemas[param_name];
        if schema.fields.is_empty() {
            continue;
        }
        writeln!(out).unwrap();
        emit_dict_props_prelude(out, &ip_name, param_name);
        writeln!(out).unwrap();
        emit_dict_sub_proc(out, &ip_name, param_name, schema, opts);
    }
}

/// Emit the newtype declaration + `::from`/`::to`/`::repr`/`::empty`
/// helper procs for one dict-schema param. Mirror of
/// [`emit_family_prelude`] — same shape, different naming source.
fn emit_dict_props_prelude(out: &mut String, ip_name: &str, param_name: &str) {
    let qualified = dict_props_name(ip_name, param_name);
    let ctor_lower = param_name.to_ascii_lowercase();
    writeln!(
        out,
        "## Typed configuration value for [{ip_name}::create]'s \
         `-{ctor_lower}` slot. Construct with [{ip_name}::{ctor_lower}].",
    )
    .unwrap();
    writeln!(out, "namespace eval {ip_name} {{}}").unwrap();
    writeln!(out, "namespace eval {qualified} {{}}").unwrap();
    writeln!(out, "type {qualified} = Properties").unwrap();
    writeln!(
        out,
        "proc {qualified}::repr {{ v: {qualified} }} string \
         {{ return [Properties::repr -v $v] }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::from {{ v: Properties }} {qualified} \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::to {{ v: {qualified} }} Properties \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::empty {{}} {qualified} \
         {{ return [{qualified}::from -v [Properties::empty]] }}"
    )
    .unwrap();
}

/// Emit the value-constructor `<ip>::<param_lower>` for one
/// dict-schema. Pure — no cell, no `-bd`, no `set_property`. Builds
/// a `Properties`-shaped dict from the supplied field kwargs and
/// wraps it in the newtype. The atomic materialization happens
/// later in `<ip>::create`'s body, where the matching
/// `-<param_lower>` kwarg gets unwrapped through `::to` +
/// `Properties::to_raw` and merged into the single
/// `set_property -dict` call.
fn emit_dict_sub_proc(
    out: &mut String,
    ip_name: &str,
    param_name: &str,
    schema: &crate::DictSchema,
    opts: &GenerateOptions,
) {
    let ctor_local = param_name.to_ascii_lowercase();
    let ctor_name = format!("{ip_name}::{ctor_local}");
    let ret_ty = dict_props_name(ip_name, param_name);

    let mut doc = Doc::new();
    doc.push(Item::DocComment(format!(
        "Configuration value for [{ip_name}::create]'s \
         `-{ctor_local}` slot (`CONFIG.{param_name}`). Composes into \
         the top proc so every provided field lands in ONE atomic \
         `set_property -dict` call.",
    )));
    if !schema.fields.is_empty() {
        doc.push(Item::Blank);
    }
    for f in &schema.fields {
        emit_dict_field_arg(&mut doc, f, opts);
    }

    let mut body = String::new();
    writeln!(body, "set _vw_d [dict create]").unwrap();
    for f in &schema.fields {
        let arg = lowercase_ident(&f.name);
        writeln!(
            body,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ dict set _vw_d {} ${arg} }}",
            f.name
        )
        .unwrap();
    }
    writeln!(
        body,
        "return [{ret_ty}::from -v [Properties::from -v $_vw_d]]"
    )
    .unwrap();
    emit_proc(out, &ctor_name, &doc, Some(&ret_ty), &body);
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
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
) -> String {
    let vlnv = component.vlnv();
    let ip_name = sanitize_ident(&component.name);

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
        "Project-level IP module name, or (when `-bd 1`) the \
         instance name in the block design."
            .into(),
    ));
    proc_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    push_bd_switch_arg(&mut proc_doc);
    if !parameters.is_empty() {
        proc_doc.push(Item::Blank);
    }
    for p in parameters {
        emit_arg_decl(
            &mut proc_doc,
            component,
            presets,
            p,
            opts,
            "",
            &ip_name,
            dict_schemas,
        );
    }

    let dict_schema_newtypes =
        build_dict_schema_newtypes(&ip_name, dict_schemas);
    let body = build_single_body(&vlnv, parameters, &dict_schema_newtypes);
    // Emit into a per-namespace buffer, then wrap. Same shape as
    // vivado-cmd's `namespace eval log { ... }` in log.htcl —
    // Tcl requires the namespace to exist before qualified proc
    // names like `<ip>::create` can be created, and wrapping the
    // whole proc block in `namespace eval <ip> { … }` is the
    // idiomatic way to establish it while letting the procs
    // themselves use bare `proc create { … }` shape.
    let mut procs = String::new();
    emit_proc(&mut procs, "create", &proc_doc, Some("bd_cell"), &body);
    write_namespace_block(&mut out, &ip_name, &procs);
    out
}

/// Wrap `body` in `namespace eval <ip> { … }`, indenting each
/// non-empty line by two spaces. The wrapper lets the enclosed
/// procs use bare names (`create`, `mac_port`, …) while remaining
/// externally addressable as `<ip>::<name>` — exactly the shape
/// vivado-cmd's hand-written `log.htcl` / `ip.htcl` use.
fn write_namespace_block(out: &mut String, ip_name: &str, body: &str) {
    writeln!(out, "namespace eval {ip_name} {{").unwrap();
    for line in body.lines() {
        if line.is_empty() {
            writeln!(out).unwrap();
        } else {
            writeln!(out, "  {line}").unwrap();
        }
    }
    writeln!(out, "}}").unwrap();
}

/// Emit the `-bd` switch as a proc-arg declaration.
///
/// `-bd 0` (default) → `create_ip` (project-level IP module);
/// `-bd 1` → `create_bd_cell` (block-design cell). Project IP is
/// the default because it's the shape Vivado's own
/// `write_ip_tcl`-generated scripts use, and most external tools
/// (simulators, downstream regeneration flows) expect wrappers
/// that create discoverable IP source objects. Wrappers going
/// into a block design still work — the caller just passes
/// `-bd 1`. The bool-as-int shape (`@enum(0, 1)`) matches every
/// other yes/no flag the generator emits.
fn push_bd_switch_arg(doc: &mut Doc) {
    doc.push(Item::Blank);
    doc.push(Item::DocComment(
        "Create the IP as a project-level module (`0`, default) via \
         `create_ip`, or as a block-design cell (`1`) via \
         `create_bd_cell`. The returned handle is compatible with the \
         sub-procs either way — Vivado's `set_property -dict …` works \
         on both IP objects and cell paths."
            .into(),
    ));
    doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words: vec![
            Word::Raw("@enum(0, 1)".into()),
            Word::Raw("@default(0)".into()),
            Word::Bare("bd".into()),
        ],
        body: None,
    }));
}

fn build_single_body(
    vlnv: &str,
    parameters: &[&Parameter],
    dict_schema_newtypes: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::new();
    // `-bd` switches between the two Vivado instantiation paths.
    // Default (`-bd 1`) is `create_bd_cell` — the block-design
    // shape most wrappers use. `-bd 0` calls `create_ip` and adds
    // the IP as a project-level source object; the returned handle
    // is still a set_property-compatible object, so downstream
    // sub-procs work unchanged. This mirrors the split-shape body
    // in `generate_split`.
    writeln!(out, "if {{$bd}} {{").unwrap();
    writeln!(
        out,
        "  set cell [vivado_cmd::create_bd_cell -type ip -vlnv {vlnv} -name $name]"
    )
    .unwrap();
    writeln!(out, "}} else {{").unwrap();
    writeln!(
        out,
        "  set cell [vivado_cmd::create_ip -vlnv {vlnv} -module_name $name]"
    )
    .unwrap();
    writeln!(out, "}}").unwrap();
    if !parameters.is_empty() {
        write_set_property_dict(
            &mut out,
            parameters,
            "",
            &[],
            dict_schema_newtypes,
        );
    }
    // Every `create_<ip>` proc must return an identifier the
    // sub-procs can pass to their own `set_property` calls. In bd
    // mode that's `$cell` (a bd_cell path). In ip mode we return
    // `$name` instead — `$cell` is the XCI file path returned by
    // `create_ip`, and downstream sub-procs need the IP's module
    // name so they can look up the object with `[get_ips $cell]`.
    // Same contract, one variable per branch.
    writeln!(out, "if {{$bd}} {{ return $cell }} else {{ return $name }}")
        .unwrap();
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
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
) -> String {
    let vlnv = component.vlnv();
    let ip_name = sanitize_ident(&component.name);
    let top_proc = format!("{ip_name}::create");

    let tree = build_tree(
        parameters.iter().copied(),
        &TreeOptions {
            min_split_size: opts.min_split_size,
        },
    );

    let families = detect_families(
        &tree,
        &DetectOptions {
            excluded_stems: opts.no_collapse.clone(),
        },
    );
    // Set of node labels whose per-N sub-proc we should NOT emit —
    // they collapsed into a family constructor. Their sub-nodes
    // (`MAC_PORT0_RX`, etc.) still emit; only the direct-param
    // per-N node vanishes.
    let collapsed_labels: std::collections::HashSet<String> = families
        .iter()
        .flat_map(|f| {
            f.indices.iter().map(move |i| stem_index_label(&f.stem, *i))
        })
        .collect();

    // Collect every node that will emit a proc — the root for the
    // top-level `<ip>::create` and every non-root node that has at
    // least one direct parameter to configure AND hasn't been
    // collapsed into a family.
    let all_nodes = tree.collect();
    let emit_nodes: Vec<&Node> = all_nodes
        .iter()
        .copied()
        .filter(|n| n.label.is_empty() || !n.direct.is_empty())
        .filter(|n| !collapsed_labels.contains(&n.label))
        .collect();

    // Family-side lookups the top-proc emitter uses.
    let family_merges: Vec<FamilyMerge<'_>> = families
        .iter()
        .map(|f| FamilyMerge {
            stem: f.stem.clone(),
            stem_lower: lowercase_ident(&f.stem),
            indices: f.indices.clone(),
            newtype_qualified: stem_props_name(&ip_name, &f.stem),
            marker: std::marker::PhantomData,
        })
        .collect();

    let dict_schema_newtypes =
        build_dict_schema_newtypes(&ip_name, dict_schemas);

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
        "Project-level IP module name, or (when `-bd 1`) the \
         instance name in the block design."
            .into(),
    ));
    top_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    push_bd_switch_arg(&mut top_doc);
    if !tree.direct.is_empty() {
        top_doc.push(Item::Blank);
        for p in &tree.direct {
            emit_arg_decl(
                &mut top_doc,
                component,
                presets,
                p,
                opts,
                "",
                &ip_name,
                dict_schemas,
            );
        }
    }
    // Family kwargs on the top proc: one `-<stem_lower><i>` per
    // family member, typed as the newtype, with a doc comment
    // referencing the constructor via `[…]` for semantic-goto.
    //
    // The `@default("")` is a placeholder — the analyzer's
    // `@default(...)` grammar rejects bracket-expressions like
    // `[<T>::empty]`, so we can't declare the semantically-correct
    // default in the annotation. The body's `__vw_kw_<arg>_set`
    // guard skips the merge loop when the caller didn't pass the
    // slot, so `$<arg>` is never dereferenced with the placeholder
    // value — the empty string never reaches the newtype machinery.
    if !families.is_empty() {
        top_doc.push(Item::Blank);
        for f in &families {
            let ctor = format!("{ip_name}::{}", lowercase_ident(&f.stem));
            let ty = stem_props_name(&ip_name, &f.stem);
            for i in &f.indices {
                let arg = format!("{}{i}", lowercase_ident(&f.stem));
                top_doc.push(Item::DocComment(format!(
                    "Configuration for {} slot {i}. Construct with [{ctor}].",
                    f.stem
                )));
                top_doc.push(Item::Command(Command {
                    doc_comments: Vec::new(),
                    words: vec![
                        Word::Raw("@default(\"\")".into()),
                        Word::Bare(format!("{arg}: {ty}")),
                    ],
                    body: None,
                }));
            }
        }
    }
    // Family newtype preludes ship at the FILE TOP LEVEL — inside
    // `namespace eval <ip>` blocks, the analyzer double-prefixes
    // qualified proc names (a proc `<ip>::T::from` inside `namespace
    // eval <ip>` becomes `<ip>::<ip>::T::from`, so external
    // references never find it). Keeping them at top level with
    // fully-qualified names avoids that and remains legal now that
    // the validator accepts qualified newtype names (see Slice 4's
    // changes to `validate::reject_nested_qualified`). An empty
    // `namespace eval <ip>::<StemProps> {}` prelude keeps Tcl happy.
    if !families.is_empty() {
        for f in &families {
            writeln!(out).unwrap();
            emit_family_prelude(&mut out, &ip_name, f);
        }
        writeln!(out).unwrap();
    }

    // One value-constructor per non-root node that has direct
    // parameters. Each is a pure function: takes typed field
    // kwargs, returns a `<ip>::<NodeProps>` newtype value that the
    // top proc composes into its atomic `set_property -dict` call.
    //
    // `emit_nodes` already excludes labels collapsed into a
    // family — those emit through `emit_family_constructor` above.
    let split_nodes: Vec<&Node<'_>> = emit_nodes
        .iter()
        .filter(|n| !n.label.is_empty())
        .copied()
        .collect();

    // Newtype preludes for each split-shape constructor land at
    // file top level (same reason as families — `namespace eval
    // <ip>` blocks double-prefix qualified proc names).
    for n in &split_nodes {
        writeln!(out).unwrap();
        emit_split_props_prelude(&mut out, &ip_name, &n.label);
    }
    if !split_nodes.is_empty() {
        writeln!(out).unwrap();
    }

    // Top-proc kwargs: one per split-shape constructor. Each is
    // typed as the node's newtype so the top proc composes ALL
    // configuration atomically. Semantic-ref doc comment points
    // at the constructor for goto/hover.
    if !split_nodes.is_empty() {
        top_doc.push(Item::Blank);
        for n in &split_nodes {
            let ctor_suffix = sanitize_ident(&n.label.to_ascii_lowercase());
            let ctor = format!("{ip_name}::{ctor_suffix}");
            let ty = split_props_name(&ip_name, &n.label);
            top_doc.push(Item::DocComment(format!(
                "Configuration for the {} sub-tree. Construct with \
                 [{ctor}].",
                n.label
            )));
            top_doc.push(Item::Command(Command {
                doc_comments: Vec::new(),
                words: vec![
                    Word::Raw("@default(\"\")".into()),
                    Word::Bare(format!("{ctor_suffix}: {ty}")),
                ],
                body: None,
            }));
        }
        // Re-emit the top proc with the updated top_doc (we
        // rebuild top_body below to inject the merge loops).
    }

    // Top-proc body: rebuild to fold in the split-shape merge
    // loops before the finalization. Structure:
    //   (create_bd_cell / create_ip)
    //   set _vw_d [list]
    //   … top-level knob loads …
    //   … family merge loops …
    //   … split-shape merge loops (NEW) …
    //   if {llength > 0} { set_property -dict $_vw_d -objects … }
    //   return $cell / $name
    let mut top_body = String::new();
    writeln!(top_body, "if {{$bd}} {{").unwrap();
    writeln!(
        top_body,
        "  set cell [vivado_cmd::create_bd_cell -type ip -vlnv {vlnv} -name $name]"
    )
    .unwrap();
    writeln!(top_body, "}} else {{").unwrap();
    writeln!(
        top_body,
        "  set cell [vivado_cmd::create_ip -vlnv {vlnv} -module_name $name]"
    )
    .unwrap();
    writeln!(top_body, "}}").unwrap();
    if !tree.direct.is_empty()
        || !families.is_empty()
        || !split_nodes.is_empty()
    {
        write_set_property_dict_with_splits(
            &mut top_body,
            &tree.direct,
            &family_merges,
            &dict_schema_newtypes,
            &split_nodes,
            &ip_name,
        );
    }
    writeln!(
        top_body,
        "if {{$bd}} {{ return $cell }} else {{ return $name }}"
    )
    .unwrap();
    // Assemble the `namespace eval <ip> { … }` body in
    // families → splits → create order.
    let mut procs = String::new();
    for (i, f) in families.iter().enumerate() {
        if i > 0 {
            writeln!(procs).unwrap();
        }
        emit_family_constructor(
            &mut procs, &ip_name, component, presets, opts, f,
        );
    }
    if !families.is_empty() {
        writeln!(procs).unwrap();
    }
    for (i, n) in split_nodes.iter().enumerate() {
        if !families.is_empty() || i > 0 {
            writeln!(procs).unwrap();
        }
        emit_split_node_constructor(
            &mut procs, &ip_name, component, presets, opts, n,
        );
    }
    if !families.is_empty() || !split_nodes.is_empty() {
        writeln!(procs).unwrap();
    }
    emit_proc(&mut procs, "create", &top_doc, Some("bd_cell"), &top_body);

    write_namespace_block(&mut out, &ip_name, &procs);
    out
}

/// Emit the newtype prelude for a split-shape node. Same shape as
/// [`emit_family_prelude`] / [`emit_dict_props_prelude`] — one
/// `namespace eval <T> {}` + `type <T> = Properties` + the four
/// helper procs. Consumed by [`emit_split_node_constructor`] and
/// the top-proc merge loop in
/// [`write_set_property_dict_with_splits`].
fn emit_split_props_prelude(out: &mut String, ip_name: &str, label: &str) {
    let qualified = split_props_name(ip_name, label);
    let ctor_lower = label.to_ascii_lowercase();
    writeln!(
        out,
        "## Typed configuration value for [{ip_name}::create]'s \
         `-{ctor_lower}` slot. Construct with [{ip_name}::{ctor_lower}].",
    )
    .unwrap();
    writeln!(out, "namespace eval {ip_name} {{}}").unwrap();
    writeln!(out, "namespace eval {qualified} {{}}").unwrap();
    writeln!(out, "type {qualified} = Properties").unwrap();
    writeln!(
        out,
        "proc {qualified}::repr {{ v: {qualified} }} string \
         {{ return [Properties::repr -v $v] }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::from {{ v: Properties }} {qualified} \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::to {{ v: {qualified} }} Properties \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {qualified}::empty {{}} {qualified} \
         {{ return [{qualified}::from -v [Properties::empty]] }}"
    )
    .unwrap();
}

/// Emit the value-constructor for a split-shape node. Bare proc
/// name (`<label_lower>`) — becomes `<ip>::<label_lower>` via the
/// enclosing `namespace eval <ip> { … }`. Pure — no cell handle,
/// no `-bd`, no `set_property`. Body builds a `Properties`-shaped
/// dict from the supplied field kwargs (index-stripped keys) and
/// wraps in the newtype.
fn emit_split_node_constructor(
    out: &mut String,
    ip_name: &str,
    component: &Component,
    presets: &crate::presets::PresetMap,
    opts: &GenerateOptions,
    n: &Node<'_>,
) {
    let ctor_local = sanitize_ident(&n.label.to_ascii_lowercase());
    let ret_ty = split_props_name(ip_name, &n.label);

    let mut doc = Doc::new();
    doc.push(Item::DocComment(format!(
        "Configuration value for [{ip_name}::create]'s \
         `-{ctor_local}` slot ({} sub-tree). Composes into the top \
         proc so every provided field lands in ONE atomic \
         `set_property -dict` call.",
        n.label
    )));
    if !n.direct.is_empty() {
        doc.push(Item::Blank);
    }
    for p in &n.direct {
        emit_arg_decl(
            &mut doc,
            component,
            presets,
            p,
            opts,
            &n.label,
            ip_name,
            &std::collections::HashMap::new(),
        );
    }

    let mut body = String::new();
    writeln!(body, "set _vw_d [dict create]").unwrap();
    for p in &n.direct {
        let arg = lowercase_ident(strip_prefix(&p.name, &n.label));
        let field_key = strip_prefix(&p.name, &n.label);
        let value_expr = if is_properties_shaped(p.value.default_value()) {
            format!("[Properties::to_raw -v ${arg}]")
        } else {
            format!("${arg}")
        };
        writeln!(
            body,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ dict set _vw_d {field_key} {value_expr} }}"
        )
        .unwrap();
    }
    writeln!(
        body,
        "return [{ret_ty}::from -v [Properties::from -v $_vw_d]]"
    )
    .unwrap();
    emit_proc(out, &ctor_local, &doc, Some(&ret_ty), &body);
}

/// Fully-qualified newtype name for a split-shape node.
/// `("dcmac", "MAC_PORT0_RX")` → `"dcmac::MacPort0RxProps"`.
fn split_props_name(ip_name: &str, label: &str) -> String {
    format!("{ip_name}::{}", split_props_local(label))
}

fn split_props_local(label: &str) -> String {
    let mut out = String::new();
    for seg in label.split('_').filter(|s| !s.is_empty()) {
        out.push_str(&pascal_case(seg));
    }
    out.push_str("Props");
    out
}

/// Extended top-proc dict writer that also merges split-shape
/// value-constructor outputs into the atomic dict. Mirrors
/// [`write_set_property_dict`]'s structure but weaves the
/// split-node merges in between the top-level knobs, family
/// merges, and the finalization.
#[allow(clippy::too_many_arguments)]
fn write_set_property_dict_with_splits(
    out: &mut String,
    parameters: &[&Parameter],
    families: &[FamilyMerge<'_>],
    dict_schema_newtypes: &std::collections::HashMap<String, String>,
    split_nodes: &[&Node<'_>],
    ip_name: &str,
) {
    writeln!(out, "set _vw_d [list]").unwrap();
    for p in parameters {
        let arg = lowercase_ident(&p.name);
        // Type-driven value unwrap:
        // - Dict-schema newtype: the constructor stores bare-string
        //   values in a paired-list dict (see
        //   [`emit_dict_sub_proc`]), which is EXACTLY what Vivado
        //   expects at `CONFIG.<PARAM>`. So just unwrap the newtype
        //   via `<T>::to` — do NOT pipe through `Properties::to_raw`,
        //   which would try to dispatch on `Property::Scalar`/
        //   `Nested` tags our stored values don't carry.
        // - Plain Properties (paired-dict-shaped default without a
        //   schema): assume the caller passed a properly-tagged
        //   Properties value; unwrap through `Properties::to_raw`.
        // - Scalar: `$arg` as-is.
        let value_expr =
            if let Some(newtype) = dict_schema_newtypes.get(&p.name) {
                format!("[{newtype}::to -v ${arg}]")
            } else if is_properties_shaped(p.value.default_value()) {
                format!("[Properties::to_raw -v ${arg}]")
            } else {
                format!("${arg}")
            };
        writeln!(
            out,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ lappend _vw_d CONFIG.{} {value_expr} }}",
            p.name
        )
        .unwrap();
    }
    // Composed-family merges.
    for fam in families {
        for idx in &fam.indices {
            let arg = format!("{}{}", fam.stem_lower, idx);
            let prefix = format!("CONFIG.{}{}_", fam.stem, idx);
            writeln!(out, "if {{${{__vw_kw_{arg}_set}}}} {{").unwrap();
            writeln!(
                out,
                "  foreach {{_vw_f _vw_v}} [{}::to -v ${arg}] {{",
                fam.newtype_qualified
            )
            .unwrap();
            writeln!(out, "    lappend _vw_d \"{prefix}$_vw_f\" $_vw_v")
                .unwrap();
            writeln!(out, "  }}").unwrap();
            writeln!(out, "}}").unwrap();
        }
    }
    // Split-shape merges — each provided value-constructor result
    // gets its dict entries flattened into the atomic dict with
    // the `CONFIG.<node.label>_` prefix so property names round-
    // trip to Vivado exactly as they were in IP-XACT.
    for n in split_nodes {
        let arg = sanitize_ident(&n.label.to_ascii_lowercase());
        let prefix = format!("CONFIG.{}_", n.label);
        let newtype = split_props_name(ip_name, &n.label);
        writeln!(out, "if {{${{__vw_kw_{arg}_set}}}} {{").unwrap();
        writeln!(
            out,
            "  foreach {{_vw_f _vw_v}} [{newtype}::to -v ${arg}] {{"
        )
        .unwrap();
        writeln!(out, "    lappend _vw_d \"{prefix}$_vw_f\" $_vw_v").unwrap();
        writeln!(out, "  }}").unwrap();
        writeln!(out, "}}").unwrap();
    }
    // Finalization — one atomic `set_property -dict` call.
    writeln!(out, "if {{[llength $_vw_d] > 0}} {{").unwrap();
    writeln!(out, "  if {{$bd}} {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_vw_d -objects $cell"
    )
    .unwrap();
    writeln!(out, "  }} else {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_vw_d -objects [get_ips $name]"
    )
    .unwrap();
    writeln!(out, "  }}").unwrap();
    writeln!(out, "}}").unwrap();
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

fn emit_file_header(out: &mut String, component: &Component, vlnv: &str) {
    // Pull in the whole `vivado-cmd` library — the body uses
    // `vivado_cmd::create_bd_cell` and `vivado_cmd::set_property`
    // alongside `ip::check`, so `src @vivado-cmd/ip` (just the
    // ip sub-module) leaves those references unresolved. The
    // analyzer reports the unbound calls; sourcing the full
    // package brings everything the emitted body actually uses
    // into scope.
    writeln!(out, "src @vivado-cmd").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "ip::check -name \"{vlnv}\"").unwrap();
    writeln!(out).unwrap();
    if let Some(desc) =
        component.description.as_deref().filter(|s| !s.is_empty())
    {
        // Split the IP-XACT description into a one-sentence summary
        // plus body so an LSP client can show a short blurb on hover
        // / completion without repeating it in the documentation
        // popup. Same shape as the cmd-doc generator.
        let raw: Vec<String> =
            desc.lines().map(|l| l.trim_end().to_string()).collect();
        let summary = vw_htcl::doc::brief(&raw);
        let extended = vw_htcl::doc::extended(&raw);
        if let Some(s) = summary {
            for line in vw_htcl::doc::wrap_paragraph(&s, 78) {
                writeln!(out, "## {line}").unwrap();
            }
        }
        if let Some(body) = extended {
            for paragraph in body.split("\n\n") {
                writeln!(out, "##").unwrap();
                for line in vw_htcl::doc::wrap_paragraph(paragraph, 78) {
                    writeln!(out, "## {line}").unwrap();
                }
            }
        }
        writeln!(out, "##").unwrap();
    }
    writeln!(out, "## Source IP-XACT: {vlnv}").unwrap();
}

/// Emit `proc <name> { <args> } <type>? { <body> }` with the args
/// and body indented two spaces each. When `return_type` is Some,
/// emits it as the 4th htcl word between args and body.
fn emit_proc(
    out: &mut String,
    name: &str,
    args: &Doc,
    return_type: Option<&str>,
    body: &str,
) {
    let args_text = args.to_string();
    writeln!(out, "proc {name} {{").unwrap();
    for line in args_text.lines() {
        if line.is_empty() {
            writeln!(out).unwrap();
        } else {
            writeln!(out, "  {line}").unwrap();
        }
    }
    match return_type {
        Some(ty) => {
            let needs_brace = ty.chars().any(char::is_whitespace);
            if needs_brace {
                writeln!(out, "}} {{{ty}}} {{").unwrap();
            } else {
                writeln!(out, "}} {ty} {{").unwrap();
            }
        }
        None => {
            writeln!(out, "}} {{").unwrap();
        }
    }
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
/// IP-XACT name; the `CONFIG.<NAME>` key keeps the full name Vivado
/// expects. Only the top-level `<ip>::create` proc calls this today —
/// sub-procs became pure value-constructors under Slice 6 and no
/// longer emit `set_property` themselves.
fn write_set_property_dict(
    out: &mut String,
    parameters: &[&Parameter],
    prefix_to_strip: &str,
    // Composed families to merge atomically into the same dict.
    // Empty for sub-procs; populated on the top proc when the
    // generator has collapsed indexed sibling groups into
    // `<ip>::<stem_lower>` constructors + `-<stem_lower><i>`
    // kwargs. Each family's per-index kwarg is unwrapped through
    // its newtype's `::to` + `Properties::to_raw` and merged into
    // `_vw_d` with `CONFIG.<STEM><i>_<FIELD>` keys.
    families: &[FamilyMerge<'_>],
    // Dict-schema newtype names, keyed by IP-XACT param name
    // (e.g. `PS_PMC_CONFIG` → `versal_cips::PsPmcConfig`). When a
    // parameter here matches a top-level top-proc param, its
    // value_expr gets the `[<T>::to -v $arg]` unwrap injected
    // BEFORE the `Properties::to_raw` step so the type-check
    // passes at compile time. Empty for sub-procs / single-shape
    // IPs without any schemas.
    dict_schema_newtypes: &std::collections::HashMap<String, String>,
) {
    // Build the dict conditionally so only user-supplied args reach
    // Vivado. See `emit_dict_proc` for the rationale — unconditionally
    // setting all CONFIG.* properties re-validates the whole cell and
    // Vivado rejects values whose declared defaults happen to be
    // out-of-range for the cell's current state. The
    // `__vw_kw_<arg>_set` flag is set by `::vw::kwargs` (shim helper)
    // only when the user passed a value for that arg.
    writeln!(out, "set _vw_d [list]").unwrap();
    for p in parameters {
        let arg = lowercase_ident(strip_prefix(&p.name, prefix_to_strip));
        // Type-driven value unwrap:
        // - Dict-schema newtype (`versal_cips::PsPmcConfig` etc.):
        //   `[Properties::to_raw -v [<T>::to -v $arg]]`. The extra
        //   `<T>::to` step satisfies the type-checker at compile
        //   time; at runtime it's identity on the underlying
        //   Properties value.
        // - Plain Properties (paired-dict-shaped default without
        //   a registered schema): `[Properties::to_raw -v $arg]`.
        // - Scalar: `$arg`.
        // Type-driven value unwrap:
        // - Dict-schema newtype: the constructor stores bare-string
        //   values in a paired-list dict (see
        //   [`emit_dict_sub_proc`]), which is EXACTLY what Vivado
        //   expects at `CONFIG.<PARAM>`. So just unwrap the newtype
        //   via `<T>::to` — do NOT pipe through `Properties::to_raw`,
        //   which would try to dispatch on `Property::Scalar`/
        //   `Nested` tags our stored values don't carry.
        // - Plain Properties (paired-dict-shaped default without a
        //   schema): assume the caller passed a properly-tagged
        //   Properties value; unwrap through `Properties::to_raw`.
        // - Scalar: `$arg` as-is.
        let value_expr =
            if let Some(newtype) = dict_schema_newtypes.get(&p.name) {
                format!("[{newtype}::to -v ${arg}]")
            } else if is_properties_shaped(p.value.default_value()) {
                format!("[Properties::to_raw -v ${arg}]")
            } else {
                format!("${arg}")
            };
        writeln!(
            out,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ lappend _vw_d CONFIG.{} {value_expr} }}",
            p.name
        )
        .unwrap();
    }
    // Composed-family merges — each provided `-<stem_lower><i>`
    // value gets unwrapped and its `FIELD → value` pairs merged
    // into `_vw_d` with the `CONFIG.<STEM><i>_` prefix. Runs
    // BEFORE the `if {llength} { set_property … }` finalization
    // so the whole thing lands as ONE atomic call.
    for fam in families {
        for idx in &fam.indices {
            let arg = format!("{}{}", fam.stem_lower, idx);
            let prefix = format!("CONFIG.{}{}_", fam.stem, idx);
            // Family constructors store bare-string values in
            // their dict (see `emit_family_constructor`), so
            // iterate the raw dict directly rather than going
            // through `Properties::to_raw`. The latter would
            // dispatch on `Property::Scalar`/`Nested` tags that
            // our stored values don't carry — the constructor
            // treats every field as a scalar and this loop has
            // to match that convention.
            writeln!(out, "if {{${{__vw_kw_{arg}_set}}}} {{").unwrap();
            writeln!(
                out,
                "  foreach {{_vw_f _vw_v}} [{}::to -v ${arg}] {{",
                fam.newtype_qualified
            )
            .unwrap();
            writeln!(out, "    lappend _vw_d \"{prefix}$_vw_f\" $_vw_v")
                .unwrap();
            writeln!(out, "  }}").unwrap();
            writeln!(out, "}}").unwrap();
        }
    }
    // `-bd 1` → cell handle is a bd_cell path, set_property targets
    // it directly. `-bd 0` → cell handle came from `create_ip`,
    // which returns an XCI file path (not a usable IP object). We
    // resolve the IP through `get_ips` keyed on the top proc's
    // `$name` arg. (Sub-procs no longer emit `set_property`, so
    // the historical `-cell`-keyed variant is gone.)
    let ip_ref = "[get_ips $name]";
    writeln!(out, "if {{[llength $_vw_d] > 0}} {{").unwrap();
    writeln!(out, "  if {{$bd}} {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_vw_d -objects $cell"
    )
    .unwrap();
    writeln!(out, "  }} else {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_vw_d -objects {ip_ref}"
    )
    .unwrap();
    writeln!(out, "  }}").unwrap();
    writeln!(out, "}}").unwrap();
}

// ---------------------------------------------------------------------------
// Indexed-family emission.
// ---------------------------------------------------------------------------

/// Emit one arg-decl for a family constructor. Mirror of
/// [`emit_arg_decl`] with one addition: the `@enum(...)` list is
/// the **union** of enum choices across every provided member
/// (`per_member`), so the collapsed constructor accepts any value
/// any port takes. Defaults are shape-equal by construction (the
/// shape-match check requires it), so we use `shape_param`'s
/// default verbatim.
fn emit_arg_decl_family(
    doc: &mut Doc,
    component: &Component,
    presets: &crate::presets::PresetMap,
    shape_param: &Parameter,
    per_member: &[&Parameter],
    shape_member_label: &str,
    opts: &GenerateOptions,
) {
    if opts.include_descriptions {
        if let Some(desc) =
            shape_param.description.as_deref().filter(|s| !s.is_empty())
        {
            for line in desc.lines() {
                doc.push(Item::DocComment(line.trim_end().into()));
            }
        }
    }
    let mut words = Vec::new();
    // Union enum choices across every per-member Parameter that
    // maps to this field. Preserve insertion order (IP-XACT first
    // across members, presets after) so hover/completion shows a
    // stable list.
    let mut seen = std::collections::HashSet::new();
    let mut unioned: Vec<String> = Vec::new();
    for p in per_member {
        for v in enum_values_for(component, presets, p) {
            if seen.insert(v.clone()) {
                unioned.push(v);
            }
        }
    }
    if !unioned.is_empty() {
        let formatted: Vec<String> =
            unioned.iter().map(|v| format_attribute_value(v)).collect();
        words.push(Word::Raw(format!("@enum({})", formatted.join(", "))));
    }
    let default = shape_param.value.default_value();
    if !default.is_empty() {
        words.push(Word::Raw(format!(
            "@default({})",
            format_attribute_value(default)
        )));
    }
    let lowered =
        lowercase_ident(strip_prefix(&shape_param.name, shape_member_label));
    let typed_name = if is_properties_shaped(default) {
        format!("{lowered}: Properties")
    } else {
        lowered
    };
    words.push(Word::Bare(typed_name));
    doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words,
        body: None,
    }));
}

/// Emit the newtype declaration and its `from`/`to`/`repr`/`empty`
/// helper procs for one family. Called from inside a
/// `namespace eval <ip> { … }` block, so identifiers use their
/// TOP-LEVEL forms — this fn is called at file top level, NOT
/// inside `namespace eval <ip> { … }`. Reason: the analyzer
/// double-prefixes qualified proc names declared inside a matching
/// namespace-eval block (`proc <ip>::T::from` inside
/// `namespace eval <ip> { … }` becomes `<ip>::<ip>::T::from`), so
/// external references never resolve. Keeping the newtype block at
/// top level with fully-qualified names avoids that. Legal thanks
/// to this slice's `validate::reject_nested_qualified` update.
fn emit_family_prelude(out: &mut String, ip_name: &str, f: &IndexedFamily<'_>) {
    let stem_props = stem_props_name(ip_name, &f.stem);
    let stem_lower = lowercase_ident(&f.stem);
    writeln!(
        out,
        "## Typed configuration slot for one [{ip_name}::{stem_lower}] on \
         [{ip_name}::create].",
    )
    .unwrap();
    // Tcl requires the target namespace to exist before qualified
    // proc names like `<ip>::<StemProps>::from` are legal. Nested
    // `namespace eval` calls establish the whole chain. The outer
    // `namespace eval <ip> {}` shape is idempotent — the proc-block
    // emitted below re-enters the same namespace.
    writeln!(out, "namespace eval {ip_name} {{}}").unwrap();
    writeln!(out, "namespace eval {stem_props} {{}}").unwrap();
    writeln!(out, "type {stem_props} = Properties").unwrap();
    writeln!(
        out,
        "proc {stem_props}::repr {{ v: {stem_props} }} string \
         {{ return [Properties::repr -v $v] }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {stem_props}::from {{ v: Properties }} {stem_props} \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {stem_props}::to {{ v: {stem_props} }} Properties \
         {{ return $v }}"
    )
    .unwrap();
    writeln!(
        out,
        "proc {stem_props}::empty {{}} {stem_props} \
         {{ return [{stem_props}::from -v [Properties::empty]] }}"
    )
    .unwrap();
}

/// Emit the family constructor `<ip>::<stem_lower>` — a pure
/// value-builder that takes the same typed args the collapsed
/// per-N procs took, packs the supplied ones into a `Properties`
/// dict, and returns it wrapped in the newtype. NO Vivado side
/// effects; the atomic materialization happens later in
/// `<ip>::create`.
fn emit_family_constructor(
    out: &mut String,
    ip_name: &str,
    component: &Component,
    presets: &crate::presets::PresetMap,
    opts: &GenerateOptions,
    f: &IndexedFamily<'_>,
) {
    // Emitted inside `namespace eval <ip> { … }` — use a bare
    // proc name; Tcl resolves it to `<ip>::<stem_lower>` at load
    // time via the enclosing namespace.
    let stem_lower = lowercase_ident(&f.stem);
    let ret_ty = stem_props_name(ip_name, &f.stem);

    let mut doc = Doc::new();
    doc.push(Item::DocComment(format!(
        "Configuration value for one of [{ip_name}::create]'s \
         `-{stem_lower}<i>` slots. Composes into the top proc so \
         every provided slot's fields land in ONE atomic \
         `set_property -dict` call.",
    )));
    doc.push(Item::Blank);
    // Each field's `@enum(...)` values are the UNION of that
    // field's per-member enum choices — the constructor has to
    // accept any value any port can take. See ShapeSlot's docs
    // for why choice_ref differences don't block the family.
    for p in &f.shape {
        let field_short = strip_prefix(&p.name, &f.shape_member_label);
        let per_member: Vec<&Parameter> = f
            .members_direct
            .iter()
            .zip(&f.member_labels)
            .filter_map(|(direct, label)| {
                direct
                    .iter()
                    .copied()
                    .find(|q| strip_prefix(&q.name, label) == field_short)
            })
            .collect();
        emit_arg_decl_family(
            &mut doc,
            component,
            presets,
            p,
            &per_member,
            &f.shape_member_label,
            opts,
        );
    }

    // Body: build a dict from the supplied kwargs and wrap it.
    let mut body = String::new();
    writeln!(body, "set _vw_d [dict create]").unwrap();
    for p in &f.shape {
        let arg = lowercase_ident(strip_prefix(&p.name, &f.shape_member_label));
        // Post-strip key becomes the CONFIG.<STEM><i>_ suffix at
        // top-proc merge time. Store un-prefixed field name here.
        let field_key = strip_prefix(&p.name, &f.shape_member_label);
        // Properties-typed sub-slots (rare in a family shape;
        // included for completeness) get unwrapped through
        // Properties::to_raw before landing in the dict. Otherwise
        // the value is a plain string.
        let value_expr = if is_properties_shaped(p.value.default_value()) {
            format!("[Properties::to_raw -v ${arg}]")
        } else {
            format!("${arg}")
        };
        writeln!(
            body,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ dict set _vw_d {field_key} {value_expr} }}"
        )
        .unwrap();
    }
    writeln!(
        body,
        "return [{ret_ty}::from -v [Properties::from -v $_vw_d]]"
    )
    .unwrap();
    emit_proc(out, &stem_lower, &doc, Some(&ret_ty), &body);
}

/// Reconstruct a family member's tree-node label from stem + index.
/// The `<STEM><INDEX>` naming (no separator) matches how
/// `tree::build_tree` produces `MAC_PORT0`, `MAC_PORT1`, ….
fn stem_index_label(stem: &str, idx: u32) -> String {
    format!("{stem}{idx}")
}

/// Fully-qualified newtype name from IP-name + uppercase-with-
/// underscores stem. `("dcmac", "MAC_PORT")` → `"dcmac::MacPortProps"`.
///
/// Namespaced under the IP so `dcmac::` completion surfaces the
/// type alongside the constructor and top proc. Requires
/// vw-htcl's validator to accept qualified newtype names (landed
/// as part of this slice — see `validate::reject_nested_qualified`).
fn stem_props_name(ip_name: &str, stem: &str) -> String {
    format!("{ip_name}::{}", stem_props_local(stem))
}

/// Local (unqualified) newtype segment — the PascalCase stem +
/// `Props`. Used inside `namespace eval <ip> { … }` where bare
/// names are preferred; the outer emission builds the qualified
/// form via [`stem_props_name`].
fn stem_props_local(stem: &str) -> String {
    let mut out = String::new();
    for seg in stem.split('_').filter(|s| !s.is_empty()) {
        out.push_str(&pascal_case(seg));
    }
    out.push_str("Props");
    out
}

/// Fully-qualified newtype name for a dict-schema parameter's
/// composed value. `("versal_cips", "PS_PMC_CONFIG")` →
/// `"versal_cips::PsPmcConfig"`. Same compositional-value pattern
/// as the family-based [`stem_props_name`], applied to Xilinx's
/// `structured_tcldict` parameter surface so top-proc kwargs like
/// `-ps_pmc_config` get a typed newtype instead of raw Properties.
fn dict_props_name(ip_name: &str, param_name: &str) -> String {
    format!("{ip_name}::{}", dict_props_local(param_name))
}

/// Local (unqualified) form of the dict-schema newtype name —
/// PascalCase of the param name.
fn dict_props_local(param_name: &str) -> String {
    let mut out = String::new();
    for seg in param_name.split('_').filter(|s| !s.is_empty()) {
        out.push_str(&pascal_case(seg));
    }
    out
}

/// Build the `param_name → qualified_newtype_name` map that
/// [`write_set_property_dict`] consults to inject the `<T>::to`
/// unwrap step. Empty when the IP has no dict schemas.
fn build_dict_schema_newtypes(
    ip_name: &str,
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
) -> std::collections::HashMap<String, String> {
    dict_schemas
        .keys()
        .map(|k| (k.clone(), dict_props_name(ip_name, k)))
        .collect()
}

/// Convert an identifier segment to PascalCase — first char upper,
/// rest lower. Non-ASCII passes through unchanged.
fn pascal_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    if let Some(c) = chars.next() {
        out.push(c.to_ascii_uppercase());
        for c in chars {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Bundle of names needed to emit one family's per-index merge
/// block inside [`write_set_property_dict`]. Built at the top-proc
/// call site from an [`IndexedFamily`] + the IP's namespace.
struct FamilyMerge<'a> {
    /// Uppercase stem (`"MAC_PORT"`).
    stem: String,
    /// Lowercase-ident stem for arg names (`"mac_port"`).
    stem_lower: String,
    /// Present indices (`[0, 1, 2, 3, 4, 5]`).
    indices: Vec<u32>,
    /// Fully-qualified newtype path for the `::to` unwrap
    /// (`"dcmac::MacPortProps"`).
    newtype_qualified: String,
    #[allow(dead_code)]
    marker: std::marker::PhantomData<&'a ()>,
}

#[allow(clippy::too_many_arguments)]
fn emit_arg_decl(
    doc: &mut Doc,
    component: &Component,
    presets: &crate::presets::PresetMap,
    p: &Parameter,
    opts: &GenerateOptions,
    prefix_to_strip: &str,
    ip_name: &str,
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
) {
    if opts.include_descriptions {
        if let Some(desc) = p.description.as_deref().filter(|s| !s.is_empty()) {
            for line in desc.lines() {
                doc.push(Item::DocComment(line.trim_end().into()));
            }
        }
    }
    // Dict-schema-backed params get their composed newtype form
    // instead of raw Properties. The IP-XACT default (a paired-list
    // string) is unrepresentable as a newtype value, so we omit the
    // `@default(...)` — the top-proc body's `__vw_kw_<arg>_set`
    // guard skips unset args, and callers explicitly compose the
    // slot via the constructor when they want to override.
    let dict_schema = dict_schemas.get(&p.name);
    if let Some(_schema) = dict_schema {
        let ctor = format!("{ip_name}::{}", p.name.to_ascii_lowercase());
        doc.push(Item::DocComment(format!("Composed via [{ctor}].")));
    }
    let mut words = Vec::new();
    if dict_schema.is_none() {
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
    } else {
        // Empty-string placeholder default. The runtime guard
        // (`__vw_kw_<arg>_set`) prevents the placeholder from ever
        // being dereferenced through the newtype machinery.
        words.push(Word::Raw("@default(\"\")".into()));
    }
    let lowered = lowercase_ident(strip_prefix(&p.name, prefix_to_strip));
    // Typing rules for the param:
    // - Dict-schema-backed: use its typed newtype
    //   (`versal_cips::PsPmcConfig`) so `dcmac::` / `versal_cips::`
    //   completion surfaces the type alongside the constructor,
    //   and misuse (passing e.g. `CpmConfig` to `-ps_pmc_config`)
    //   is a compile-time error.
    // - Otherwise Properties-shaped (paired-dict default without a
    //   registered schema): keep raw `Properties` — callers still
    //   compose these by hand, and the top-proc body unwraps via
    //   `Properties::to_raw` at the `set_property -dict` boundary
    //   (see [`write_set_property_dict`]).
    // - Plain scalar: no type annotation.
    let default = p.value.default_value();
    let typed_name = if dict_schema.is_some() {
        format!("{lowered}: {}", dict_props_name(ip_name, &p.name))
    } else if is_properties_shaped(default) {
        format!("{lowered}: Properties")
    } else {
        lowered
    };
    words.push(Word::Bare(typed_name));
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

/// True when the default value parses as a paired-dict Tcl-list
/// shape — i.e. has an even number of whitespace-separated tokens
/// (≥ 2) with identifier-shaped keys at every even index. Used by
/// [`emit_arg_decl`] / [`write_set_property_dict`] to decide which
/// wrapper args get the typed `Properties` annotation + automatic
/// `Properties::to_raw` unwrap at the extern boundary.
///
/// Vivado IP-XACT defaults for CONFIG.* dict slots typically look
/// like `"KEY1 VAL1 KEY2 VAL2"` (e.g. `CPM_PCIE0_MODES None`,
/// `SMON_ALARMS Set_Alarms_On SMON_ENABLE_TEMP_AVERAGING 0`),
/// while scalar params look like `Custom` or `0` or
/// `versal_cips_v3_4`. Two-pair-shaped strings whose keys happen
/// to be bare-identifier-shaped slip through as Properties even
/// when the IP author meant them as a scalar — unlikely enough
/// to be acceptable noise; the wrapper still works when the
/// caller passes a string-shaped raw value (it round-trips through
/// `Properties::to_raw` returning the same paired list).
fn is_properties_shaped(default: &str) -> bool {
    let tokens: Vec<&str> = default.split_whitespace().collect();
    if tokens.len() < 2 || !tokens.len().is_multiple_of(2) {
        return false;
    }
    for (i, t) in tokens.iter().enumerate() {
        if i % 2 != 0 {
            continue;
        }
        let mut chars = t.chars();
        let first = match chars.next() {
            Some(c) => c,
            None => return false,
        };
        if !first.is_ascii_alphabetic() && first != '_' {
            return false;
        }
        for c in chars {
            if !(c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                return false;
            }
        }
    }
    true
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
        // Procs live inside `namespace eval <ip> { … }` and get
        // the 2-space indent from `write_namespace_block`.
        let n_procs = out.matches("\n  proc ").count();
        assert_eq!(n_procs, 1, "{out}");
        assert!(out.contains("proc create"));
        assert!(out.contains("namespace eval demo {"));
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
        assert!(out.contains("proc create"));
        assert!(out.contains("proc big_one"));
        assert!(out.contains("proc big_two"));
        let parsed = vw_htcl::parse(&out);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let diags = vw_htcl::validate(&parsed.document, &out);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == vw_htcl::Severity::Error)
            // The generator emits calls into vivado-cmd
            // (`ip::check`, `create_bd_cell`, `set_property`);
            // those resolve when the wrapper is sourced through
            // the loader, but this unit test runs the validator
            // on the bare generated text. The unknown-call
            // diagnostic is *expected* in that mode; we filter it
            // out so the test catches real structural breakage.
            .filter(|d| !d.message.starts_with("undefined proc"))
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
        // Sub-procs are pure value-constructors — no `cell:`
        // first arg, no bd switch — and return the node's typed
        // newtype instead of `bd_cell`. Assert the shape.
        assert!(
            out.contains("proc big_one {\n    ## Configuration value"),
            "{out}"
        );
        assert!(out.contains("} wide::BigOneProps {"), "{out}");
        // The old `cell: bd_cell` mutator arg must NOT appear.
        assert!(!out.contains("cell: bd_cell"), "{out}");
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
        for name in ["proc tiny_a ", "proc tiny_b ", "proc stray "] {
            assert!(!out.contains(name), "unexpected {name} in:\n{out}");
        }
        // ...and the params instead appear as args on the top proc
        // (`create`). Slice the create-proc range so we can search
        // its args without accidentally matching arg names embedded
        // in one of the value-constructor sub-procs' bodies.
        let create_start = out.find("proc create {").expect("no proc create");
        let create_body = &out[create_start..];
        let create_end = create_body
            .find("\n  }\n")
            .map(|e| e + 5)
            .unwrap_or(create_body.len());
        let create_range = &create_body[..create_end];
        for arg in ["tiny_a_one", "tiny_b_one", "tiny_c_one", "stray_thing"] {
            assert!(
                create_range.contains(arg),
                "{arg} missing from create proc: {create_range}"
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
        // The constructor stores the index-stripped key in its
        // Properties dict…
        assert!(out.contains("dict set _vw_d FIELD0 $field0"), "{out}");
        // …and the top proc's merge loop prefixes with
        // `CONFIG.GROUP_A_` when composing atomically. The literal
        // format uses `$_vw_f` at runtime, so we assert on the
        // prefix pattern.
        assert!(out.contains("CONFIG.GROUP_A_$_vw_f"), "{out}");
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
    fn bd_switch_arg_toggles_construction() {
        // Every generated wrapper — single-shape or split-shape —
        // should carry the `-bd` arg and a Tcl `if {$bd}` block
        // that picks between `create_bd_cell` (default) and
        // `create_ip`. Regression test for the omission that had
        // wrappers only supporting the block-design path.
        for out in [
            generate(
                &mk_component(),
                &Default::default(),
                &::std::collections::HashMap::new(),
                &GenerateOptions::default(),
            ),
            generate(
                &mk_split_component(6),
                &Default::default(),
                &::std::collections::HashMap::new(),
                &GenerateOptions {
                    split_threshold: 5,
                    ..GenerateOptions::default()
                },
            ),
        ] {
            assert!(out.contains("@enum(0, 1) @default(0) bd"), "{out}");
            assert!(out.contains("if {$bd} {"), "{out}");
            assert!(out.contains("create_bd_cell"), "{out}");
            assert!(out.contains("create_ip -vlnv"), "{out}");
            let parsed = vw_htcl::parse(&out);
            assert!(
                parsed.errors.is_empty(),
                "wrapped output should parse cleanly: {:?}",
                parsed.errors
            );
        }
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
