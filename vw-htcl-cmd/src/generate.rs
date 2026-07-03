// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Emit an htcl wrapper proc for a parsed [`ManPage`].
//!
//! Shape, for a command `add_files`:
//!
//! ```htcl
//! # Preserve the underlying Vivado command so the wrapper can forward
//! # to it after shadowing the global name.
//! if {[info commands __viv_add_files] eq "" && [info commands add_files] ne ""} {
//!   rename add_files __viv_add_files
//! }
//!
//! ## Adds one or more source files ...
//! proc add_files {
//!   ## (Optional) The fileset to add to.
//!   @default("") fileset
//!   ## (Optional) Do not recurse ...
//!   @enum(0, 1) @default(0) norecurse
//!   ## Positional operands ...
//!   @default("") operands
//! } {
//!   set cmd [list __viv_add_files]
//!   if {$fileset ne ""} { lappend cmd -fileset $fileset }
//!   if {$norecurse} { lappend cmd -norecurse }
//!   if {$operands ne ""} { lappend cmd {*}$operands }
//!   return [{*}$cmd]
//! }
//! ```
//!
//! The wrapper keeps the command's natural name and shadows the
//! builtin; a guarded `rename` stashes the original under
//! `<prefix><name>` so the body forwards to it without recursing. All
//! arguments are addressed by keyword (`-fileset value`); boolean flags
//! take a `0`/`1` value at the htcl layer and lower to flag
//! presence/absence on the Vivado command line.

use std::fmt::Write;

use vw_htcl::emit::{Command, Doc, Item, Word};

use crate::constraints::{ArgOverride, ConstraintsTable};
use crate::model::{ArgKind, Argument, ManPage};

/// Arg names whose values are Vivado typed `Tcl_Obj` handles —
/// `bd_cell`, `bd_pin`, etc. — and therefore must be passed to the
/// underlying command **directly** (`-flag $value`) rather than
/// through `[list]` or `lappend`. List construction shimmers Tcl's
/// internal typed representation away, leaving the handle as a
/// plain path string; downstream code paths in Vivado (notably
/// `set_property -objects`) reject the stringified path with
/// `[Common 17-161] Invalid option value`.
///
/// Curated list of the obvious cases. Per-arg override via
/// `cmd-constraints.toml`'s `typed = true|false` covers the long
/// tail.
const TYPED_ARG_NAMES: &[&str] = &[
    "object",
    "objects",
    "of_objects",
    "cell",
    "cells",
    "pin",
    "pins",
    "port",
    "ports",
    "intf_pin",
    "intf_pins",
    "intf_port",
    "intf_ports",
    "net",
    "nets",
    "intf_net",
    "intf_nets",
];

fn is_typed_arg(name: &str, override_: Option<bool>) -> bool {
    match override_ {
        Some(t) => t,
        None => TYPED_ARG_NAMES.contains(&name),
    }
}

/// Map a typed-arg name to its concrete `TypeExpr` text, when
/// known. Drives the `name: TYPE` annotation emitted in the
/// generated proc args. Plural names (`cells`, `pins`) map to
/// `list<bd_*>`; singulars (`cell`, `pin`) map to the bd_* type
/// directly. Generic catch-alls (`object`, `objects`,
/// `of_objects`) can refer to any Vivado handle class, so we
/// leave them untyped at this layer — the type system doesn't
/// have unions in v1.
///
/// Returning `None` means "the arg is typed (don't list-wrap in
/// the body) but we don't have a precise type expression for
/// it" — the generator emits the arg without an annotation.
fn typed_arg_type(name: &str) -> Option<&'static str> {
    match name {
        "cell" => Some("bd_cell"),
        "cells" => Some("list<bd_cell>"),
        "pin" => Some("bd_pin"),
        "pins" => Some("list<bd_pin>"),
        "port" => Some("bd_port"),
        "ports" => Some("list<bd_port>"),
        "net" => Some("bd_net"),
        "nets" => Some("list<bd_net>"),
        "intf_pin" => Some("bd_intf_pin"),
        "intf_pins" => Some("list<bd_intf_pin>"),
        "intf_port" => Some("bd_intf_port"),
        "intf_ports" => Some("list<bd_intf_port>"),
        "intf_net" => Some("bd_intf_net"),
        "intf_nets" => Some("list<bd_intf_net>"),
        // object / objects / of_objects: any handle class — no
        // precise type until we have unions.
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct GenerateOptions {
    /// Prefix for the stashed original command (`rename add_files
    /// __viv_add_files`). Kept for backwards compatibility — the
    /// lowering pass now generates the rename plumbing, so this
    /// field has no effect.
    pub rename_prefix: String,
    /// Emit each command's `See Also` list as a doc-comment footer.
    pub include_see_also: bool,
    /// Per-command signature augmentations loaded from
    /// `cmd-constraints.toml`. The generator merges these onto the
    /// man-page-derived shape so wrapper authors can declare
    /// mutually-exclusive call modes, value-taking flags
    /// misclassified by the man page, etc., without hand-editing
    /// the generated files.
    pub constraints: ConstraintsTable,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            rename_prefix: "__viv_".to_string(),
            include_see_also: true,
            constraints: ConstraintsTable::empty(),
        }
    }
}

/// Generate the htcl wrapper text for `page`.
pub fn generate(page: &ManPage, opts: &GenerateOptions) -> String {
    let cmd = &page.name;
    // Wrapper body forwards to the underlying Vivado proc via
    // `extern::` (which the lowering rewrites to the bare native
    // name). The wrapper itself lives inside `namespace eval
    // vivado { ... }` so it doesn't shadow the global name the
    // body is forwarding to — that's what frees Vivado's own
    // internal Tcl from accidentally hitting our typed wrappers
    // when it calls a sibling builtin.
    let forwarded = format!("extern::{}", page.name);

    let overrides = opts.constraints.for_command(&page.name);
    let effective = effective_args(page, overrides);

    let mut out = String::new();
    writeln!(
        out,
        "# Generated by `vw htcl-cmd generate` from the Vivado command \
         reference."
    )
    .unwrap();
    writeln!(out, "# Do not edit by hand.").unwrap();
    writeln!(out).unwrap();

    // Wrappers live in `vivado_cmd::`, NOT `vivado::`. Vivado has
    // its own internal `::vivado` namespace and code paths that
    // behave differently depending on the calling namespace —
    // notably `set_property -dict -objects ...` rejects valid cell
    // handles when invoked from inside `::vivado`. Picking a name
    // Vivado doesn't use means our wrapper bodies never collide
    // with Vivado-internal namespace state.
    writeln!(out, "namespace eval vivado_cmd {{").unwrap();
    writeln!(out).unwrap();

    // Proc doc comment: the command Description, then a See-Also footer.
    emit_proc_doc(&mut out, page, opts);

    // Proc args (structured) and body (compact Tcl).
    let args = build_args(page, &effective);
    let body = build_body(&forwarded, &effective);
    // Resolve return type. Priority:
    //   1. Explicit override in `cmd-constraints.toml`.
    //   2. The page's `Returns:` section, if present.
    //   3. Phrases in the `Description:` section — Vivado very
    //      rarely uses a dedicated Returns: header, so this is
    //      actually the common path. The phrase table is the same
    //      either way.
    let return_type = overrides
        .and_then(|o| o.returns.as_deref())
        .map(String::from)
        .or_else(|| infer_return_type(page.returns.as_deref()))
        .or_else(|| infer_return_type(Some(page.description.as_slice())));
    emit_proc(&mut out, cmd, &args, return_type.as_deref(), &body);

    writeln!(out).unwrap();
    writeln!(out, "}}").unwrap();

    // Trim trailing whitespace line-by-line (empty doc comments emit a
    // trailing space) and guarantee a single trailing newline.
    let mut cleaned: String = out
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    cleaned.push('\n');
    cleaned
}

/// Write the proc-level doc comments above the `proc` line so they
/// attach to it. The output is structured as
///
/// ```text
/// ## <summary sentence>
/// ##
/// ## <body paragraph 1>
/// ##
/// ## <body paragraph 2>
/// ```
///
/// where the summary is the first sentence of the source description
/// (LSP-clients use it for inline annotations like
/// `CompletionItem::detail`) and the body is everything after,
/// rendered as separate paragraphs. The body paragraphs are
/// re-wrapped at ~78 columns so the on-disk file stays readable
/// without preserving the man-page's source wrap.
fn emit_proc_doc(out: &mut String, page: &ManPage, opts: &GenerateOptions) {
    let raw: Vec<String> =
        page.description.iter().map(|l| sanitize_doc(l)).collect();
    let summary = vw_htcl::doc::brief(&raw);
    let extended = vw_htcl::doc::extended(&raw);

    match summary {
        None => {
            writeln!(out, "## Wrapper for the Vivado `{}` command.", page.name)
                .unwrap();
        }
        Some(s) => {
            emit_paragraph_lines(out, &s, "## ", 78);
        }
    }
    if let Some(body) = extended {
        for paragraph in body.split("\n\n") {
            writeln!(out, "##").unwrap();
            emit_paragraph_lines(out, paragraph, "## ", 78);
        }
    }

    if opts.include_see_also && !page.see_also.is_empty() {
        writeln!(out, "##").unwrap();
        writeln!(out, "## See also: {}", page.see_also.join(", ")).unwrap();
    }
}

fn emit_paragraph_lines(
    out: &mut String,
    text: &str,
    prefix: &str,
    width: usize,
) {
    let body_width = width.saturating_sub(prefix.len());
    for line in vw_htcl::doc::wrap_paragraph(text, body_width) {
        writeln!(out, "{prefix}{line}").unwrap();
    }
}

/// One argument plus whatever overrides from `cmd-constraints.toml`
/// apply to it. `default`, `enum_values`, `one_of`, `requires`,
/// `conflicts` are the *final* values the wrapper should emit;
/// constraint resolution has already happened.
///
/// `kind` is derived: a constraint that clears the enum and adds a
/// default to a man-page-Boolean arg flips it to value-taking, so
/// the body-emitter forwards `-flag $value` instead of `if {$f} {
/// lappend cmd -flag }`.
#[derive(Clone, Debug)]
struct EffectiveArg {
    ident: String,
    flag: Option<String>,
    kind: ArgKind,
    /// `None` → no default (required); `Some(text)` → emit `@default(text)`.
    default: Option<String>,
    /// `None` → no enum; `Some(vec)` → emit `@enum(...)`.
    enum_values: Option<Vec<String>>,
    one_of: Vec<String>,
    requires: Vec<String>,
    conflicts: Vec<String>,
    description: Vec<String>,
    /// True when this arg carries a Vivado typed `Tcl_Obj` handle
    /// — body emission passes it directly (`-flag $value`) rather
    /// than threading it through a list. See [`TYPED_ARG_NAMES`].
    typed: bool,
    /// The arg's declared type expression, if known. Emitted in
    /// the proc args as `name: TYPE`. Set from
    /// [`typed_arg_type`] for the typed-handle allowlist; an
    /// explicit per-arg `type = "..."` override in
    /// `cmd-constraints.toml` wins over the inferred value.
    arg_type: Option<String>,
}

fn effective_args(
    page: &ManPage,
    overrides: Option<&crate::constraints::CommandOverride>,
) -> Vec<EffectiveArg> {
    page.arguments
        .iter()
        .map(|arg| {
            effective_arg(arg, overrides.and_then(|o| o.args.get(&arg.ident)))
        })
        .collect()
}

fn effective_arg(arg: &Argument, over: Option<&ArgOverride>) -> EffectiveArg {
    let empty = ArgOverride::default();
    let over = over.unwrap_or(&empty);

    // Default value: explicit override wins; else man-page heuristic.
    let mut default: Option<String> = match &arg.kind {
        ArgKind::Boolean => Some("0".to_string()),
        ArgKind::Value | ArgKind::Positional => {
            (!arg.required).then(|| "".to_string())
        }
    };
    if let Some(d) = over.default.as_deref() {
        default = Some(d.to_string());
    }

    // Enum: man-page-derived for booleans; constraints can clear or
    // replace.
    let mut enum_values: Option<Vec<String>> = match &arg.kind {
        ArgKind::Boolean => Some(vec!["0".to_string(), "1".to_string()]),
        _ => None,
    };
    if over.clear_enum {
        enum_values = None;
    }
    if let Some(v) = &over.enum_ {
        enum_values = Some(v.clone());
    }

    // Kind: an arg with no `@enum` (cleared) and a string-typed
    // default acts like a value-taking flag — body-emit should
    // forward `-flag $value`, not `if {$flag} { ... }`. This is the
    // exact shape `set_property -dict` needs.
    let kind = if matches!(arg.kind, ArgKind::Boolean) && enum_values.is_none()
    {
        if arg.flag.is_some() {
            ArgKind::Value
        } else {
            ArgKind::Positional
        }
    } else {
        arg.kind
    };

    let typed = is_typed_arg(&arg.ident, over.typed);
    let arg_type = over
        .arg_type
        .clone()
        .or_else(|| typed_arg_type(&arg.ident).map(String::from));

    EffectiveArg {
        ident: arg.ident.clone(),
        flag: arg.flag.clone(),
        kind,
        default,
        enum_values,
        one_of: over.one_of.clone(),
        requires: over.requires.clone(),
        conflicts: over.conflicts.clone(),
        description: arg.description.clone(),
        typed,
        arg_type,
    }
}

/// Build the structured arg list as an emit [`Doc`]: per-argument doc
/// comments followed by an `@attr… ident` declaration. The doc
/// comments follow the same `summary, blank, body` shape the
/// proc-level docs use, so LSP clients can split brief/detail from
/// extended documentation consistently.
fn build_args(_page: &ManPage, effective: &[EffectiveArg]) -> Doc {
    let mut doc = Doc::new();
    for (i, arg) in effective.iter().enumerate() {
        if i > 0 {
            doc.push(Item::Blank);
        }
        let raw: Vec<String> =
            arg.description.iter().map(|l| sanitize_doc(l)).collect();
        let summary = vw_htcl::doc::brief(&raw);
        let extended = vw_htcl::doc::extended(&raw);

        let body_width = 76usize;
        if let Some(s) = summary.as_deref() {
            for line in vw_htcl::doc::wrap_paragraph(s, body_width) {
                doc.push(Item::DocComment(line));
            }
        }
        if let Some(body) = extended {
            for paragraph in body.split("\n\n") {
                doc.push(Item::DocComment(String::new()));
                for line in vw_htcl::doc::wrap_paragraph(paragraph, body_width)
                {
                    doc.push(Item::DocComment(line));
                }
            }
        }

        doc.push(Item::Command(Command {
            doc_comments: Vec::new(),
            words: effective_attr_words(arg),
            body: None,
        }));
    }
    doc
}

/// The attribute words + identifier for one effective argument.
fn effective_attr_words(arg: &EffectiveArg) -> Vec<Word> {
    let mut words = Vec::new();
    if let Some(values) = &arg.enum_values {
        let inner = values
            .iter()
            .map(|v| format_attribute_value(v))
            .collect::<Vec<_>>()
            .join(", ");
        words.push(Word::Raw(format!("@enum({inner})")));
    }
    if let Some(default) = &arg.default {
        words.push(Word::Raw(format!(
            "@default({})",
            format_attribute_value(default)
        )));
    }
    if !arg.one_of.is_empty() {
        words.push(Word::Raw(format!("@one_of({})", arg.one_of.join(", "))));
    }
    if !arg.requires.is_empty() {
        words
            .push(Word::Raw(format!("@requires({})", arg.requires.join(", "))));
    }
    if !arg.conflicts.is_empty() {
        words.push(Word::Raw(format!(
            "@conflicts({})",
            arg.conflicts.join(", ")
        )));
    }
    match arg.arg_type.as_deref() {
        Some(ty) => {
            // Emit `name: TYPE` as two adjacent bare words. The
            // proc-args parser tokenizes `name`, `:`, and TYPE
            // independently — the layout reads as the user would
            // write it.
            words.push(Word::Bare(format!("{}:", arg.ident)));
            words.push(Word::Bare(ty.to_string()));
        }
        None => {
            words.push(Word::Bare(arg.ident.clone()));
        }
    }
    words
}

fn format_attribute_value(s: &str) -> String {
    let is_int = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let is_ident = !s.is_empty()
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_alphabetic() || b == b'_'
            } else {
                b.is_ascii_alphanumeric() || b == b'_'
            }
        });
    if is_int || is_ident {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Build the proc body.
///
/// Args are split into two cohorts:
///
/// - **Non-typed args** (booleans, strings, positionals whose name
///   isn't in the typed-handle allowlist) accumulate into a `flags`
///   list via `lappend`. Each arg's kind drives its emit form —
///   `Boolean` → `if {$x} { lappend flags -flag }`, `Value` →
///   `if {$x ne ""} { lappend flags -flag $x }`, `Positional` →
///   `if {$x ne ""} { lappend flags {*}$x }`. These values are all
///   strings, so the lappend / `{*}`-expansion that follows is
///   safe — string values don't have a typed Tcl_Obj to shimmer.
/// - **Typed-handle args** (`-objects`/`-cell`/etc., per
///   [`TYPED_ARG_NAMES`] or per-arg `typed = true` override) are
///   passed **directly** to the underlying command via
///   `-flag $value`. Putting them through `[list]` or `lappend`
///   would shimmer Vivado's internal typed Tcl_Obj to a string,
///   and downstream code paths like `set_property -objects` reject
///   the stringified path with `[Common 17-161] Invalid option
///   value '...' specified for 'objects'`.
///
/// The invocation site branches on which typed args are present so
/// no typed-arg flag appears in the call when its variable is
/// empty. With N typed args this is 2^N branches; in practice N is
/// 0 or 1 for almost every Vivado command, and never more than 2.
fn build_body(orig: &str, effective: &[EffectiveArg]) -> String {
    let mut body = String::new();

    let non_typed: Vec<&EffectiveArg> =
        effective.iter().filter(|a| !a.typed).collect();
    let typed: Vec<&EffectiveArg> =
        effective.iter().filter(|a| a.typed).collect();

    // Non-typed accumulator. `flags` is a plain Tcl list — only
    // ever contains string values, so list-construction shimmer is
    // a non-issue.
    writeln!(body, "set flags [list]").unwrap();
    for arg in &non_typed {
        let id = &arg.ident;
        let required = arg.default.is_none();
        match arg.kind {
            ArgKind::Boolean => {
                let flag = arg.flag.as_deref().unwrap_or(id);
                writeln!(body, "if {{${id}}} {{ lappend flags -{flag} }}")
                    .unwrap();
            }
            ArgKind::Value => {
                let flag = arg.flag.as_deref().unwrap_or(id);
                if required {
                    writeln!(body, "lappend flags -{flag} ${id}").unwrap();
                } else {
                    writeln!(
                        body,
                        "if {{${id} ne \"\"}} {{ lappend flags -{flag} ${id} }}"
                    )
                    .unwrap();
                }
            }
            ArgKind::Positional => {
                if required {
                    writeln!(body, "lappend flags {{*}}${id}").unwrap();
                } else {
                    writeln!(
                        body,
                        "if {{${id} ne \"\"}} \
                         {{ lappend flags {{*}}${id} }}"
                    )
                    .unwrap();
                }
            }
        }
    }

    // Typed-arg branching. Direct invocation per combination of
    // typed args that are non-empty, so the typed values never
    // touch a Tcl list.
    emit_typed_invocation(&mut body, orig, &typed, 0);

    body
}

/// Emit the typed-arg branch tree. At each level we split on
/// "this typed arg present?" and recurse; at the leaves we emit
/// `return [extern::_vw_global_call extern::<cmd> {*}$flags …]`
/// with whatever subset of typed args was present. The `extern::`
/// prefix on the helper is what the htcl analyzer sees — the
/// lowerer's `rewrite_externs` pass strips it, so the actual
/// runtime call is `::_vw_global_call ::<cmd> …`.
///
/// The `::_vw_global_call` helper (defined in the shim at
/// `::` namespace) is what keeps Vivado's internal Tcl (XDC
/// parsing, OOC synth flows) from resolving unqualified commands
/// to *our* wrappers just because we're currently inside
/// `::vivado_cmd::<wrapper>`. Without a global-namespace call, a
/// wrapper that forwards to `synth_ip` — which itself sources XDC
/// files whose scripts call `create_clock`, `get_ports`, …
/// positionally — would put those XDC scripts in the
/// `::vivado_cmd::` namespace context. The XDC's unqualified
/// `create_clock` would then land on our typed wrapper (which
/// expects `-flag` args) and its kwargs prologue would crash on
/// the positional port name.
///
/// We use the helper rather than `namespace eval ::` /
/// `namespace inscope ::` / `uplevel #0`, all of which internally
/// serialize their args to a script string via `concat` and
/// re-parse them. That round-trip loses Tcl_Obj internal reps —
/// bd_cell handles become plain paths like `/cpm5`, which
/// Vivado's `set_property -objects` then rejects as "Invalid
/// option value". The helper's `{*}$cmd {*}$args` expansion is a
/// direct arg-passing, not a script re-parse, so object identity
/// is preserved end-to-end.
fn emit_typed_invocation(
    body: &mut String,
    orig: &str,
    typed: &[&EffectiveArg],
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    if typed.is_empty() {
        writeln!(
            body,
            "{indent}return [extern::_vw_global_call {orig} {{*}}$flags]"
        )
        .unwrap();
        return;
    }
    if let Some((first, rest)) = typed.split_first() {
        // Required typed args have no `ne ""` guard — they're
        // always passed. Optional typed args branch on presence.
        let id = &first.ident;
        let flag = first.flag.as_deref().unwrap_or(id);
        let required = first.default.is_none();
        if required {
            emit_typed_invocation_with(
                body,
                orig,
                rest,
                &[(*first, flag)],
                depth,
            );
        } else {
            writeln!(body, "{indent}if {{${id} ne \"\"}} {{").unwrap();
            emit_typed_invocation_with(
                body,
                orig,
                rest,
                &[(*first, flag)],
                depth + 1,
            );
            writeln!(body, "{indent}}} else {{").unwrap();
            emit_typed_invocation(body, orig, rest, depth + 1);
            writeln!(body, "{indent}}}").unwrap();
        }
    }
}

/// Inner: we've decided to include `included` typed args; the
/// remaining `rest` still need branching. At the leaf we emit a
/// `return` with `{*}$flags` and each included typed arg as
/// `-flag $var`.
fn emit_typed_invocation_with(
    body: &mut String,
    orig: &str,
    rest: &[&EffectiveArg],
    included: &[(&EffectiveArg, &str)],
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    if rest.is_empty() {
        // Same `extern::_vw_global_call` helper as the no-typed
        // leaf — see [`emit_typed_invocation`] for the rationale
        // (including why the `extern::` prefix is on the helper).
        let mut line = format!(
            "{indent}return [extern::_vw_global_call {orig} {{*}}$flags"
        );
        for (arg, flag) in included {
            match arg.kind {
                ArgKind::Positional => {
                    write!(line, " ${id}", id = arg.ident).unwrap();
                }
                _ => {
                    write!(line, " -{flag} ${id}", id = arg.ident).unwrap();
                }
            }
        }
        line.push(']');
        writeln!(body, "{line}").unwrap();
        return;
    }
    if let Some((first, more)) = rest.split_first() {
        let id = &first.ident;
        let flag = first.flag.as_deref().unwrap_or(id);
        let required = first.default.is_none();
        if required {
            let mut new_included = included.to_vec();
            new_included.push((*first, flag));
            emit_typed_invocation_with(body, orig, more, &new_included, depth);
        } else {
            writeln!(body, "{indent}if {{${id} ne \"\"}} {{").unwrap();
            let mut new_included = included.to_vec();
            new_included.push((*first, flag));
            emit_typed_invocation_with(
                body,
                orig,
                more,
                &new_included,
                depth + 1,
            );
            writeln!(body, "{indent}}} else {{").unwrap();
            emit_typed_invocation_with(body, orig, more, included, depth + 1);
            writeln!(body, "{indent}}}").unwrap();
        }
    }
}

/// Emit `proc <name> { <args> } <type>? { <body> }` with args and
/// body each indented two spaces. When `return_type` is Some, emits
/// it as the 4th htcl word between the args block and the body —
/// brace-wrapping if the type expression contains whitespace so it
/// parses as a single word.
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
            // Wrap with `{ … }` if the type expression contains
            // whitespace (the htcl parser would otherwise see
            // multiple words).
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

/// Infer a return-type annotation from the Vivado man-page's
/// `Returns:` prose. The phrase-table is intentionally small —
/// matches the recurring shapes Vivado uses across hundreds of
/// commands. Unmatched phrasings return `None`; the
/// `cmd-constraints.toml` `returns = "…"` override picks up
/// whatever doesn't match.
///
/// Matched on the joined, lowercased text — Vivado's prose is
/// short (usually one or two lines) so we don't need a real NLP
/// pipeline.
fn infer_return_type(returns: Option<&[String]>) -> Option<String> {
    let lines = returns?;
    let joined = lines.join(" ").to_ascii_lowercase();
    let text = joined.trim();
    if text.is_empty() {
        return None;
    }
    // Order matters: more-specific phrases first. Each entry is
    // (substring needle, type). A real future implementation
    // could swap in regex; substring search is good enough for
    // the v1 phrase set.
    let table: &[(&str, &str)] = &[
        // "nothing" / "Tcl_OK on success" — side-effecting commands.
        ("returns nothing", "unit"),
        ("nothing", "unit"),
        // Lists of typed handles. Order matters within each type
        // family: more-specific "intf" phrasings first so a plain
        // "list of pins" doesn't shadow "list of intf_pins" for a
        // page whose prose mentions both.
        //
        // The `list of <type> objects` variants catch phrasing
        // Vivado uses on the `get_bd_*` pages ("Gets a list of pin
        // objects", "Gets a list of net objects", …). Without them,
        // those procs land untyped and the REPL renders their
        // return value as one wrapped wall of text instead of
        // one-per-line via `list<T>::repr`.
        ("a list of intf_pins", "list<bd_intf_pin>"),
        ("a list of interface pins", "list<bd_intf_pin>"),
        ("list of intf_pin objects", "list<bd_intf_pin>"),
        ("list of interface pin objects", "list<bd_intf_pin>"),
        ("a list of intf_ports", "list<bd_intf_port>"),
        ("a list of interface ports", "list<bd_intf_port>"),
        ("list of intf_port objects", "list<bd_intf_port>"),
        ("list of interface port objects", "list<bd_intf_port>"),
        ("a list of intf_nets", "list<bd_intf_net>"),
        ("a list of interface nets", "list<bd_intf_net>"),
        ("list of intf_net objects", "list<bd_intf_net>"),
        ("list of interface net objects", "list<bd_intf_net>"),
        ("a list of cells", "list<bd_cell>"),
        ("a list of bd_cells", "list<bd_cell>"),
        ("list of cell objects", "list<bd_cell>"),
        ("a list of pins", "list<bd_pin>"),
        ("a list of bd_pins", "list<bd_pin>"),
        ("list of pin objects", "list<bd_pin>"),
        ("a list of ports", "list<bd_port>"),
        ("list of port objects", "list<bd_port>"),
        ("a list of nets", "list<bd_net>"),
        ("list of net objects", "list<bd_net>"),
        // Singular handles.
        ("the cell created", "bd_cell"),
        ("the new cell", "bd_cell"),
        ("the pin created", "bd_pin"),
        ("the port created", "bd_port"),
        ("the net created", "bd_net"),
        // Property values.
        ("the property value", "string"),
        ("the value of the property", "string"),
        ("a list of properties", "list<string>"),
        // Generic strings (catch-all when prose says "string"
        // explicitly).
        ("returns a string", "string"),
    ];
    for (needle, ty) in table {
        if text.contains(needle) {
            return Some((*ty).into());
        }
    }
    None
}

/// Make doc-comment text safe to embed inside the proc arg-list braces.
///
/// The htcl parser captures a proc's arg list as a braced word and
/// brace-matches it raw (only `{`, `}`, `\` are special); a per-arg
/// `##` doc comment with an unbalanced brace or a stray backslash would
/// corrupt that match. Neutralize the three offenders — braces become
/// parentheses, backslashes become slashes — which keeps the prose
/// legible while guaranteeing the generated wrapper parses.
fn sanitize_doc(s: &str) -> String {
    s.replace('\\', "/")
        .replace('{', "(")
        .replace('}', ")")
        .trim_end()
        .to_string()
}
