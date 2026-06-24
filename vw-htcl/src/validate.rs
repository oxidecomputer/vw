// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Signature-aware call-site validation.
//!
//! Builds a {proc_name → ProcSignature} table from the top-level
//! procs in a document, then walks every call site in the same
//! document and checks the keyword arguments against the declared
//! signature. Diagnostics are language-neutral; downstream (the LSP,
//! `vw check`) maps them to the appropriate display form.

use std::collections::HashMap;

use crate::ast::{
    Attribute, AttributeValue, Command, CommandKind, Document, ProcArg,
    ProcSignature, Stmt, Word, WordPart,
};
use crate::span::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
}

pub fn validate(document: &Document, source: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let table = build_signature_table(document, &mut diags);
    validate_stmts(&document.stmts, source, &table, &mut diags);
    diags
}

/// Validate every command in `stmts`, descending into proc bodies so
/// that calls nested inside a proc are checked just like top-level
/// ones. The signature table is document-wide, so a call resolves to
/// its (top-level) proc at any depth.
fn validate_stmts(
    stmts: &[Stmt],
    source: &str,
    table: &HashMap<String, &ProcSignature>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        validate_command(cmd, source, table, diags);
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                validate_stmts(&proc.body, source, table, diags);
            }
            CommandKind::NamespaceEval(ns) => {
                // Calls inside the namespace body are validated the
                // same way; the signature-table is document-wide so
                // a call to `project::set_target_language` from
                // anywhere resolves to the same entry. (Bare,
                // sibling-relative calls inside a namespace body
                // aren't auto-qualified yet — write the qualified
                // name explicitly.)
                validate_stmts(&ns.body, source, table, diags);
            }
            _ => {}
        }
        // Also descend into any `[ … ]` command substitutions on this
        // command's words so calls written inline get validated the
        // same as top-level ones.
        for word in &cmd.words {
            for part in &word.parts {
                if let WordPart::CmdSubst { body, .. } = part {
                    validate_stmts(body, source, table, diags);
                }
            }
        }
    }
}

/// Build a name → signature map from every proc declaration in
/// the document, including those nested inside `namespace eval`
/// blocks (which register under `<ns>::<proc>`, matching Tcl's
/// namespace semantics). Duplicate names raise a diagnostic and the
/// later declaration wins, again matching Tcl (a second `proc`
/// redefines).
pub fn build_signature_table<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> HashMap<String, &'doc ProcSignature> {
    let mut table = HashMap::new();
    collect_signatures(&document.stmts, "", &mut table, diags);
    table
}

fn collect_signatures<'doc>(
    stmts: &'doc [Stmt],
    prefix: &str,
    table: &mut HashMap<String, &'doc ProcSignature>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(name) = proc.name.as_deref() else {
                    continue;
                };
                let Some(sig) = proc.signature.as_ref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                if table.insert(qualified.clone(), sig).is_some() {
                    diags.push(Diagnostic {
                        severity: Severity::Warning,
                        message: format!(
                            "duplicate definition of proc {qualified}; \
                             later definition wins"
                        ),
                        span: proc.name_span,
                    });
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                // `extern` is reserved by htcl's lowering as the
                // prefix for runtime-Tcl-proc disambiguation
                // (`extern::foo` → `__vw_extern_foo`). A user-
                // defined namespace named `extern` would silently
                // collide with that rewrite at call sites; reject
                // it up front.
                if name == "extern" {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: "`extern` is a reserved namespace name in \
                             htcl (used for runtime-Tcl-proc \
                             disambiguation); pick a different name"
                            .into(),
                        span: ns.name_span,
                    });
                    continue;
                }
                let nested = qualify(prefix, name);
                collect_signatures(&ns.body, &nested, table, diags);
            }
            _ => {}
        }
    }
}

/// Tcl core builtins that legitimately take either `-flag`
/// arguments natively (`string match -nocase`, `regexp -line`,
/// `lsort -unique`) or take positional list arguments that
/// commonly start with `-` (e.g. `lappend cmd -ruledeck $x` where
/// `-ruledeck` is being appended as a literal token, not parsed
/// by `lappend`). Calls to anything in this list pass the
/// unknown-call check unconditionally.
///
/// Keep this small but pragmatic: a missed builtin produces a
/// pestering error on calls that work fine; an over-included name
/// hides a real "you forgot to src @x" mistake. The set below is
/// the standard Tcl core surface most htcl bodies actually use.
fn is_known_tcl_builtin(name: &str) -> bool {
    matches!(
        name,
        // Container ops whose positional args often look like flags.
        "lappend"
            | "lset"
            | "linsert"
            | "lreplace"
            | "lrange"
            | "lindex"
            | "list"
            | "llength"
            | "dict"
            | "array"
            | "set"
            | "unset"
            | "incr"
            | "append"
            | "concat"
            // String / regex / sort builtins that accept `-flag`s natively.
            | "string"
            | "regexp"
            | "regsub"
            | "lsort"
            | "lsearch"
            | "switch"
            | "format"
            | "scan"
            | "binary"
            // Flow / introspection / interp.
            | "after"
            | "eval"
            | "uplevel"
            | "upvar"
            | "apply"
            | "info"
            | "package"
            | "catch"
            | "try"
            | "throw"
            | "error"
            | "return"
            | "expr"
            // I/O & filesystem.
            | "puts"
            | "gets"
            | "read"
            | "close"
            | "open"
            | "file"
            | "exec"
            | "fconfigure"
            | "fileevent"
            | "flush"
            // Channels / Tk-style.
            | "namespace"
            | "variable"
            | "global"
            | "rename"
            | "interp"
    )
}

/// Join a namespace prefix with a member name using Tcl's `::`
/// separator. The empty prefix yields the bare name (used at the
/// document root where there's no enclosing namespace).
fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn validate_command(
    cmd: &Command,
    source: &str,
    table: &HashMap<String, &ProcSignature>,
    diags: &mut Vec<Diagnostic>,
) {
    let call_name = match &cmd.kind {
        CommandKind::Generic => match cmd.words.first() {
            Some(w) => match w.as_text() {
                Some(t) => t,
                None => return,
            },
            None => return,
        },
        // Don't validate inside declarations themselves — those
        // aren't calls. (NamespaceEval is a declaration; its body's
        // statements are validated by the recursion in
        // `validate_stmts`.)
        CommandKind::Proc(_)
        | CommandKind::Set
        | CommandKind::Src(_)
        | CommandKind::NamespaceEval(_) => {
            return;
        }
    };
    // `extern::name` is the user's opt-out: "this call resolves
    // to a runtime Tcl proc, don't analyze its signature." Lowering
    // strips the prefix and aliases the underlying proc into place.
    if crate::lower::is_extern_call(call_name) {
        return;
    }
    let Some(sig) = table.get(call_name) else {
        // Unknown call. If it uses `-flag` keyword arguments, the
        // user probably meant an htcl wrapper that isn't loaded —
        // shipping it to the EDA backend would either error
        // cryptically or do something nonsensical with the
        // arguments. Force the user to be explicit: either `src` a
        // wrapper module, or use `extern::<name>` for the raw
        // Tcl/EDA proc.
        let uses_keyword = cmd.words.iter().skip(1).any(|w| {
            w.as_text()
                .is_some_and(|t| t.starts_with('-') && t.len() > 1)
        });
        if uses_keyword && !is_known_tcl_builtin(call_name) {
            let hint = match suggest_name(call_name, table.keys()) {
                Some(s) => format!(" — did you mean `{s}`?"),
                None => String::new(),
            };
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "undefined proc `{call_name}`{hint}; either \
                     `src` a module that defines it or use \
                     `extern::{call_name}` to call the underlying \
                     Tcl proc directly"
                ),
                span: cmd.words[0].span,
            });
        }
        return;
    };

    // Parse keyword args from the command's words. The first word is
    // the call name; the remaining words alternate -flag/value.
    let mut idx = 1usize;
    let mut seen: HashMap<String, Span> = HashMap::new();
    while idx < cmd.words.len() {
        let word = &cmd.words[idx];
        let flag_text = match word.as_text() {
            Some(t) if t.starts_with('-') => &t[1..],
            Some(t) => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!("expected keyword argument, found {t}"),
                    span: word.span,
                });
                idx += 1;
                continue;
            }
            None => {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: "expected keyword argument".into(),
                    span: word.span,
                });
                idx += 1;
                continue;
            }
        };
        let flag_name = flag_text.to_string();
        let value_word = cmd.words.get(idx + 1);

        match sig.find(&flag_name) {
            None => {
                let known: Vec<&str> =
                    sig.args.iter().map(|a| a.name.as_str()).collect();
                let hint = if known.is_empty() {
                    String::new()
                } else {
                    format!(". Possible values are {}", known.join(", "))
                };
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    message: format!("undefined argument -{flag_name}{hint}"),
                    span: word.span,
                });
            }
            Some(arg) => {
                if let Some(prev) = seen.insert(flag_name.clone(), word.span) {
                    let _ = prev;
                    diags.push(Diagnostic {
                        severity: Severity::Warning,
                        message: format!("duplicate argument -{flag_name}"),
                        span: word.span,
                    });
                }
                if let Some(value) = value_word {
                    validate_value(call_name, arg, value, source, diags);
                } else {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} is missing a value"
                        ),
                        span: word.span,
                    });
                }
            }
        }
        // Step past the flag and its value.
        idx += if value_word.is_some() { 2 } else { 1 };
    }

    // Build canonical `@one_of` groups. Each arg's `@one_of(...)`
    // declares an alternatives set: the arg itself plus the named
    // siblings. We collapse declarations from each direction (sib A
    // says `@one_of(B)` and sib B says `@one_of(A)`) into one
    // canonical group, then check that **exactly one** arg from each
    // group is supplied at the call site.
    //
    // Args participating in a group are treated as optional for the
    // missing-required check below — the group rule is the source of
    // truth for "must supply something."
    let one_of_groups = collect_one_of_groups(sig);
    let in_one_of: std::collections::HashSet<&str> = one_of_groups
        .iter()
        .flat_map(|g| g.iter().map(String::as_str))
        .collect();

    // Required-args check. An arg is required when it has no
    // `@default` to fall back to — the user must supply a value.
    // Args in an `@one_of` group are governed by the group rule
    // instead, so skip them here.
    for arg in &sig.args {
        if seen.contains_key(&arg.name) {
            continue;
        }
        if in_one_of.contains(arg.name.as_str()) {
            continue;
        }
        let is_required = arg.attribute("default").is_none();
        if is_required {
            diags.push(Diagnostic {
                severity: Severity::Error,
                message: format!(
                    "missing required argument -{name}",
                    name = arg.name,
                ),
                span: cmd.span,
            });
        }
    }

    // `@one_of` groups: exactly one alternative must be present.
    for group in &one_of_groups {
        let present: Vec<&str> = group
            .iter()
            .filter(|n| seen.contains_key(n.as_str()))
            .map(String::as_str)
            .collect();
        if present.len() == 1 {
            continue;
        }
        let opts: Vec<String> = group.iter().map(|n| format!("-{n}")).collect();
        let message = if present.is_empty() {
            format!(
                "missing required argument — exactly one of {} must be \
                 supplied",
                opts.join(", ")
            )
        } else {
            let got: Vec<String> =
                present.iter().map(|n| format!("-{n}")).collect();
            format!(
                "exactly one of {} may be supplied, got {}",
                opts.join(", "),
                got.join(", ")
            )
        };
        diags.push(Diagnostic {
            severity: Severity::Error,
            message,
            span: cmd.span,
        });
    }

    // Inter-arg deps for present args.
    for (flag_name, flag_span) in &seen {
        let Some(arg) = sig.find(flag_name) else {
            continue;
        };
        if let Some(req) = arg.attribute("requires") {
            for value in &req.values {
                let referenced = match value {
                    AttributeValue::Ident { value, .. }
                    | AttributeValue::String { value, .. } => value.as_str(),
                    AttributeValue::Integer { .. } => continue,
                };
                if !seen.contains_key(referenced) {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} requires -{referenced} \
                             to also be set"
                        ),
                        span: *flag_span,
                    });
                }
            }
        }
        if let Some(conflicts) = arg.attribute("conflicts") {
            for value in &conflicts.values {
                let referenced = match value {
                    AttributeValue::Ident { value, .. }
                    | AttributeValue::String { value, .. } => value.as_str(),
                    AttributeValue::Integer { .. } => continue,
                };
                if seen.contains_key(referenced) {
                    diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "argument -{flag_name} conflicts with \
                             -{referenced}"
                        ),
                        span: *flag_span,
                    });
                }
            }
        }
        if arg.attribute("deprecated").is_some() {
            let msg = arg
                .attribute("deprecated")
                .and_then(|a| a.values.first())
                .map(|v| v.as_str().to_string())
                .unwrap_or_default();
            let m = if msg.is_empty() {
                format!("argument -{flag_name} is deprecated")
            } else {
                format!("argument -{flag_name} is deprecated: {msg}")
            };
            diags.push(Diagnostic {
                severity: Severity::Warning,
                message: m,
                span: *flag_span,
            });
        }
    }
}

/// Collect canonical `@one_of` alternatives groups for a signature.
///
/// Each arg's `@one_of(sib1, sib2, ...)` declares that exactly one
/// of `{arg, sib1, sib2, ...}` must be supplied at the call site.
/// We treat the declaration as symmetric (both `dict @one_of(name)`
/// and `name @one_of(dict)` describe the same group), so a `BTreeSet`
/// of the participating names canonicalizes each group regardless of
/// which direction (or which redundant copies) the author wrote.
fn collect_one_of_groups(
    sig: &ProcSignature,
) -> Vec<std::collections::BTreeSet<String>> {
    use std::collections::BTreeSet;
    let mut seen: std::collections::HashSet<BTreeSet<String>> =
        std::collections::HashSet::new();
    let mut out: Vec<BTreeSet<String>> = Vec::new();
    for arg in &sig.args {
        let Some(attr) = arg.attribute("one_of") else {
            continue;
        };
        let mut group: BTreeSet<String> = BTreeSet::new();
        group.insert(arg.name.clone());
        for value in &attr.values {
            match value {
                AttributeValue::Ident { value, .. }
                | AttributeValue::String { value, .. } => {
                    group.insert(value.clone());
                }
                AttributeValue::Integer { .. } => continue,
            }
        }
        if group.len() >= 2 && seen.insert(group.clone()) {
            out.push(group);
        }
    }
    out
}

fn validate_value(
    call_name: &str,
    arg: &ProcArg,
    value_word: &Word,
    _source: &str,
    diags: &mut Vec<Diagnostic>,
) {
    // For Phase 2 we only validate literal-text values. Word forms
    // that include `$var` or `[cmd]` are runtime-dynamic; we let them
    // through silently. Future work can teach the validator about
    // values produced by known builtins.
    let Some(literal) = literal_value(value_word) else {
        return;
    };

    if let Some(enum_attr) = arg.attribute("enum") {
        check_enum(
            call_name, &arg.name, enum_attr, &literal, value_word, diags,
        );
    }
    if let Some(range_attr) = arg.attribute("range") {
        check_range(
            call_name, &arg.name, range_attr, &literal, value_word, diags,
        );
    }
}

fn literal_value(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::Escape { value, .. } => out.push(*value),
            WordPart::VarRef { .. } | WordPart::CmdSubst { .. } => {
                // Dynamic content — not a literal.
                return None;
            }
        }
    }
    Some(out)
}

fn check_enum(
    _call_name: &str,
    arg_name: &str,
    enum_attr: &Attribute,
    literal: &str,
    value_word: &Word,
    diags: &mut Vec<Diagnostic>,
) {
    let allowed: Vec<String> = enum_attr
        .values
        .iter()
        .map(|v| match v {
            AttributeValue::Integer { value, .. } => value.to_string(),
            AttributeValue::Ident { value, .. }
            | AttributeValue::String { value, .. } => value.clone(),
        })
        .collect();
    if !allowed.iter().any(|a| a == literal) {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "value {literal} for -{arg_name} is not in @enum. Possible \
                 values are {}",
                allowed.join(", ")
            ),
            span: value_word.span,
        });
    }
}

fn check_range(
    _call_name: &str,
    arg_name: &str,
    range_attr: &Attribute,
    literal: &str,
    value_word: &Word,
    diags: &mut Vec<Diagnostic>,
) {
    let (Some(min), Some(max)) =
        (range_attr.values.first(), range_attr.values.get(1))
    else {
        diags.push(Diagnostic {
            severity: Severity::Warning,
            message: format!(
                "@range on -{arg_name} should have two numeric bounds"
            ),
            span: range_attr.span,
        });
        return;
    };
    let (
        AttributeValue::Integer { value: min, .. },
        AttributeValue::Integer { value: max, .. },
    ) = (min, max)
    else {
        diags.push(Diagnostic {
            severity: Severity::Warning,
            message: format!("@range on -{arg_name} has non-integer bounds"),
            span: range_attr.span,
        });
        return;
    };
    let Ok(n) = literal.parse::<i64>() else {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "argument -{arg_name} expects an integer, found {literal}"
            ),
            span: value_word.span,
        });
        return;
    };
    if n < *min || n > *max {
        diags.push(Diagnostic {
            severity: Severity::Error,
            message: format!(
                "value {n} for -{arg_name} is out of @range({min}, {max})"
            ),
            span: value_word.span,
        });
    }
}

/// Standard compiler-style "did you mean X?" suggestion: pick the
/// in-scope name with the smallest edit distance from `target`,
/// within a length-scaled threshold. Returns `None` when no
/// candidate is close enough (so unknown calls that aren't
/// near-misses don't get nonsense suggestions tacked on).
fn suggest_name<'a, I>(target: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a String>,
{
    // rustc-style threshold: scales with name length so single-char
    // typos count for short names, but a 12-char identifier
    // tolerates a few keystroke errors. Floor at 1, ceiling at 3 —
    // anything past 3 starts producing surprising suggestions.
    let threshold = (target.chars().count() / 3).clamp(1, 3);
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = levenshtein(target, cand);
        if d == 0 || d > threshold {
            continue;
        }
        if best.map(|(b, _)| d < b).unwrap_or(true) {
            best = Some((d, cand.as_str()));
        }
    }
    best.map(|(_, s)| s.to_string())
}

/// Standard Levenshtein edit distance — number of single-character
/// insertions, deletions, or substitutions to turn `a` into `b`.
/// Two-row rolling table; O(n*m) time, O(n) space.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![0usize; n + 1];
    for i in 1..=m {
        cur[0] = i;
        for j in 1..=n {
            let sub = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let parsed = parse(src);
        // Parse errors shouldn't be present in these tests; assert
        // so that test failures point at the right layer.
        assert!(
            parsed.errors.is_empty(),
            "unexpected parse errors: {:?}",
            parsed.errors
        );
        validate(&parsed.document, src)
    }

    fn proc_decl(body: &str, call: &str) -> String {
        format!("proc axis_interface {{\n{body}\n}} {{ # body\n}}\n{call}\n")
    }

    #[test]
    fn happy_path_no_diagnostics() {
        let src = proc_decl(
            "  @default(0) has_tkeep\n  @default(8) tdata_num_bytes",
            "axis_interface -has_tkeep 1 -tdata_num_bytes 16",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn unknown_arg() {
        let src =
            proc_decl("  @default(0) has_tkeep", "axis_interface -has_typo 1");
        let d = diags(&src);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].message.contains("undefined argument -has_typo"),
            "{:?}",
            d
        );
        assert!(d[0].message.contains("Possible values are has_tkeep"));
    }

    #[test]
    fn missing_required() {
        let src = proc_decl("  @required width", "axis_interface");
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("missing required")));
    }

    #[test]
    fn enum_rejects_unlisted_value() {
        let src = proc_decl(
            "  @enum(1, 2, 4, 8) tdata_num_bytes",
            "axis_interface -tdata_num_bytes 3",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("@enum")));
    }

    #[test]
    fn enum_accepts_listed_value() {
        let src = proc_decl(
            "  @enum(1, 2, 4, 8) tdata_num_bytes",
            "axis_interface -tdata_num_bytes 4",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn range_check() {
        let src =
            proc_decl("  @range(1, 16) width", "axis_interface -width 32");
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("out of @range")));
    }

    #[test]
    fn requires_dependency() {
        let src = proc_decl(
            "  @default(0) has_tuser\n  @requires(has_tuser) tuser_width",
            "axis_interface -tuser_width 8",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("requires")), "{:?}", d);
    }

    #[test]
    fn conflicts_dependency() {
        let src = proc_decl(
            "  has_a\n  @conflicts(has_a) has_b",
            "axis_interface -has_a 1 -has_b 1",
        );
        let d = diags(&src);
        assert!(d.iter().any(|d| d.message.contains("conflicts")));
    }

    #[test]
    fn one_of_requires_exactly_one_alternative() {
        // Two args in an @one_of group — neither supplied → error.
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface",
        );
        let d = diags(&src);
        assert!(
            d.iter().any(|m| m.message.contains("exactly one of -a, -b")
                && m.message.contains("must be supplied")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_satisfied_by_either_alternative() {
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface -a 1",
        );
        assert!(diags(&src).is_empty());
    }

    #[test]
    fn one_of_rejects_both_alternatives() {
        // Both supplied — should be reported once (group rule).
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") @one_of(a) b",
            "axis_interface -a 1 -b 2",
        );
        let d = diags(&src);
        assert!(
            d.iter().any(|m| m.message.contains("got -a, -b")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_arg_is_not_treated_as_required() {
        // An @one_of arg without @default should NOT trigger the
        // separate "missing required" error — the group rule
        // supersedes individual required-ness.
        let src =
            proc_decl("  @one_of(b) a\n  @one_of(a) b", "axis_interface -a 1");
        let d = diags(&src);
        assert!(
            d.iter()
                .all(|m| !m.message.contains("missing required argument -a")
                    && !m.message.contains("missing required argument -b")),
            "{:?}",
            d
        );
    }

    #[test]
    fn one_of_declarations_are_symmetric() {
        // Declaring `@one_of(b)` on `a` alone is enough; we don't need
        // the reverse on `b`.
        let src = proc_decl(
            "  @default(\"\") @one_of(b) a\n  @default(\"\") b",
            "axis_interface",
        );
        let d = diags(&src);
        let group_errors: Vec<_> = d
            .iter()
            .filter(|m| {
                m.message.contains("exactly one of")
                    && m.message.contains("must be supplied")
            })
            .collect();
        assert_eq!(group_errors.len(), 1, "{:?}", d);
    }

    #[test]
    fn namespace_eval_proc_validates_at_qualified_name() {
        // A proc declared inside `namespace eval project { ... }`
        // should be reachable from the validator at its qualified
        // name (`project::set_target_language`), so `@enum`
        // constraints on its args still catch bad values at call
        // sites — exactly like a top-level proc declaration would.
        let src = "\
namespace eval project {
  proc set_target_language {
    proj
    @enum(VHDL, Verilog) language
  } { }
}
project::set_target_language -proj p -language Klingon
";
        let d = diags(src);
        assert!(
            d.iter().any(|m| m.message.contains("Klingon")
                && m.message.contains("@enum")),
            "{:?}",
            d
        );
    }

    #[test]
    fn namespaced_proc_satisfied_by_valid_args() {
        let src = "\
namespace eval project {
  proc set_target_language {
    proj
    @enum(VHDL, Verilog) language
  } { }
}
project::set_target_language -proj p -language VHDL
";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn nested_namespace_eval_qualifies_recursively() {
        let src = "\
namespace eval outer {
  namespace eval inner {
    proc foo { @enum(a, b) x } { }
  }
}
outer::inner::foo -x bogus
";
        let d = diags(src);
        assert!(d.iter().any(|m| m.message.contains("bogus")), "{:?}", d);
    }

    #[test]
    fn unknown_call_gets_did_you_mean_suggestion() {
        // The exact shape that caught the user's typo in metroid:
        // a single-char edit-distance miss against a known proc
        // should produce a `did you mean ...` suggestion.
        let src = "\
namespace eval port {
  proc plumb_if_pin {
    name
    pin
  } { }
}
port::plum_if_pin -name p -pin q
";
        let d = diags(src);
        let err = d.iter().find(|m| m.severity == Severity::Error).unwrap();
        assert!(
            err.message.contains("did you mean `port::plumb_if_pin`"),
            "{}",
            err.message
        );
    }

    #[test]
    fn unrelated_unknown_call_has_no_suggestion() {
        // A name with no near-miss should NOT get a fake suggestion
        // tacked on — that's just misleading noise.
        let src = "totally_made_up_thing -arg 1\n";
        let d = diags(src);
        let err = d.iter().find(|m| m.severity == Severity::Error).unwrap();
        assert!(!err.message.contains("did you mean"), "{}", err.message);
    }

    #[test]
    fn unknown_keyword_call_is_an_error() {
        // No proc declaration in scope, no `extern::` prefix — the
        // call uses `-flag` shape so the validator demands the user
        // be explicit about the dependency.
        let src = "create_project -in_memory 1 -name foo\n";
        let d = diags(src);
        assert!(
            d.iter().any(|m| m.severity == Severity::Error
                && m.message.contains("create_project")
                && m.message.contains("extern::")),
            "{:?}",
            d
        );
    }

    #[test]
    fn extern_prefixed_call_skips_unknown_check() {
        // `extern::` is the user's opt-out: they're calling a raw
        // Tcl proc deliberately. No diagnostic even though the
        // name isn't in the signature table.
        let src = "extern::create_project -name foo\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn positional_unknown_call_is_allowed() {
        // No `-flag` args → looks like a positional Tcl builtin
        // call (puts, set, etc.). Pass through silently.
        let src = "puts hello\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn known_tcl_builtin_with_keyword_args_is_allowed() {
        // `string match -nocase ...` is a legitimate Tcl-core
        // pattern; the allowlist keeps it from triggering the
        // unknown-call error.
        let src = "string match -nocase pat str\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn namespace_eval_extern_is_rejected() {
        let src = "namespace eval extern { proc foo {} { } }\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|m| m.message.contains("reserved namespace name")),
            "{:?}",
            d
        );
    }

    #[test]
    fn duplicate_arg_warns() {
        let src = proc_decl("  has_a", "axis_interface -has_a 1 -has_a 2");
        let d = diags(&src);
        assert!(
            d.iter().any(|d| d.message.contains("duplicate argument")),
            "{:?}",
            d
        );
    }

    #[test]
    fn dynamic_value_skips_enum_check() {
        let src =
            proc_decl("  @enum(1, 2, 4) width", "axis_interface -width $x");
        // $x is runtime; we don't statically know it's outside the
        // enum, so no enum diagnostic.
        let d = diags(&src);
        assert!(d.iter().all(|d| !d.message.contains("@enum")));
    }

    #[test]
    fn validates_call_inside_proc_body() {
        // A bad flag on a call nested in another proc's body should be
        // diagnosed, same as at the top level.
        let src = "\
proc if_tport {\n  type\n  name\n} { }\n\
proc axis_if {\n  kind\n} {\n  if_tport -type t -namze m\n}\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("undefined argument -namze")),
            "{:?}",
            d
        );
    }

    #[test]
    fn validates_call_inside_command_substitution() {
        // The user case: `set cell [create_cpm5 -foo bar]`. The
        // validator must descend into `[…]` so the bad flag is caught
        // the same way it is at the top level.
        let src = "\
proc create_cpm5 {\n  @default(0) name\n} { }\n\
set cell [create_cpm5 -foo bar]\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("undefined argument -foo")),
            "{:?}",
            d
        );
    }

    #[test]
    fn arg_with_no_default_is_implicitly_required() {
        // `name` has neither `@default` nor `@required` — calling the
        // proc without a value for it should still error.
        let src = "\
proc create_cpm5 {\n  name\n} { }\n\
create_cpm5\n";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|d| d.message.contains("missing required argument -name")),
            "{:?}",
            d
        );
    }

    #[test]
    fn implicit_required_satisfied_when_supplied() {
        let src = "\
proc create_cpm5 {\n  name\n} { }\n\
create_cpm5 -name x\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn unknown_positional_call_is_not_validated() {
        // Bare positional call to an unknown name (could be a Tcl
        // builtin) is silently accepted. Unknown calls with
        // `-flag` args are the *only* unknown-call case that
        // errors — see `unknown_keyword_call_is_an_error`.
        let src = "axis_interface tkeep_yes 1\n";
        assert!(diags(src).is_empty());
    }
}
