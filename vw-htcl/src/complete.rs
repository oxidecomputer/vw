// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Code completion for htcl.
//!
//! Two contexts, both keyed off [`cmdline::analyze`]:
//!
//! - **Command position** (typing the first word) → the names of
//!   `proc`s declared in the document.
//! - **Argument position** (after a known proc's name) → that proc's
//!   `-flag` arguments, minus any already supplied.
//!
//! Pure analysis: returns structured [`Completion`]s referencing the
//! document; the LSP backend maps them to `CompletionItem`s and the
//! REPL will render them its own way. Vivado builtins are not offered
//! yet — that needs the UG835 command database (project-plan Phase 8).

use std::fmt::Write;

use crate::ast::{
    AttributeValue, CommandKind, Document, ProcArg, ProcSignature, Stmt,
};
use crate::cmdline::{self, CmdLine};
use crate::span::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// A `proc` name in command position.
    Proc,
    /// A `-flag` keyword argument of a known proc.
    Flag,
    /// A value from a flag's `@enum(...)` constraint.
    EnumValue,
}

#[derive(Clone, Debug)]
pub struct Completion {
    /// Text shown in the list and inserted (`greet`, `-name`).
    pub label: String,
    pub kind: CompletionKind,
    /// Short, single-line annotation shown inline next to the label.
    pub detail: Option<String>,
    /// Longer markdown shown in the item's documentation popup.
    pub documentation: Option<String>,
    /// Source range the inserted text replaces (the partial word, or a
    /// zero-width insertion point between words).
    pub replace: Span,
}

struct ProcInfo<'a> {
    /// Qualified name as it would be called — bare proc name for
    /// top-level declarations, `<ns>::<name>` for procs declared
    /// inside `namespace eval` blocks.
    name: String,
    doc_comments: &'a [String],
    signature: Option<&'a ProcSignature>,
}

/// Completions available at `offset`.
pub fn complete_at(
    document: &Document,
    source: &str,
    offset: u32,
) -> Vec<Completion> {
    // Inside a proc's argument-declaration braces, command/flag
    // completion is meaningless (attribute completion will live here
    // later). Stay quiet rather than offer nonsense.
    if in_proc_args(&document.stmts, offset) {
        return Vec::new();
    }

    let line = cmdline::analyze(source, offset);
    let procs = collect_procs(document);

    if line.in_command_position() {
        return complete_proc_names(&procs, &line);
    }

    // If the previous complete word is a `-flag`, the cursor is in
    // value position — even if the partial is empty (user just hit
    // space after the flag). Offer the flag's `@enum(...)` choices
    // when it has them; otherwise stay silent so the user can type a
    // free-form value (string, int, etc.) without a flag list popping
    // up in front of it.
    //
    // If the partial *starts with* `-` we step back into flag-typing
    // mode regardless — the user is clearly typing a new flag.
    let last_word_is_flag = line.words.len() >= 2
        && line.words.last().is_some_and(|w| w.starts_with('-'));
    if last_word_is_flag && !line.partial.starts_with('-') {
        return complete_enum_values(&procs, &line);
    }

    complete_flags(&procs, &line)
}

/// `@enum(…)` value completions when the cursor sits in value
/// position. Returns empty when the flag has no `@enum` (so the
/// caller can fall back to flag completion).
fn complete_enum_values(
    procs: &[ProcInfo<'_>],
    line: &CmdLine<'_>,
) -> Vec<Completion> {
    let Some(name) = line.command_name() else {
        return Vec::new();
    };
    let Some(proc) = procs.iter().find(|p| p.name == name) else {
        return Vec::new();
    };
    let Some(sig) = proc.signature else {
        return Vec::new();
    };
    // The flag whose value we're completing is the last word on the
    // line; if it isn't a `-flag`, the user is between options and
    // there's nothing to enum-complete.
    let Some(last) = line.words.last() else {
        return Vec::new();
    };
    let Some(flag) = last.strip_prefix('-') else {
        return Vec::new();
    };
    let Some(arg) = sig.find(flag) else {
        return Vec::new();
    };
    let Some(enum_attr) = arg.attribute("enum") else {
        return Vec::new();
    };

    let needle = line.partial;
    enum_attr
        .values
        .iter()
        .filter_map(|v| {
            let raw = enum_value_text(v);
            // Filter by either the bare or quoted form so a user typing
            // `Mas` matches the value `Master Mode` whose insert form
            // is `"Master Mode"`.
            if !raw.starts_with(needle)
                && !quote_for_completion(&raw).starts_with(needle)
            {
                return None;
            }
            let insert = quote_for_completion(&raw);
            Some(Completion {
                label: insert.clone(),
                kind: CompletionKind::EnumValue,
                detail: Some(format!("value for -{}", arg.name)),
                documentation: crate::doc::brief(&arg.doc_comments),
                replace: line.partial_span,
            })
        })
        .collect()
}

fn enum_value_text(v: &AttributeValue) -> String {
    match v {
        AttributeValue::Integer { value, .. } => value.to_string(),
        AttributeValue::Ident { value, .. }
        | AttributeValue::String { value, .. } => value.clone(),
    }
}

/// Quote `s` for use as a value on a call site if it can't ride as a
/// bare word. Mirrors the rule [`crate::emit::Word::lit`] uses: bare
/// when safe, double-quoted with `\`/`"` escapes otherwise.
fn quote_for_completion(s: &str) -> String {
    let needs = s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '"' | '\\' | '[' | ']' | '{' | '}' | '$' | '#'
                )
        });
    if needs {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn complete_proc_names(
    procs: &[ProcInfo<'_>],
    line: &CmdLine<'_>,
) -> Vec<Completion> {
    procs
        .iter()
        .filter(|p| p.name.starts_with(line.partial))
        .map(|p| Completion {
            label: p.name.to_string(),
            kind: CompletionKind::Proc,
            detail: first_doc_line(p.doc_comments),
            documentation: proc_documentation(p),
            replace: line.partial_span,
        })
        .collect()
}

fn complete_flags(
    procs: &[ProcInfo<'_>],
    line: &CmdLine<'_>,
) -> Vec<Completion> {
    let Some(name) = line.command_name() else {
        return Vec::new();
    };
    let Some(proc) = procs.iter().find(|p| p.name == name) else {
        return Vec::new();
    };
    let Some(sig) = proc.signature else {
        return Vec::new();
    };

    let used: Vec<&str> = line.used_flags().collect();
    let needle = line.partial;
    let bare_needle = needle.trim_start_matches('-');

    sig.args
        .iter()
        .filter_map(|arg| {
            let label = format!("-{}", arg.name);
            // Don't re-offer a flag already on the line, unless it's
            // the very word being typed.
            if used.iter().any(|u| *u == label) && needle != label {
                return None;
            }
            // Match either the dashed form (`-na`) or the bare name
            // (`na`); an empty needle matches everything.
            if !label.starts_with(needle) && !arg.name.starts_with(bare_needle)
            {
                return None;
            }
            Some(Completion {
                label,
                kind: CompletionKind::Flag,
                detail: first_doc_line(&arg.doc_comments),
                documentation: Some(arg_documentation(arg)),
                replace: line.partial_span,
            })
        })
        .collect()
}

fn collect_procs(document: &Document) -> Vec<ProcInfo<'_>> {
    let mut out = Vec::new();
    collect_procs_in(&document.stmts, "", &mut out);
    out
}

fn collect_procs_in<'a>(
    stmts: &'a [Stmt],
    prefix: &str,
    out: &mut Vec<ProcInfo<'a>>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(name) = proc.name.as_deref() else { continue };
                let qualified = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}::{name}")
                };
                out.push(ProcInfo {
                    name: qualified,
                    doc_comments: &cmd.doc_comments,
                    signature: proc.signature.as_ref(),
                });
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else { continue };
                let nested = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}::{name}")
                };
                collect_procs_in(&ns.body, &nested, out);
            }
            _ => {}
        }
    }
}

/// True if `offset` is inside any proc's argument-declaration braces,
/// at any nesting depth.
fn in_proc_args(stmts: &[Stmt], offset: u32) -> bool {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(proc) = &cmd.kind else {
            continue;
        };
        if proc.args_span.contains(offset) {
            return true;
        }
        if in_proc_args(&proc.body, offset) {
            return true;
        }
    }
    false
}

fn first_doc_line(docs: &[String]) -> Option<String> {
    crate::doc::brief(docs)
}

fn proc_documentation(p: &ProcInfo<'_>) -> Option<String> {
    // Use `extended` (body only) here because the call site populates
    // `CompletionItem::detail` with the brief sentence separately —
    // shipping the full reflowed text would duplicate that sentence at
    // the top of every popup.
    let mut out = String::new();
    if let Some(ext) = crate::doc::extended(p.doc_comments) {
        out.push_str(&ext);
    }
    if let Some(sig) = p.signature {
        if !sig.args.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            for arg in &sig.args {
                write!(out, "- `-{}`", arg.name).unwrap();
                if let Some(d) = crate::doc::brief(&arg.doc_comments) {
                    write!(out, " — {d}").unwrap();
                }
                out.push('\n');
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn arg_documentation(arg: &ProcArg) -> String {
    // `extended` only — the brief sentence is handled by the caller's
    // `detail` field; see `proc_documentation` for the rationale.
    let mut out = String::new();
    if let Some(ext) = crate::doc::extended(&arg.doc_comments) {
        out.push_str(&ext);
    }
    for attr in &arg.attributes {
        if !out.is_empty() {
            out.push('\n');
        }
        write!(out, "- `@{}`", attr.name).unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// Build `src` plus a cursor at the `|` marker, returning the
    /// marker-free source and the byte offset of the cursor.
    fn cursor(src_with_marker: &str) -> (String, u32) {
        let offset = src_with_marker.find('|').expect("no cursor marker");
        let src = src_with_marker.replacen('|', "", 1);
        (src, offset as u32)
    }

    fn labels(src_with_marker: &str) -> Vec<String> {
        let (src, off) = cursor(src_with_marker);
        let parsed = parse(&src);
        complete_at(&parsed.document, &src, off)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    #[test]
    fn proc_names_in_command_position() {
        let src = "\
proc greet {} { }\n\
proc grumble {} { }\n\
gr|\n";
        let mut got = labels(src);
        got.sort();
        assert_eq!(got, vec!["greet", "grumble"]);
    }

    #[test]
    fn proc_names_filtered_by_prefix() {
        let src = "\
proc greet {} { }\n\
proc grumble {} { }\n\
gree|\n";
        assert_eq!(labels(src), vec!["greet"]);
    }

    #[test]
    fn flags_in_argument_position() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg |\n";
        let mut got = labels(src);
        got.sort();
        assert_eq!(got, vec!["-depth", "-width"]);
    }

    #[test]
    fn flags_filtered_by_partial() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -w|\n";
        assert_eq!(labels(src), vec!["-width"]);
    }

    #[test]
    fn already_used_flag_is_not_reoffered() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -width 8 |\n";
        assert_eq!(labels(src), vec!["-depth"]);
    }

    #[test]
    fn completes_call_inside_proc_body() {
        let src = "\
proc helper {} { }\n\
proc outer {} {\n  hel|\n}\n";
        assert_eq!(labels(src), vec!["helper"]);
    }

    #[test]
    fn no_completion_inside_arg_decls() {
        let src = "\
proc greet {} { }\n\
proc cfg {\n  wi|\n} { }\n";
        assert!(labels(src).is_empty());
    }

    #[test]
    fn enum_value_position_offers_choices() {
        // `2.5_GT/s` etc. aren't valid `attribute_value_ident`s, so
        // the IP generator quotes them in `@enum(…)` — the proc-args
        // grammar parses them as strings. The completion labels come
        // back bare here because no whitespace requires re-quoting.
        let src = "\
proc cfg {\n  @enum(\"2.5_GT/s\", \"5.0_GT/s\", \"8.0_GT/s\") max_link_speed\n} { }\n\
cfg -max_link_speed |\n";
        let mut got = labels(src);
        got.sort();
        assert_eq!(got, vec!["2.5_GT/s", "5.0_GT/s", "8.0_GT/s"]);
    }

    #[test]
    fn enum_values_filter_by_partial() {
        let src = "\
proc cfg {\n  @enum(\"2.5_GT/s\", \"5.0_GT/s\", \"8.0_GT/s\") max_link_speed\n} { }\n\
cfg -max_link_speed 5|\n";
        assert_eq!(labels(src), vec!["5.0_GT/s"]);
    }

    #[test]
    fn enum_completion_kind_marks_items() {
        let src = "\
proc cfg {\n  @enum(target, controller) kind\n} { }\n\
cfg -kind |\n";
        let (s, off) = cursor(src);
        let parsed = parse(&s);
        let items = complete_at(&parsed.document, &s, off);
        assert!(items.iter().all(|c| c.kind == CompletionKind::EnumValue));
    }

    #[test]
    fn enum_value_with_spaces_gets_quoted() {
        let src = "\
proc cfg {\n  @enum(\"Master Mode\", \"Slave Mode\") role\n} { }\n\
cfg -role |\n";
        let mut got = labels(src);
        got.sort();
        assert_eq!(got, vec!["\"Master Mode\"", "\"Slave Mode\""]);
    }

    #[test]
    fn flag_without_enum_offers_no_completions_at_value_position() {
        // For a flag with no `@enum` the user is expected to type a
        // free-form value. Popping a flag list there is wrong; it
        // gets in the way of the actual value the user is typing.
        let src = "\
proc cfg {\n  @default(0) width\n  @default(0) depth\n} { }\n\
cfg -width |\n";
        assert!(labels(src).is_empty(), "{:?}", labels(src));
    }

    #[test]
    fn flag_completion_returns_after_value_is_typed() {
        // After the value is typed, the cursor is between args again
        // — show the next flags.
        let src = "\
proc cfg {\n  @default(0) width\n  @default(0) depth\n} { }\n\
cfg -width 8 |\n";
        let mut got = labels(src);
        got.sort();
        assert_eq!(got, vec!["-depth"]);
    }

    #[test]
    fn dash_partial_keeps_flag_completion() {
        // Typing `-` after a complete flag should still mean "new
        // flag," not "enum value."
        let src = "\
proc cfg {\n  @enum(a, b) mode\n  @default(0) width\n} { }\n\
cfg -mode -|\n";
        let got = labels(src);
        assert!(got.contains(&"-width".to_string()), "{got:?}");
        assert!(!got.contains(&"a".to_string()), "{got:?}");
    }

    #[test]
    fn unknown_command_offers_no_flags() {
        let src = "puts |\n";
        assert!(labels(src).is_empty());
    }

    #[test]
    fn flag_completion_carries_doc_and_detail() {
        // Multi-sentence doc: the brief sentence goes in `detail`,
        // the rest goes in `documentation`. They must NOT overlap —
        // an LSP client renders both, and a repeated leading
        // sentence reads as a duplicate to the user.
        let src = "\
proc cfg {
  ## Bus width in bits. Must be a power of two.
  @default(8) width
} { }
cfg |
";
        let (s, off) = cursor(src);
        let parsed = parse(&s);
        let items = complete_at(&parsed.document, &s, off);
        let item = items.iter().find(|c| c.label == "-width").unwrap();
        assert_eq!(item.kind, CompletionKind::Flag);
        assert_eq!(item.detail.as_deref(), Some("Bus width in bits."));
        let doc = item.documentation.as_deref().unwrap();
        assert!(doc.contains("Must be a power of two."), "{doc}");
        assert!(
            !doc.contains("Bus width in bits."),
            "documentation should not repeat the brief: {doc}"
        );
        assert!(doc.contains("@default"), "{doc}");
    }
}
