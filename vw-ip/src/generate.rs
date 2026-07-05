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
    /// Per-IP TOML overrides file. Attaches `@enum(…)` refinements
    /// and per-field default overrides to XML-derived DictSchemas
    /// (see [`crate::overrides::OverridesFile`]). Empty when no
    /// override file is present — the generator falls back to
    /// XML-only defaults.
    pub overrides: crate::overrides::OverridesFile,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            include_descriptions: true,
            user_configurable_only: true,
            split_threshold: 100,
            min_split_size: 8,
            no_collapse: Vec::new(),
            overrides: crate::overrides::OverridesFile::default(),
        }
    }
}

/// Multi-file generation output.
///
/// Big IPs (gtwiz-versal at 160k lines, cpm5 at 40k) blow past
/// tree-sitter's default incremental-parse budget when squeezed
/// into one file. Splitting per split-node keeps every file under
/// ~20k lines — still large, but within reach of downstream tools.
///
/// `main` is the primary `module.htcl` content, which sources every
/// entry in `subfiles` via `src ./<basename>` lines. Each subfile
/// contains one split-node's dict-schema newtypes + its constructor
/// proc, emitted with fully-qualified proc names so the file
/// stands alone (no namespace-block wrapping needed).
#[derive(Debug, Clone, Default)]
pub struct MultiFileOutput {
    pub main: String,
    /// `(basename, content)` pairs — basename is the file name
    /// (no leading `./`, no directory prefix); the CLI writes each
    /// to `<output_dir>/<basename>`.
    pub subfiles: Vec<(String, String)>,
}

impl MultiFileOutput {
    /// Merge every subfile into `main` and return the concatenated
    /// text. Preserves the pre-split single-file shape so unit tests
    /// and callers that don't care about file layout can keep
    /// treating the output as one string.
    pub fn into_single(mut self) -> String {
        for (_, sub) in &self.subfiles {
            self.main.push('\n');
            self.main.push_str(sub);
        }
        self.main
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
) -> MultiFileOutput {
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
    // Only the CSV-driven top-level schemas are surfaced here.
    // XML-derived schemas for split-node params get emitted inline
    // inside `emit_split_node_constructor`, and top-level XML
    // schemas (unclaimed by split nodes) get emitted inside
    // `generate_split` / `generate_single` themselves — see the
    // dedicated merge steps there.
    if !dict_schemas.is_empty() {
        append_dict_sub_procs(&mut out, component, dict_schemas, opts);
    }
    // Peel off large split-node blocks into sibling files so
    // tree-sitter (and any other line-oriented consumer) doesn't
    // choke on the aggregate. See [`split_into_files`] for the
    // peel heuristic and file-shape contract.
    split_into_files(out)
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
        // Top-level dict-schema procs (PS_PMC_CONFIG, etc.) live
        // directly under `<ip>::` — pass an empty namespace prefix.
        // Sub-schemas emitted while recursing pick up their own
        // prefix chain from `emit_dict_sub_schemas`.
        emit_dict_props_prelude(out, &ip_name, &[], param_name);
        writeln!(out).unwrap();
        emit_dict_sub_proc(out, &ip_name, &[], param_name, schema, opts);
    }
}

/// Emit the newtype declaration + `::from`/`::to`/`::repr`/`::empty`
/// helper procs for one dict-schema param. Mirror of
/// [`emit_family_prelude`] — same shape, different naming source.
///
/// `namespace_prefix` composes intermediate namespace segments
/// between the IP name and the param name — for a nested
/// sub-constructor emitted under `<ip>::intf::gt_settings::`, pass
/// `["intf", "gt_settings"]`. Empty slice reproduces the original
/// flat `<ip>::<param>` shape used by PS_PMC_CONFIG.
fn emit_dict_props_prelude(
    out: &mut String,
    ip_name: &str,
    namespace_prefix: &[&str],
    param_name: &str,
) {
    let qualified = dict_props_name(ip_name, namespace_prefix, param_name);
    let ctor_lower = param_name.to_ascii_lowercase();
    let scope_display = if namespace_prefix.is_empty() {
        format!("[{ip_name}::create]")
    } else {
        format!("[{ip_name}::{}]", namespace_prefix.join("::"))
    };
    writeln!(
        out,
        "## Typed configuration value for {scope_display}'s \
         `-{ctor_lower}` slot. Construct with [{}::{ctor_lower}].",
        proc_scope(ip_name, namespace_prefix)
    )
    .unwrap();
    // Every ancestor namespace segment needs `namespace eval` so
    // downstream `proc <ip>::a::b::c` declarations resolve. Emit the
    // whole chain from `<ip>` down to the newtype's own namespace.
    writeln!(out, "namespace eval {ip_name} {{}}").unwrap();
    for i in 0..namespace_prefix.len() {
        let chain = std::iter::once(ip_name)
            .chain(namespace_prefix[..=i].iter().copied())
            .collect::<Vec<_>>()
            .join("::");
        writeln!(out, "namespace eval {chain} {{}}").unwrap();
    }
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

/// Compose the parent-namespace form for a nested sub-proc, without
/// the leaf. `("gtwiz_versal", ["intf", "gt_settings"])` →
/// `"gtwiz_versal::intf::gt_settings"`. Used by doc comments to
/// point at the containing scope.
fn proc_scope(ip_name: &str, namespace_prefix: &[&str]) -> String {
    if namespace_prefix.is_empty() {
        ip_name.to_string()
    } else {
        format!("{ip_name}::{}", namespace_prefix.join("::"))
    }
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
    namespace_prefix: &[&str],
    param_name: &str,
    schema: &crate::DictSchema,
    opts: &GenerateOptions,
) {
    // Recurse into sub-schemas FIRST so nested newtypes are declared
    // before the outer proc references them. The outer proc's slot
    // args carry types like `Intf0GtSettingsLr0Settings` — those
    // types must exist in the analyzer's view by the time the outer
    // proc's signature is checked, so the emission order (deepest
    // first, then parent) matches lexical declaration order.
    let sub_ctors = emit_dict_sub_schemas(
        out,
        ip_name,
        namespace_prefix,
        param_name,
        schema,
        opts,
    );

    let ctor_local = param_name.to_ascii_lowercase();
    let ctor_scope = proc_scope(ip_name, namespace_prefix);
    let ctor_name = format!("{ctor_scope}::{ctor_local}");
    let ret_ty = dict_props_name(ip_name, namespace_prefix, param_name);

    let mut doc = Doc::new();
    let scope_display = if namespace_prefix.is_empty() {
        format!("[{ip_name}::create]")
    } else {
        format!("[{}]", proc_scope(ip_name, namespace_prefix))
    };
    doc.push(Item::DocComment(format!(
        "Configuration value for {scope_display}'s \
         `-{ctor_local}` slot (`CONFIG.{param_name}`). Composes into \
         the top proc so every provided field lands in ONE atomic \
         `set_property -dict` call.",
    )));
    if !schema.fields.is_empty() || !sub_ctors.is_empty() {
        doc.push(Item::Blank);
    }
    for f in &schema.fields {
        emit_dict_field_arg(&mut doc, f, opts);
    }
    // Typed slots for nested sub-schemas — the outer proc takes
    // one arg per LRn slot (or equivalent), and the runtime merges
    // each slot's `<T>::to` unwrap into the top-level Properties
    // dict under the slot's XML key.
    for (raw_name, sub_ret_ty) in &sub_ctors {
        let arg = lowercase_ident(raw_name);
        doc.push(Item::Command(Command {
            doc_comments: Vec::new(),
            words: vec![
                Word::Raw("@default(\"\")".into()),
                Word::Bare(format!("{arg}: {sub_ret_ty}")),
            ],
            body: None,
        }));
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
    // Sub-slot merges: unwrap the typed newtype to its raw paired
    // dict via `<T>::to` + `Properties::to_raw`, and stash it under
    // the slot's original XML key so `set_property -dict` sees the
    // nested-paired-dict shape Vivado expects.
    for (raw_name, sub_ret_ty) in &sub_ctors {
        let arg = lowercase_ident(raw_name);
        writeln!(
            body,
            "if {{${{__vw_kw_{arg}_set}}}} \
             {{ dict set _vw_d {raw_name} \
                [Properties::to_raw -v [{sub_ret_ty}::to -v ${arg}]] }}",
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

/// Recursively emit the sub-constructor procs for `schema`'s nested
/// slots. Each sub-schema gets its own prelude + proc, declared in
/// a namespace one level deeper than the outer schema
/// (`<ip>::<prefix…>::<param>::<slot>`). Returns
/// `(raw_slot_name, sub_ret_ty)` pairs so the outer proc's emitter
/// can declare typed slot args referencing these newtypes.
fn emit_dict_sub_schemas(
    out: &mut String,
    ip_name: &str,
    namespace_prefix: &[&str],
    outer_param: &str,
    schema: &crate::DictSchema,
    opts: &GenerateOptions,
) -> Vec<(String, String)> {
    let mut acc = Vec::new();
    if schema.sub_schemas.is_empty() {
        return acc;
    }
    // Extend the namespace prefix with the outer param's lowercase
    // form — every sub-slot lives one level under the outer proc's
    // scope: `<prefix…>::<outer_param>::<slot>`.
    let outer_lower = outer_param.to_ascii_lowercase();
    let mut deeper: Vec<&str> = namespace_prefix.to_vec();
    deeper.push(&outer_lower);
    for (raw_name, sub_schema) in &schema.sub_schemas {
        // Empty sub-schemas (no fields + no further sub-slots)
        // arise when an anchor's inner slot is a placeholder — e.g.
        // TXRX_OPTIONAL_PORTS's `INTF_LR_SETTINGS` has LR0_SETTINGS
        // populated but LR1_SETTINGS..LR15_SETTINGS empty. Emitting
        // a proc for those would produce a body-less arg list
        // (`proc … { ## doc only }`) which the htcl parser rejects
        // as "doc comment with no following argument". Skip them —
        // the outer proc's typed slot becomes bare (no sub-slot
        // arg) and the pair simply isn't set.
        if sub_schema.fields.is_empty() && sub_schema.sub_schemas.is_empty() {
            continue;
        }
        writeln!(out).unwrap();
        emit_dict_props_prelude(out, ip_name, &deeper, raw_name);
        writeln!(out).unwrap();
        emit_dict_sub_proc(out, ip_name, &deeper, raw_name, sub_schema, opts);
        let sub_ret_ty = dict_props_name(ip_name, &deeper, raw_name);
        acc.push((raw_name.clone(), sub_ret_ty));
    }
    acc
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

    // `configure`'s arg-doc — the whole documented kwarg pile.
    // Same shape as the old inline-`create` version, minus the
    // `-name` / `-bd` slots which are now `create`-only.
    let mut configure_doc = Doc::new();
    for p in parameters {
        emit_arg_decl(
            &mut configure_doc,
            component,
            presets,
            p,
            opts,
            "",
            &ip_name,
            dict_schemas,
            &[],
        );
    }

    // `create`'s arg-doc — the narrow surface. Just the
    // instantiation-mode args + a single typed `-config`.
    let mut create_doc = Doc::new();
    create_doc.push(Item::DocComment(
        "Project-level IP module name, or (when `-bd 1`) the \
         instance name in the block design."
            .into(),
    ));
    create_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    push_bd_switch_arg(&mut create_doc);
    create_doc.push(Item::DocComment(format!(
        "Typed configuration value. Construct with [{ip_name}::configure]. \
         Defaults to an empty config (all parameters take their IP defaults)."
    )));
    create_doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words: vec![
            Word::Raw("@default(\"\")".into()),
            Word::Bare(format!("config: {ip_name}::Config")),
        ],
        body: None,
    }));

    // Config prelude ships at file TOP LEVEL — namespace-eval
    // double-prefix bug means qualified proc names like
    // `<ip>::Config::from` inside `namespace eval <ip> {…}` end
    // up as `<ip>::<ip>::Config::from`. See emit_family_prelude
    // for the same trick.
    writeln!(out).unwrap();
    emit_config_prelude(&mut out, &ip_name);
    writeln!(out).unwrap();

    let dict_schema_newtypes =
        build_dict_schema_newtypes(&ip_name, dict_schemas);
    let configure_body = build_single_configure_body(
        parameters,
        &ip_name,
        &dict_schema_newtypes,
    );
    let create_body = build_single_create_body(&vlnv, &ip_name);

    // Emit both procs inside `namespace eval <ip> { … }` so
    // `configure` and `create` register as `<ip>::configure` and
    // `<ip>::create`. Same wrap idiom as vivado-cmd's log.htcl.
    let mut procs = String::new();
    emit_proc(
        &mut procs,
        "configure",
        &configure_doc,
        Some(&config_name(&ip_name)),
        &configure_body,
    );
    writeln!(procs).unwrap();
    emit_proc(
        &mut procs,
        "create",
        &create_doc,
        Some("bd_cell"),
        &create_body,
    );
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

/// Build `<ip>::create`'s body (single shape). Just cell-creation,
/// config unwrap, `set_property` finalize, and return. All the
/// dict-assembly happens in `<ip>::configure` — see
/// [`build_single_configure_body`].
///
/// The `-bd` switch chooses between `create_bd_cell` (default
/// bd=1) for block-design usage and `create_ip` (bd=0) for
/// project-IP usage. Returns `$cell` or `$name` accordingly per
/// the two-mode contract [`emit_config_finalize`] and its callers
/// depend on.
fn build_single_create_body(vlnv: &str, ip_name: &str) -> String {
    let mut out = String::new();
    // Guard: `-config` defaults to `""` because the analyzer's
    // `@default(...)` grammar rejects bracket-expressions like
    // `[<T>::empty]`. Coerce to a real empty Config value at the
    // top of the body so downstream `<T>::to` unwrap works.
    // Family / split-node kwargs on the old shape used the same
    // placeholder pattern (see the old generate_split arg-decl
    // block for the family precedent).
    writeln!(out, "if {{$config eq \"\"}} {{").unwrap();
    writeln!(out, "  set config [{ip_name}::Config::empty]").unwrap();
    writeln!(out, "}}").unwrap();
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
    emit_config_finalize(&mut out, ip_name);
    // Return the identifier sub-procs / downstream code needs. In
    // bd mode that's `$cell` (a bd_cell path); in ip mode `$name`
    // (module name) since `$cell` is an XCI path.
    writeln!(out, "if {{$bd}} {{ return $cell }} else {{ return $name }}")
        .unwrap();
    out
}

/// Build `<ip>::configure`'s body (single shape). Pure dict
/// assembly + wrap in `<ip>::Config`. Zero side effects.
fn build_single_configure_body(
    parameters: &[&Parameter],
    ip_name: &str,
    dict_schema_newtypes: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::new();
    write_dict_assembly(&mut out, parameters, "", &[], dict_schema_newtypes);
    let config_ty = config_name(ip_name);
    // Lift the assembled `_vw_d` (flat `CONFIG.<PARAM> value` pairs
    // with bare-string values) into a NESTED, tagged Properties
    // tree — CONFIG at the top wraps `Property::Nested` containing
    // every `<PARAM>` sub-key as `Property::Scalar`. Matches the
    // shape `props::get` returns from a live BD cell, so consumers
    // can use `dict get [<ip>::Config::to -v $cfg] CONFIG` +
    // `Property::as_nested -v ...` to extract sub-trees.
    writeln!(
        out,
        "return [{config_ty}::from -v [Properties::from_dotted_pairs -v $_vw_d]]"
    )
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

    // Merge XML-derived DictSchemas for the top-level Properties-
    // shaped params (`tree.direct` only, so we don't clobber
    // schemas the split-node emitter builds for its own params).
    // These end up in `dict_schemas` for both the top-proc
    // arg-decl (`emit_arg_decl` picks up the typed newtype) and
    // the tail `append_dict_sub_procs` call in `generate()` (which
    // emits the newtype prelude + constructor proc for each).
    let mut dict_schemas: std::collections::HashMap<String, crate::DictSchema> =
        dict_schemas.clone();
    for p in &tree.direct {
        if dict_schemas.contains_key(&p.name) {
            continue;
        }
        // Anchor lookup: some Vivado top-level params carry a scalar
        // sentinel default (`0`) while a sibling `<modelParameter>`
        // of the same name holds the actual paired-list schema. Use
        // the model-param default as the anchor when the top-level
        // default doesn't parse as paired. Enables typed-constructor
        // emission for INTF_PARENT_PIN_LIST and similar — where the
        // slot names live in the internal HDL-generic view rather
        // than on the user-facing property.
        let anchor_default = model_param_anchor_default(component, &p.name);
        let use_anchor = !is_properties_shaped_param(p)
            && anchor_default.as_deref().is_some_and(|d| {
                !crate::paired_list::parse_paired_list(d).is_empty()
            });
        if !is_properties_shaped_param(p) && !use_anchor {
            continue;
        }
        let default = if use_anchor {
            anchor_default.as_deref().unwrap()
        } else {
            p.value.default_value()
        };
        let shape_path = lowercase_ident(&p.name);
        let mut schema = crate::DictSchema::from_paired_default(
            default,
            &shape_path,
            &opts.overrides,
        );
        // Extrapolate `QUAD<n>_<X>` keys across every quad the IP
        // ships (5 for gtwiz-versal) so the constructor exposes
        // ALL slots, not just quad0's — the model-param default
        // only enumerates one quad's worth as a template.
        // Also attaches auto-derived pin-path enums when the key
        // shape matches `QUAD<q>_<RX|TX><n>`. See
        // `extrapolate_quad_schema` for the details.
        if use_anchor {
            extrapolate_quad_schema(&mut schema, component, &shape_path, opts);
        }
        if schema.fields.is_empty() && schema.sub_schemas.is_empty() {
            continue;
        }
        dict_schemas.insert(p.name.clone(), schema);
    }
    // Take a reference to what all downstream code expects.
    let dict_schemas = &dict_schemas;

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
    // Emit newtype declarations + constructor procs for XML-derived
    // top-level dict schemas. These live flat under `<ip>::` (like
    // PS_PMC_CONFIG) — the CSV-driven ones the caller supplied get
    // emitted by `append_dict_sub_procs` at the tail of `generate`.
    // Emitting them here keeps the top-proc's typed arg references
    // (`hnic_pipe_parameters: <ip>::HnicPipeParameters`) resolvable
    // during validation.
    for p in &tree.direct {
        let Some(schema) = dict_schemas.get(&p.name) else {
            continue;
        };
        // Skip CSV-driven schemas (`append_dict_sub_procs` emits
        // those at the tail). We differentiate two rails:
        //   * XML-derived schemas — populated above from `tree.direct`
        //     via `is_properties_shaped_param` OR via the
        //     model-param anchor path. Both are keyed under
        //     `p.name`; caller's original `dict_schemas` didn't
        //     hold them yet. We check by whether the schema has
        //     content — CSV and XML both do, so that's ambiguous.
        //   * Anchor-derived: `p` isn't structurally Properties
        //     but a sibling model param is. Emit iff we DID pick
        //     up an anchor for it.
        // Simplest gate: emit whenever `dict_schemas` has an
        // entry that wasn't in the original caller-supplied map.
        // We express that indirectly via the two-track OR:
        // Properties-shaped (usual path) OR model-param anchor
        // present (INTF_PARENT_PIN_LIST-shaped path).
        let has_anchor = model_param_anchor_default(component, &p.name)
            .as_deref()
            .is_some_and(|d| {
                !crate::paired_list::parse_paired_list(d).is_empty()
            });
        if !is_properties_shaped_param(p) && !has_anchor {
            continue;
        }
        writeln!(&mut out).unwrap();
        emit_dict_props_prelude(&mut out, &ip_name, &[], &p.name);
        writeln!(&mut out).unwrap();
        emit_dict_sub_proc(&mut out, &ip_name, &[], &p.name, schema, opts);
    }
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

    // `configure`'s arg-doc — the whole documented kwarg pile
    // (direct + family + split-node). Zero cell handles, zero
    // `-name`/`-bd`. The old inline-`create` doc had all of these
    // mixed with `-name`/`-bd`; the split lifts them out into
    // `configure` so `<ip>::create`'s surface stays tiny.
    let mut configure_doc = Doc::new();
    for p in &tree.direct {
        emit_arg_decl(
            &mut configure_doc,
            component,
            presets,
            p,
            opts,
            "",
            &ip_name,
            dict_schemas,
            &[],
        );
    }
    // Family kwargs: one `-<stem_lower><i>` per family member,
    // typed as the newtype, with a doc comment referencing the
    // constructor via `[…]` for semantic-goto.
    //
    // `@default("")` is a placeholder — the analyzer's
    // `@default(...)` grammar rejects bracket-expressions like
    // `[<T>::empty]`, so we can't declare the semantically-correct
    // default in the annotation. The body's `__vw_kw_<arg>_set`
    // guard skips the merge loop when the caller didn't pass the
    // slot, so `$<arg>` is never dereferenced with the placeholder
    // value — the empty string never reaches the newtype machinery.
    if !families.is_empty() {
        if !tree.direct.is_empty() {
            configure_doc.push(Item::Blank);
        }
        for f in &families {
            let ctor = format!("{ip_name}::{}", lowercase_ident(&f.stem));
            let ty = stem_props_name(&ip_name, &f.stem);
            for i in &f.indices {
                let arg = format!("{}{i}", lowercase_ident(&f.stem));
                configure_doc.push(Item::DocComment(format!(
                    "Configuration for {} slot {i}. Construct with [{ctor}].",
                    f.stem
                )));
                configure_doc.push(Item::Command(Command {
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

    // Split-shape newtype preludes are emitted INSIDE each
    // split-node's own subfile (see `emit_split_node_constructor`)
    // rather than up here at module.htcl top-level. That way
    // opening `intf7.htcl` standalone in the LSP still finds the
    // `gtwiz_versal::Intf7Props` declaration the file needs.
    // module.htcl still sees them because it `src`s each subfile
    // via the `## ==== split-file: ... ==== ##` peel-off pass.

    // Split-node kwargs on configure: one per split-shape
    // constructor, typed as the node's newtype so configure
    // composes ALL configuration into one Config value.
    // Semantic-ref doc comment points at the constructor for
    // goto/hover.
    if !split_nodes.is_empty() {
        if !tree.direct.is_empty() || !families.is_empty() {
            configure_doc.push(Item::Blank);
        }
        for n in &split_nodes {
            let ctor_suffix = sanitize_ident(&n.label.to_ascii_lowercase());
            let ctor = format!("{ip_name}::{ctor_suffix}");
            let ty = split_props_name(&ip_name, &n.label);
            configure_doc.push(Item::DocComment(format!(
                "Configuration for the {} sub-tree. Construct with \
                 [{ctor}].",
                n.label
            )));
            configure_doc.push(Item::Command(Command {
                doc_comments: Vec::new(),
                words: vec![
                    Word::Raw("@default(\"\")".into()),
                    Word::Bare(format!("{ctor_suffix}: {ty}")),
                ],
                body: None,
            }));
        }
    }

    // Config prelude at file top level (namespace-eval double-
    // prefix workaround). Emit BEFORE the namespace-eval block
    // opens so `<ip>::Config` is available when the enclosed
    // `<ip>::configure` returns it and `<ip>::create` accepts it.
    writeln!(out).unwrap();
    emit_config_prelude(&mut out, &ip_name);
    writeln!(out).unwrap();

    // `configure` body: pure dict assembly + wrap. No cell handle,
    // no `set_property`, no `-bd` branch.
    let mut configure_body = String::new();
    write_dict_assembly_with_splits(
        &mut configure_body,
        &tree.direct,
        &family_merges,
        &dict_schema_newtypes,
        &split_nodes,
        &ip_name,
    );
    let config_ty = config_name(&ip_name);
    // Lift flat `CONFIG.<PARAM>` keys into a nested tagged Properties
    // tree (see `build_single_configure_body` for the rationale).
    writeln!(
        configure_body,
        "return [{config_ty}::from -v [Properties::from_dotted_pairs -v $_vw_d]]"
    )
    .unwrap();

    // `create`'s arg-doc — narrow surface: -name, -bd, -config.
    let mut create_doc = Doc::new();
    create_doc.push(Item::DocComment(
        "Project-level IP module name, or (when `-bd 1`) the \
         instance name in the block design."
            .into(),
    ));
    create_doc.push(Item::Command(Command::call(
        "name",
        std::iter::empty::<Word>(),
    )));
    push_bd_switch_arg(&mut create_doc);
    create_doc.push(Item::DocComment(format!(
        "Typed configuration value. Construct with [{ip_name}::configure]. \
         Defaults to an empty config (all parameters take their IP defaults)."
    )));
    create_doc.push(Item::Command(Command {
        doc_comments: Vec::new(),
        words: vec![
            Word::Raw("@default(\"\")".into()),
            Word::Bare(format!("config: {config_ty}")),
        ],
        body: None,
    }));

    let create_body = build_single_create_body(&vlnv, &ip_name);

    // Assemble the `namespace eval <ip> { … }` body in
    // families → splits → configure → create order.
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
        // The `out` buffer collects dict-schema newtypes and
        // constructor procs for this node's Properties args. They
        // live at fully-qualified names like
        // `gtwiz_versal::intf0::ChannelMap::from`, which Tcl only
        // resolves correctly when NOT inside a `namespace eval <ip>
        // { … }` block. So `emit_split_node_constructor` writes them
        // to the outer `out` buffer (which is emitted at the top
        // level of the file, before `write_namespace_block` wraps
        // `procs`).
        emit_split_node_constructor(
            &mut procs, &mut out, &ip_name, component, presets, opts, n,
        );
    }
    if !families.is_empty() || !split_nodes.is_empty() {
        writeln!(procs).unwrap();
    }
    emit_proc(
        &mut procs,
        "configure",
        &configure_doc,
        Some(&config_ty),
        &configure_body,
    );
    writeln!(procs).unwrap();
    emit_proc(
        &mut procs,
        "create",
        &create_doc,
        Some("bd_cell"),
        &create_body,
    );

    write_namespace_block(&mut out, &ip_name, &procs);
    out
}

/// Emit the newtype prelude for a split-shape node. Same shape as
/// [`emit_family_prelude`] / [`emit_dict_props_prelude`] — one
/// `namespace eval <T> {}` + `type <T> = Properties` + the four
/// helper procs. Consumed by [`emit_split_node_constructor`] and
/// the top-proc merge loop in
/// [`write_set_property_dict_with_splits`].
/// Emit the top-level `<ip>::Config = Properties` newtype at file
/// top level (outside `namespace eval <ip> {…}` to sidestep the
/// analyzer's double-prefix bug on qualified proc names inside
/// namespace-eval blocks). Structural mirror of
/// [`emit_split_props_prelude`] / [`emit_family_prelude`] /
/// [`emit_dict_props_prelude`] — same four helpers (`empty`,
/// `from`, `to`, `repr`) with identity implementations. The name
/// is always `<ip>::Config` — one per generated wrapper — so the
/// callsite pattern `set cfg [<ip>::configure -foo x]` returns a
/// value the analyzer knows about and `<ip>::create` can accept
/// via its typed `-config <ip>::Config` param.
fn emit_config_prelude(out: &mut String, ip_name: &str) {
    let qualified = config_name(ip_name);
    writeln!(
        out,
        "## Typed configuration value for [{ip_name}::create]. \
         Construct with [{ip_name}::configure].",
    )
    .unwrap();
    writeln!(out, "namespace eval {ip_name} {{}}").unwrap();
    writeln!(out, "namespace eval {qualified} {{}}").unwrap();
    writeln!(out, "type {qualified} = Properties").unwrap();
    // Values are a properly nested tagged Properties tree by the
    // time they reach Config (see `build_single_configure_body` —
    // configure's return path lifts through
    // `Properties::from_dotted_pairs`). Delegate to
    // `Properties::repr` for uniform tagged-Property rendering —
    // same `KEY Scalar(VALUE)` / `KEY Nested(…)` shape the REPL's
    // syntax highlighter colours specially.
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

/// Qualified name of the top-level Config newtype for `ip_name`.
fn config_name(ip_name: &str) -> String {
    format!("{ip_name}::Config")
}

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
    // Buffer for output that must live OUTSIDE the ip-scoped
    // `namespace eval <ip> { … }` block that wraps `out`. Dict-schema
    // sub-procs (which use fully-qualified names like
    // `gtwiz_versal::intf0::ChannelMap::from`) go here — placing them
    // inside the `namespace eval` block causes Tcl to double the ip
    // prefix (`gtwiz_versal::gtwiz_versal::intf0::…`) and the
    // procs become undiscoverable.
    outer_out: &mut String,
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
    // Build the XML-driven dict-schema map for this split-node's
    // Properties-shaped params (channel_map, gt_settings, etc. on an
    // `intf` node). Each schema gets emitted as a nested-namespace
    // typed sub-proc under `<ip>::<node_local>::`, and the arg-decl
    // references the typed newtype instead of raw `Properties`.
    // See [`build_split_dict_schemas`] for the extraction logic.
    let node_local = sanitize_ident(&n.label.to_ascii_lowercase());
    let local_dict_schemas = build_split_dict_schemas(n, &node_local, opts);
    for p in &n.direct {
        emit_arg_decl(
            &mut doc,
            component,
            presets,
            p,
            opts,
            &n.label,
            ip_name,
            &local_dict_schemas,
            &[node_local.as_str()],
        );
    }

    // Split-file marker: everything between OPEN and CLOSE ends up
    // in a sibling `.htcl` file (see `split_into_files`) with the
    // main module.htcl gaining a `src ./<basename>` line at this
    // spot. Basename derives from the split-node's local ident so
    // gtwiz-versal's 8 intfN nodes land in `intf0.htcl`…`intf7.htcl`.
    // For small IPs the marker still emits but every subfile stays
    // tiny (a handful of lines) which the tree-sitter parser handles
    // instantly.
    writeln!(outer_out, "## ==== split-file: {node_local}.htcl ==== ##")
        .unwrap();
    // Every subfile needs its own `src @vivado-cmd` so it can be
    // analyzed (and hovered / go-to-def'd) standalone in the LSP
    // without the analyzer opening it via module.htcl. The dict-
    // schema prelude references `Properties::repr`, `Properties::empty`,
    // and similar procs that live in the vivado-cmd library; without
    // the src line the analyzer flags every reference as undefined.
    writeln!(outer_out, "src @vivado-cmd").unwrap();
    writeln!(outer_out).unwrap();

    // Split-shape newtype prelude for THIS node — moved from
    // module.htcl into the subfile so opening the subfile
    // standalone in the LSP finds the type declaration the file's
    // own return-type annotations reference.
    emit_split_props_prelude(outer_out, ip_name, &n.label);

    // Emit the per-node typed sub-procs first so their newtypes
    // are visible before this proc's signature references them.
    // Each sub-proc lives at `<ip>::<node_local>::<param_lower>`;
    // its newtype at `<ip>::<node_local>::<PascalOfParam>`.
    // Placed on the outer buffer so the fully-qualified proc names
    // (`gtwiz_versal::intf0::…`) don't get an accidental extra
    // `gtwiz_versal::` prefix from Tcl's namespace resolution.
    emit_split_dict_sub_procs(
        outer_out,
        ip_name,
        &node_local,
        &n.label,
        &n.direct,
        &local_dict_schemas,
        opts,
    );

    let mut body = String::new();
    writeln!(body, "set _vw_d [dict create]").unwrap();
    for p in &n.direct {
        let field_key = strip_prefix(&p.name, &n.label);
        let arg = lowercase_ident(field_key);
        // Dict-schema-backed slot: unwrap the typed newtype to its
        // raw paired dict via `<T>::to`. Other Properties-shaped
        // slots (none in gtwiz-versal after this change, but the
        // path still handles legacy IPs whose defaults slip past
        // the schema extractor) arrive as raw paired dicts; store
        // `$arg` verbatim without `Properties::to_raw` (which would
        // try to strip Scalar/Nested tags that aren't present).
        let value_expr = if local_dict_schemas.contains_key(field_key) {
            let ret_ty =
                dict_props_name(ip_name, &[node_local.as_str()], field_key);
            format!("[{ret_ty}::to -v ${arg}]")
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
    // Emit the split-node's own constructor proc into the outer
    // buffer (same file as its dict-schema sub-procs) with a fully
    // qualified name — so it lives at `<ip>::<ctor_local>` without
    // needing to sit inside a `namespace eval <ip> {…}` block. That
    // lets the whole split-node emission end up in ONE sibling file
    // wrapped by split markers, instead of being torn between the
    // main-file namespace-block and the outer-scope schema block.
    let ctor_qualified = format!("{ip_name}::{ctor_local}");
    emit_proc(outer_out, &ctor_qualified, &doc, Some(&ret_ty), &body);

    // Split-file close marker — see the matching OPEN above and
    // `split_into_files` for the peel logic.
    writeln!(
        outer_out,
        "## ==== end split-file: {node_local}.htcl ==== ##"
    )
    .unwrap();

    // Consume `out` (the namespace-block buffer) so the reader can
    // see we intentionally skipped writing into it — the split-node
    // no longer contributes short-name procs there.
    let _ = out;
}

/// Build the XML-driven DictSchema map for a split-node's
/// Properties-shaped direct params. Runs once per split-node
/// emission; the returned map is keyed by raw IP-XACT parameter
/// name (matches the arg-decl / body lookup pattern).
///
/// The shape-path passed to `DictSchema::from_paired_default` is
/// `<stem>::<param_lower>`, where `stem` is the un-indexed stem of
/// the node's label (`INTF0` → `intf`, `QUAD0_CH0` → `quad_ch`),
/// so overrides written against `intf::gt_settings` apply across
/// every `intf0`..`intf7` sibling. Matches the "one override entry
/// per family" convention documented in
/// [`crate::overrides::OverridesFile`].
fn build_split_dict_schemas(
    n: &Node<'_>,
    _node_local: &str,
    opts: &GenerateOptions,
) -> std::collections::HashMap<String, crate::DictSchema> {
    let stem_lower = split_stem_lower(&n.label);
    let mut out = std::collections::HashMap::new();

    // Pass 1 — extract each Properties-shaped param's own schema
    // from its default value. Empty results (from `0` / `""` /
    // `NA NA` sentinel defaults) still land in the map so pass 2
    // can decide whether to synthesize from an anchor.
    for p in &n.direct {
        if !is_properties_shaped_param(p) {
            continue;
        }
        let field_key = strip_prefix(&p.name, &n.label).to_string();
        let field_lower = lowercase_ident(&field_key);
        let shape_path = if stem_lower.is_empty() {
            field_lower
        } else {
            format!("{stem_lower}::{field_lower}")
        };
        let schema = crate::DictSchema::from_paired_default(
            p.value.default_value(),
            &shape_path,
            &opts.overrides,
        );
        out.insert(field_key, schema);
    }

    // Pass 2 — resolve schemas whose own default was uninformative.
    // The gtwiz-versal LR{n}_SETTINGS / GT_SETTINGS / GT_INTERNAL
    // shapes all share a field vocabulary that only lives inside
    // TXRX_OPTIONAL_PORTS's `INTF_LR_SETTINGS.LR0_SETTINGS` payload
    // (see the Explore report). Find that payload once, then use it
    // to backfill:
    //   - each trivial-schema `LR{n}_SETTINGS` slot (16 of them),
    //   - the tcldict-tagged `GT_SETTINGS` / `GT_INTERNAL` params,
    //     synthesized as wrappers holding 16 LR sub-slots.
    let anchor_lr = out
        .get("TXRX_OPTIONAL_PORTS")
        .and_then(|s| s.sub_schemas.get("INTF_LR_SETTINGS"))
        .and_then(|s| s.sub_schemas.get("LR0_SETTINGS"))
        .cloned();
    let params_by_key: std::collections::HashMap<&str, &&Parameter> = n
        .direct
        .iter()
        .map(|p| (strip_prefix(&p.name, &n.label), p))
        .collect();
    if let Some(lr_template) = anchor_lr.as_ref() {
        for i in 0..16 {
            let key = format!("LR{i}_SETTINGS");
            // Only backfill if the slot exists on the node (LR0..15
            // are all declared params on gtwiz-versal's intf nodes).
            if !params_by_key.contains_key(key.as_str()) {
                continue;
            }
            // Always replace with the anchor template — the LR*
            // params' own defaults are Xilinx sentinels (`NA NA`
            // extracts to a bogus `NA=NA` field, hardly trivial by
            // fields.is_empty() but useless as a schema). The
            // TXRX_OPTIONAL_PORTS anchor carries the real field
            // vocabulary that Vivado actually accepts.
            let mut copy = lr_template.clone();
            // The anchor's fields were built under the txrx shape
            // path; reapply the destination slot's overrides so
            // `intf::lrN_settings` gets its own refinements.
            let dst_shape_path = if stem_lower.is_empty() {
                lowercase_ident(&key)
            } else {
                format!("{stem_lower}::{}", lowercase_ident(&key))
            };
            copy.reapply_overrides(&dst_shape_path, &opts.overrides);
            out.insert(key, copy);
        }
        // Synthesize wrappers for tcldict GT_SETTINGS /
        // GT_INTERNAL. Both take the same LR0..LR15 sub-slot shape
        // (Vivado accepts `CONFIG.INTF*_GT_SETTINGS(LR<n>_SETTINGS)
        // {...}` — the parenthesized sub-key IS the wrapper slot).
        for wrapper_key in ["GT_SETTINGS", "GT_INTERNAL"] {
            let Some(p) = params_by_key.get(wrapper_key) else {
                continue;
            };
            if !p.has_parameter_type("tcldict") {
                continue;
            }
            let existing = out.get(wrapper_key);
            if existing.is_some_and(|s| !schema_is_trivial(s)) {
                continue;
            }
            let mut sub = std::collections::BTreeMap::new();
            for i in 0..16 {
                let mut copy = lr_template.clone();
                // Sub-slot's shape path is `<stem>::<wrapper>::lrN_settings`
                // — refines overrides written for that specific path
                // (e.g. `intf::gt_settings::lr0_settings`), separate
                // from the top-level `intf::lrN_settings` slot.
                let wrapper_lower = wrapper_key.to_ascii_lowercase();
                let lr_lower = format!("lr{i}_settings");
                let sub_shape_path = if stem_lower.is_empty() {
                    format!("{wrapper_lower}::{lr_lower}")
                } else {
                    format!("{stem_lower}::{wrapper_lower}::{lr_lower}")
                };
                copy.reapply_overrides(&sub_shape_path, &opts.overrides);
                sub.insert(format!("LR{i}_SETTINGS"), copy);
            }
            out.insert(
                wrapper_key.to_string(),
                crate::DictSchema {
                    fields: Vec::new(),
                    sub_schemas: sub,
                },
            );
        }
    }

    // Drop trivial schemas — params whose default was `0` / `""`
    // sentinels with no anchor available. Fall back to raw
    // Properties for those; a bare typed slot with no fields
    // would just clutter the LSP surface.
    out.retain(|_, schema| !schema_is_trivial(schema));
    out
}

/// A schema is "trivial" when it has no fields AND no sub-slots —
/// derived from an empty default (`0`, `""`) or a nonsense `NA NA`
/// where the parser found no meaningful structure. Callers use
/// this to decide whether to backfill from an anchor param.
fn schema_is_trivial(s: &crate::DictSchema) -> bool {
    s.fields.is_empty() && s.sub_schemas.is_empty()
}

/// Emit the dict-schema sub-procs (prelude + constructor + any
/// nested sub-slots) for each entry in `dict_schemas`, all under
/// `<ip>::<node_local>::`. Deep recursion happens inside
/// `emit_dict_sub_proc` via `emit_dict_sub_schemas`, so multi-level
/// XML shapes (`INTF0_TXRX_OPTIONAL_PORTS` → `INTF_LR_SETTINGS` →
/// `LR0_SETTINGS`) unfold naturally.
fn emit_split_dict_sub_procs(
    out: &mut String,
    ip_name: &str,
    node_local: &str,
    node_label: &str,
    params: &[&Parameter],
    dict_schemas: &std::collections::HashMap<String, crate::DictSchema>,
    opts: &GenerateOptions,
) {
    // Emit in the same order params appear so review diffs are
    // deterministic and consumers can visually pair the newtype
    // with its Properties arg in the outer proc.
    for p in params {
        let stripped = strip_prefix(&p.name, node_label);
        let Some(schema) = dict_schemas.get(stripped) else {
            continue;
        };
        writeln!(out).unwrap();
        emit_dict_props_prelude(out, ip_name, &[node_local], stripped);
        writeln!(out).unwrap();
        emit_dict_sub_proc(out, ip_name, &[node_local], stripped, schema, opts);
    }
}

/// Strip the trailing digit(s) from a split-node label and
/// lowercase — `INTF0` → `"intf"`, `QUAD0_CH1` → `"quad_ch"`.
/// Used as the shape-path stem when looking up overrides so a
/// single override entry applies across every indexed instance in
/// the family.
fn split_stem_lower(label: &str) -> String {
    let mut segs: Vec<String> = Vec::new();
    for seg in label.split('_') {
        // Strip a trailing digit run from each segment. Keeps
        // multi-word stems (`QUAD_CH`) intact while collapsing
        // `INTF0` → `INTF`, `QUAD0_CH1` → `QUAD_CH`.
        let trimmed = seg.trim_end_matches(|c: char| c.is_ascii_digit());
        if trimmed.is_empty() {
            continue;
        }
        segs.push(trimmed.to_ascii_lowercase());
    }
    segs.join("_")
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
fn write_dict_assembly_with_splits(
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
            } else if is_properties_shaped_param(p) {
                // Properties-typed args now arrive as tagged trees
                // (from other IPs' `configure` procs, or extracted
                // via `dict get $props CONFIG` + `Property::as_nested`
                // by the caller). Unwrap tags to a raw paired list
                // via `Properties::to_raw` so
                // `Properties::from_dotted_pairs` (which lifts the
                // whole `_vw_d` at the end of the configure body)
                // sees consistent bare-string values across all
                // sub-slots. Without this the tagged-tree elements
                // would confuse the shim's structural lifter.
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
    // No finalization here — callers wrap the assembled `_vw_d`
    // in a typed `<ip>::Config` value. The `set_property` finalize
    // has moved to `emit_config_finalize` on the `<ip>::create`
    // side, which unwraps the caller's `-config` param and applies.
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// Break the concatenated generator output into one main file plus
/// zero-or-more sibling files.
///
/// Peel rule: scan the aggregate for the split marker
/// `## ==== split-file: <name> ==== ##` … (matching close marker).
/// The interior lands in `subfiles[<name>]`; the open/close markers
/// in `main` become a `src ./<name>.htcl` line so the main file's
/// downstream references still resolve at load time.
///
/// Content without markers stays in `main` untouched. Callers that
/// don't want the split at all (unit tests, single-file consumers)
/// use `MultiFileOutput::into_single()` to re-flatten.
///
/// Threshold-based auto-splitting layers on top: the emitters wrap
/// each large split-node in markers so this pass does the physical
/// separation. Small nodes don't get markers → they stay inline.
fn split_into_files(aggregate: String) -> MultiFileOutput {
    const OPEN: &str = "## ==== split-file: ";
    const CLOSE: &str = "## ==== end split-file: ";
    let mut main = String::with_capacity(aggregate.len());
    let mut subfiles: Vec<(String, String)> = Vec::new();
    let mut rest = aggregate.as_str();
    while let Some(open_off) = rest.find(OPEN) {
        // Everything before the marker stays in main.
        main.push_str(&rest[..open_off]);
        rest = &rest[open_off + OPEN.len()..];
        let Some(open_end) = rest.find('\n') else {
            // Malformed marker — retain the rest in main and bail.
            main.push_str(OPEN);
            main.push_str(rest);
            return MultiFileOutput { main, subfiles };
        };
        let name_line = rest[..open_end].trim();
        // Strip the trailing ` ==== ##` from the marker line. The
        // suffix has interleaved whitespace, ``#``, and ``=``; strip
        // them all in one pass so partial-suffix boundaries (like
        // "==== " with the trailing space blocking the "====" strip)
        // don't leak into the filename.
        let name = name_line
            .trim_end_matches(|c: char| {
                c == '#' || c == '=' || c.is_whitespace()
            })
            .trim();
        rest = &rest[open_end + 1..];
        // Find the matching close marker.
        let close_needle = format!("{CLOSE}{name}");
        let Some(close_off) = rest.find(&close_needle) else {
            // Unpaired open — restore in main and stop splitting.
            main.push_str(OPEN);
            main.push_str(name_line);
            main.push('\n');
            main.push_str(rest);
            return MultiFileOutput { main, subfiles };
        };
        let body = rest[..close_off].to_string();
        // Advance past the close marker's line (up to and
        // including its newline).
        let after = &rest[close_off..];
        let close_line_end =
            after.find('\n').map(|n| n + 1).unwrap_or(after.len());
        rest = &after[close_line_end..];
        // Emit the src line into main so the loader still walks
        // this subfile. Uses a relative `./` path — the CLI
        // writes the subfile as a sibling of the main output.
        writeln!(main, "src ./{name}").unwrap();
        subfiles.push((name.to_string(), body));
    }
    main.push_str(rest);
    MultiFileOutput { main, subfiles }
}

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

/// Emit the paired-list assembly loops that build `_vw_d` from
/// direct parameters + composed families + dict-schema newtypes.
/// Arg names are built by stripping `prefix_to_strip` from each
/// parameter's full IP-XACT name; the `CONFIG.<NAME>` key keeps
/// the full name Vivado expects.
///
/// Callers: `<ip>::configure`'s body (both single and split shapes).
/// The output is a plain `_vw_d` paired list — no finalize, no
/// `set_property` call. `configure` wraps the assembled dict in
/// `[<ip>::Config::from -v [Properties::from -v $_vw_d]]` and
/// returns it; `<ip>::create` unwraps and applies. See
/// [`emit_config_finalize`] for the corresponding apply side.
fn write_dict_assembly(
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
            } else if is_properties_shaped_param(p) {
                // Properties-typed args now arrive as tagged trees
                // (from other IPs' `configure` procs, or extracted
                // via `dict get $props CONFIG` + `Property::as_nested`
                // by the caller). Unwrap tags to a raw paired list
                // via `Properties::to_raw` so
                // `Properties::from_dotted_pairs` (which lifts the
                // whole `_vw_d` at the end of the configure body)
                // sees consistent bare-string values across all
                // sub-slots. Without this the tagged-tree elements
                // would confuse the shim's structural lifter.
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
}

/// Emit the create-side finalize: unwrap `$config` through
/// `<ip>::Config::to`, flatten the nested tagged Properties tree
/// back into the flat `CONFIG.<PARAM> value` paired list
/// `set_property -dict` expects (via [`Properties::to_dotted_flat`]),
/// then apply against the cell handle. Two-mode `-bd` branch:
/// `bd=1` → target `$cell` directly (a bd_cell path); `bd=0` →
/// resolve via `[get_ips $name]` because `create_ip` returns an
/// XCI file path, not an IP handle.
fn emit_config_finalize(out: &mut String, ip_name: &str) {
    let config_ty = config_name(ip_name);
    writeln!(
        out,
        "set _dict [Properties::to_dotted_flat -v [{config_ty}::to -v $config]]"
    )
    .unwrap();
    writeln!(out, "if {{[llength $_dict] > 0}} {{").unwrap();
    writeln!(out, "  if {{$bd}} {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_dict -objects $cell"
    )
    .unwrap();
    writeln!(out, "  }} else {{").unwrap();
    writeln!(
        out,
        "    vivado_cmd::set_property -dict $_dict -objects [get_ips $name]"
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
    let typed_name = if is_properties_shaped_param(shape_param) {
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
        // Properties-typed sub-slots arrive as raw paired-list
        // dicts; scalar sub-slots are plain strings. Both cases
        // reduce to `$arg`. The old `Properties::to_raw` step
        // required Property::Scalar/Nested tags that our new
        // configure-based value flow no longer produces.
        let value_expr = format!("${arg}");
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
/// Fully-qualified newtype name for a dict-schema sub-proc.
///
/// `("gtwiz_versal", ["intf", "gt_settings"], "LR0_SETTINGS")` →
/// `"gtwiz_versal::intf::gt_settings::Lr0Settings"`. The IP name
/// keeps its snake_case; namespace-prefix segments keep their
/// lowercase form; the leaf param name becomes PascalCase.
fn dict_props_name(
    ip_name: &str,
    namespace_prefix: &[&str],
    param_name: &str,
) -> String {
    let scope = proc_scope(ip_name, namespace_prefix);
    format!("{scope}::{}", dict_props_local(param_name))
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
        // Top-level dict-schema newtype lookup for the top-proc
        // arg-decl (`-ps_pmc_config: versal_cips::PsPmcConfig`).
        // Nested newtypes referenced by sub-schemas aren't part of
        // this map — they're referenced by the sub-procs directly.
        .map(|k| (k.clone(), dict_props_name(ip_name, &[], k)))
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
    // Namespace prefix under which any dict-schema newtype for `p`
    // has been (or will be) emitted. Empty for top-proc args (the
    // classic PS_PMC_CONFIG rail); non-empty for split-node procs
    // that emit their dict-schema sub-procs under
    // `<ip>::<node_local>::`. Ignored when the param isn't
    // dict-schema-backed.
    dict_ns: &[&str],
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
    //
    // Dict-schema map keys: split-node schemas are keyed by the
    // STRIPPED slot name (`CHANNEL_MAP`) so the emitted newtype is
    // `<ip>::intf0::ChannelMap` not `<ip>::intf0::Intf0ChannelMap`.
    // Top-proc schemas use the raw param name (`PS_PMC_CONFIG`) as
    // key with an empty prefix_to_strip → strip is a no-op.
    let schema_key = strip_prefix(&p.name, prefix_to_strip);
    let dict_schema = dict_schemas.get(schema_key);
    if let Some(_schema) = dict_schema {
        let ctor_scope = proc_scope(ip_name, dict_ns);
        let ctor = format!("{ctor_scope}::{}", schema_key.to_ascii_lowercase());
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
        // Always emit `@default(...)` — a missing default would
        // make the param required at the analyzer level, but every
        // IP-XACT parameter is optional in Vivado's semantics (the
        // IP uses its own internal default when the user doesn't
        // override). Empty defaults happen when the XML's
        // `<spirit:value>` is whitespace-only (BOARD_PARAMETER,
        // ANLT_PARAMETERS in gtwiz_versal, etc.) — quick-xml's
        // `$text` strips leading/trailing whitespace, so we see
        // `""`. Emit `@default("")` as the placeholder and let the
        // runtime `__vw_kw_<arg>_set` guard skip the merge loop
        // when the caller didn't pass a value — same pattern as
        // the family / split-node kwarg placeholders.
        let default = p.value.default_value();
        if !default.is_empty() {
            words.push(Word::Raw(format!(
                "@default({})",
                format_attribute_value(default)
            )));
        } else {
            words.push(Word::Raw("@default(\"\")".into()));
        }
    } else {
        // Dict-schema-backed param: empty-string placeholder
        // default. The runtime guard (`__vw_kw_<arg>_set`)
        // prevents the placeholder from ever being dereferenced
        // through the newtype machinery.
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
    let typed_name = if dict_schema.is_some() {
        // Dict-schema slot — reference the newtype at whatever
        // namespace it was emitted under. Top-proc args pass
        // `dict_ns=&[]`; split-node args pass `dict_ns=&[node_local]`.
        // Use the stripped key so the PascalCase newtype name isn't
        // prefixed with the split-node's label (see schema_key).
        format!(
            "{lowered}: {}",
            dict_props_name(ip_name, dict_ns, schema_key)
        )
    } else if is_properties_shaped_param(p) {
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
/// Parameter-level Properties-shape decision. Prefers Xilinx's
/// authoritative vendor tag (`<xilinx:parameterInfo><xilinx:parameterType>tcldict`
/// on `<spirit:parameter>`) when present, then falls back to the
/// structural default-value heuristic below.
///
/// Xilinx writes the default of a tcldict param as the bare
/// sentinel `0` (see `INTF0_GT_SETTINGS`, `INTF0_GT_INTERNAL`,
/// `INTF1_*` in the `gtwiz_versal_v1_0` component.xml), which the
/// structural heuristic reads as scalar and gets wrong. The
/// vendor tag captures the array-indexed compound-property
/// intent — Vivado's Tcl uses `CONFIG.INTF0_GT_SETTINGS(LR0_SETTINGS)
/// {…}` on those params — so params carrying it must be exposed
/// as Properties-typed args regardless of their default shape.
fn is_properties_shaped_param(p: &Parameter) -> bool {
    p.has_parameter_type("tcldict")
        || is_properties_shaped(p.value.default_value())
}

/// Look up the sibling `<modelParameter>` whose name matches
/// `param_name` and return its default. Vivado sometimes hides the
/// paired-list schema for a user-facing scalar param in an
/// internally-flagged model parameter with the same name — the
/// `INTF_PARENT_PIN_LIST` case (top-level default `0`, model-param
/// default `QUAD0_RX0 undef QUAD0_RX1 undef …`). `None` when no
/// matching model param exists or when the two names diverge.
fn model_param_anchor_default(
    component: &Component,
    param_name: &str,
) -> Option<String> {
    component
        .model_parameters()
        .find(|mp| mp.name == param_name)
        .map(|mp| mp.value.default_value().to_string())
}

/// Post-process an anchor-derived schema: if its keys follow the
/// `QUAD<n>_(RX|TX)<m>` pattern (Vivado's per-quad-per-channel
/// pin-map convention), replicate the slot set across every quad
/// the IP declares and attach an `@enum(…)` value list of valid
/// pin paths.
///
/// **Key extrapolation.** Model-param anchors typically only
/// enumerate ONE quad's worth of slots as a template. Detect the
/// `QUAD<n>_` prefix and, for each other quad `q` up to the IP's
/// max, clone the `QUAD0_*` slots as `QUAD<q>_*`. The max is
/// derived from the parameter tree — every top-level param whose
/// name starts `QUAD<k>_` counts, so gtwiz-versal's 5-quad layout
/// materializes 40 slots (5 × 8) from the 8-slot QUAD0 template.
///
/// **Value enum.** For each slot whose stripped name matches
/// `RX<m>` or `TX<m>`, build the enum
/// `[undef, /INTF0_<dir><n>_GT_IP_Interface_0, /INTF1_<dir><n>_…,
/// … /INTF<N-1>_<dir><n>_…]`. `N` = number of interface split
/// nodes on the IP (8 for gtwiz-versal, detected by scanning the
/// parameter set for the `INTF<k>_` prefix). `n` ranges over the
/// channel count implied by the source slot name — same convention
/// Vivado's BD builder uses when materializing GT interface pins.
fn extrapolate_quad_schema(
    schema: &mut crate::DictSchema,
    component: &Component,
    shape_path: &str,
    opts: &GenerateOptions,
) {
    // Detect the quad-prefix pattern on any existing field.
    let has_quad_pattern = schema
        .fields
        .iter()
        .any(|f| parse_quad_slot(&f.name).is_some());
    if !has_quad_pattern {
        return;
    }
    let max_quads = count_indexed_prefix(component, "QUAD");
    let max_intfs = count_indexed_prefix(component, "INTF");
    // Number of RX/TX channels per interface. Look at any INTF_RXn
    // or INTF_TXn model params — same shape as the QUAD channels.
    // Fall back to 4 (Xilinx's fixed per-interface channel count on
    // Versal GT wizards) if we can't count.
    let n_channels = detect_channel_count(schema).max(1);

    // Build a fresh field list: one per (quad, orig_local_slot)
    // pair, ordered quad-major then original-order.
    let template: Vec<crate::DictField> = std::mem::take(&mut schema.fields);
    let mut fields = Vec::with_capacity(template.len() * max_quads.max(1));
    for q in 0..max_quads.max(1) {
        for f in &template {
            let Some((_orig_q, local)) = parse_quad_slot(&f.name) else {
                // Non-QUAD-shaped keys stay as-is on the first quad
                // iteration only, so we don't duplicate them.
                if q == 0 {
                    fields.push(f.clone());
                }
                continue;
            };
            let name = format!("QUAD{q}_{local}");
            let field_lookup = lowercase_ident(&name);
            let field_override =
                opts.overrides.field(shape_path, &field_lookup);
            let mut enum_values = f.enum_values.clone();
            // Auto-derived pin-path enum. Only overwrite when the
            // slot name has a recognizable direction — leaves
            // non-RX/TX slots (uncommon on this pattern but
            // possible) alone.
            if let Some(dir_ch) = parse_dir_channel(&local) {
                let derived =
                    derive_pin_enum(dir_ch.0, dir_ch.1, max_intfs, n_channels);
                if !derived.is_empty() {
                    enum_values = derived;
                }
            }
            let mut default = f.default.clone();
            if let Some(fo) = field_override {
                if let Some(d) = &fo.default {
                    default = d.clone();
                }
                if let Some(ev) = &fo.enum_values {
                    enum_values = ev.iter().cloned().collect();
                }
            }
            fields.push(crate::DictField {
                name,
                default,
                description: f.description.clone(),
                enum_values,
            });
        }
    }
    schema.fields = fields;
}

/// Split a `QUAD<n>_<REST>` slot name into `(n, REST)`. Returns
/// `None` when the prefix doesn't match.
fn parse_quad_slot(name: &str) -> Option<(usize, String)> {
    let rest = name.strip_prefix("QUAD")?;
    let digit_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let n: usize = rest[..digit_end].parse().ok()?;
    let after = rest[digit_end..].strip_prefix('_')?;
    Some((n, after.to_string()))
}

/// Split an `RX<m>` / `TX<m>` slot local name into
/// `(direction, m)`. Direction is `"RX"` or `"TX"`.
fn parse_dir_channel(local: &str) -> Option<(&'static str, usize)> {
    let (dir, rest) = if let Some(r) = local.strip_prefix("RX") {
        ("RX", r)
    } else if let Some(r) = local.strip_prefix("TX") {
        ("TX", r)
    } else {
        return None;
    };
    // Channel index has to be a run of digits terminating the local
    // name (`RX0`, `TX3`). Anything else disqualifies.
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let m: usize = rest.parse().ok()?;
    Some((dir, m))
}

/// Count how many top-level params share the `<prefix><N>_` shape,
/// which gives us the max index for that family. Used to figure
/// out the IP's max quad count and interface count without
/// hard-coding.
fn count_indexed_prefix(component: &Component, prefix: &str) -> usize {
    let mut indices = std::collections::BTreeSet::new();
    for p in component.component_parameters() {
        let Some(rest) = p.name.strip_prefix(prefix) else {
            continue;
        };
        let Some(digit_end) = rest.find(|c: char| !c.is_ascii_digit()) else {
            continue;
        };
        if rest[digit_end..].starts_with('_') {
            if let Ok(n) = rest[..digit_end].parse::<usize>() {
                indices.insert(n);
            }
        }
    }
    indices.iter().max().map(|n| n + 1).unwrap_or(0)
}

/// Peek at an anchor's fields to figure out how many RX/TX
/// channels the shape carries. `QUAD0_RX0..RX3` → 4. Returns 0
/// when no direction-shaped slots are present.
fn detect_channel_count(schema: &crate::DictSchema) -> usize {
    let mut max_ch: usize = 0;
    for f in &schema.fields {
        let Some((_q, local)) = parse_quad_slot(&f.name) else {
            continue;
        };
        if let Some((_dir, ch)) = parse_dir_channel(&local) {
            max_ch = max_ch.max(ch + 1);
        }
    }
    max_ch
}

/// Build the `@enum(…)` value list for a `QUAD<q>_<dir><n>` slot.
/// Enum contents: the `undef` sentinel + every valid interface
/// pin path (`/INTF<i>_<dir><m>_GT_IP_Interface_0`) for `i` in
/// `0..n_intfs` and `m` in `0..n_channels`. Empty when either
/// axis is zero (defensive — caller falls back to whatever the
/// XML default declares).
fn derive_pin_enum(
    dir: &'static str,
    _slot_channel: usize,
    n_intfs: usize,
    n_channels: usize,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    if n_intfs == 0 || n_channels == 0 {
        return out;
    }
    out.insert("undef".to_string());
    for i in 0..n_intfs {
        for m in 0..n_channels {
            out.insert(format!("/INTF{i}_{dir}{m}_GT_IP_Interface_0"));
        }
    }
    out
}

fn is_properties_shaped(default: &str) -> bool {
    // Fast path via the Tcl-aware paired-list tokenizer — catches
    // defaults with `{…}`-grouped values (INTF*_TXRX_OPTIONAL_PORTS
    // and other tcldict params carry braces in their inner
    // `INTF_LR_SETTINGS {LR0_SETTINGS {…}}` payload). The naive
    // `split_whitespace` heuristic below tokenizes those braces as
    // separate items and misclassifies the default as non-paired.
    if !crate::paired_list::parse_paired_list(default).is_empty() {
        return true;
    }
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
        )
        .into_single();
        // Procs live inside `namespace eval <ip> { … }` and get
        // the 2-space indent from `write_namespace_block`. Two
        // procs: `configure` (typed value constructor) and
        // `create` (cell instantiator + config applier).
        let n_procs = out.matches("\n  proc ").count();
        assert_eq!(n_procs, 2, "{out}");
        assert!(out.contains("proc configure"));
        assert!(out.contains("proc create"));
        assert!(out.contains("namespace eval demo {"));
        // Top-level Config newtype ships outside the namespace
        // block (see emit_config_prelude for why).
        assert!(out.contains("type demo::Config = Properties"));
    }

    #[test]
    fn split_mode_emits_top_and_sub_procs() {
        let component = mk_split_component(60); // 60 * 2 + 4 = 124 params > 100
        let out = generate(
            &component,
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        )
        .into_single();
        eprintln!("--- generated ---\n{out}\n--- end ---");
        assert!(out.contains("proc create"));
        // Split-node procs use fully-qualified names now — they live
        // outside the namespace-block so they can be extracted into
        // sibling `.htcl` files by `split_into_files`.
        assert!(out.contains("proc wide::big_one"));
        assert!(out.contains("proc wide::big_two"));
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
        )
        .into_single();
        // Sub-procs are pure value-constructors — no `cell:`
        // first arg, no bd switch — and return the node's typed
        // newtype instead of `bd_cell`. Assert the shape.
        // Fully-qualified name shape after the split-file refactor.
        assert!(
            out.contains("proc wide::big_one {\n  ## Configuration value"),
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
        )
        .into_single();
        // None of the tiny prefix groups becomes its own proc...
        for name in ["proc tiny_a ", "proc tiny_b ", "proc stray "] {
            assert!(!out.contains(name), "unexpected {name} in:\n{out}");
        }
        // ...and the params instead appear as args on the top
        // proc's configure counterpart. Post-refactor `create` has
        // just `-name`/`-bd`/`-config`; the full documented kwarg
        // pile lives on `configure`. Slice the configure range so
        // we can search its args without accidentally matching arg
        // names embedded in one of the value-constructor sub-procs'
        // bodies.
        let cfg_start =
            out.find("proc configure {").expect("no proc configure");
        let cfg_body = &out[cfg_start..];
        let cfg_end = cfg_body
            .find("\n  }\n")
            .map(|e| e + 5)
            .unwrap_or(cfg_body.len());
        let cfg_range = &cfg_body[..cfg_end];
        for arg in ["tiny_a_one", "tiny_b_one", "tiny_c_one", "stray_thing"] {
            assert!(
                cfg_range.contains(arg),
                "{arg} missing from configure proc: {cfg_range}"
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
        )
        .into_single();
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
        )
        .into_single();
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
            )
            .into_single(),
            generate(
                &mk_split_component(6),
                &Default::default(),
                &::std::collections::HashMap::new(),
                &GenerateOptions {
                    split_threshold: 5,
                    ..GenerateOptions::default()
                },
            )
            .into_single(),
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
        )
        .into_single();
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
        )
        .into_single();
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
        )
        .into_single();
        // Post-refactor these lappend lines live inside `configure`,
        // not `create`. `create`'s body just unwraps `-config` and
        // splats the resulting dict via `set_property -dict`.
        assert!(out.contains("CONFIG.BUS_WIDTH $bus_width"), "{out}");
        assert!(out.contains("CONFIG.MODE $mode"), "{out}");
    }

    /// `configure`'s body is pure dict assembly + a Config wrap.
    /// Any of the side-effecting Vivado calls appearing inside its
    /// body would mean the seam wasn't cleanly cut.
    #[test]
    fn configure_returns_typed_config_no_side_effects() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        )
        .into_single();
        let cfg_start = out.find("proc configure {").expect("no configure");
        let cfg_body = &out[cfg_start..];
        let cfg_end = cfg_body
            .find("\n  }\n")
            .map(|e| e + 5)
            .unwrap_or(cfg_body.len());
        let cfg_range = &cfg_body[..cfg_end];
        for forbidden in ["create_bd_cell", "create_ip", "set_property"] {
            assert!(
                !cfg_range.contains(forbidden),
                "configure body should not contain `{forbidden}`:\n{cfg_range}"
            );
        }
        // But it SHOULD wrap the assembled dict as a Config value.
        assert!(cfg_range.contains("demo::Config::from"), "{cfg_range}");
    }

    /// `create`'s arg surface is exactly `-name`, `-bd`, `-config`
    /// under the post-refactor design. No documented-kwarg pile.
    #[test]
    fn create_takes_only_name_bd_config() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        )
        .into_single();
        let create_start = out.find("proc create {").expect("no create");
        // Argspec ends at `} bd_cell {`.
        let create_body = &out[create_start..];
        let argspec_end = create_body
            .find("} bd_cell {")
            .expect("create should return bd_cell");
        let argspec = &create_body[..argspec_end];
        for expected in ["name", "bd", "config: demo::Config"] {
            assert!(
                argspec.contains(expected),
                "expected `{expected}` in create argspec:\n{argspec}"
            );
        }
        // No leftover typed IP-param kwargs on create.
        for forbidden in ["bus_width: int", "@enum(FAST, SLOW)"] {
            assert!(
                !argspec.contains(forbidden),
                "IP-param kwarg `{forbidden}` leaked onto create:\n{argspec}"
            );
        }
    }

    /// `create`'s body unwraps `$config` through the Config newtype
    /// and applies via `set_property -dict` in both `-bd 1` and
    /// `-bd 0` branches.
    #[test]
    fn create_body_unwraps_config_and_applies() {
        let out = generate(
            &mk_component(),
            &Default::default(),
            &::std::collections::HashMap::new(),
            &GenerateOptions::default(),
        )
        .into_single();
        let create_start = out.find("proc create {").expect("no create");
        let create_range = &out[create_start..];
        // Unwrap step.
        assert!(
            create_range.contains("demo::Config::to -v $config"),
            "create should unwrap $config via Config::to:\n{create_range}"
        );
        // Guard for the empty-Config default (bracket-expr @default
        // fallback pattern documented in build_single_create_body).
        assert!(
            create_range.contains("demo::Config::empty"),
            "create should coerce empty-string default to Config::empty:\n{create_range}"
        );
        // Both bd branches.
        assert!(
            create_range.contains("set_property -dict $_dict -objects $cell")
        );
        assert!(create_range
            .contains("set_property -dict $_dict -objects [get_ips $name]"));
    }

    // ------------------------------------------------------------------
    // is_properties_shaped_param — Xilinx vendor-tag routing.
    // ------------------------------------------------------------------

    use ipxact::{ParameterInfo, VendorExtensions};

    fn mk_param(default: &str, tcldict: bool) -> Parameter {
        Parameter {
            name: "X".into(),
            value: ParamValue {
                text: default.into(),
                ..Default::default()
            },
            vendor_extensions: if tcldict {
                Some(VendorExtensions {
                    xilinx_parameter_info: Some(ParameterInfo {
                        parameter_type: vec!["tcldict".into()],
                    }),
                })
            } else {
                None
            },
            ..Default::default()
        }
    }

    #[test]
    fn tcldict_tag_overrides_scalar_default() {
        // The `INTF0_GT_SETTINGS` shape — vendor tag says
        // `tcldict`, default is Xilinx's `0` sentinel that reads
        // as scalar. The vendor tag must win.
        let p = mk_param("0", true);
        assert!(is_properties_shaped_param(&p));
    }

    #[test]
    fn structural_shape_still_wins_without_tag() {
        // The `INTF0_LR0_SETTINGS` shape — no vendor tag but the
        // default's paired-list shape gives it away.
        let p = mk_param("NA NA", false);
        assert!(is_properties_shaped_param(&p));
    }

    #[test]
    fn neither_tag_nor_shape_stays_scalar() {
        // Plain scalar param. Was scalar before this change,
        // stays scalar after.
        let p = mk_param("0", false);
        assert!(!is_properties_shaped_param(&p));
    }
}
