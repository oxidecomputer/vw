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
        if let CommandKind::Proc(proc) = &cmd.kind {
            validate_stmts(&proc.body, source, table, diags);
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

/// Build a name → signature map from top-level proc declarations.
/// Duplicate names raise a diagnostic and the later declaration wins
/// (matching Tcl semantics: a second `proc` redefines).
pub fn build_signature_table<'doc>(
    document: &'doc Document,
    diags: &mut Vec<Diagnostic>,
) -> HashMap<String, &'doc ProcSignature> {
    let mut table = HashMap::new();
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(proc) = &cmd.kind else {
            continue;
        };
        let Some(name) = proc.name.clone() else {
            continue;
        };
        let Some(sig) = proc.signature.as_ref() else {
            continue;
        };
        if table.insert(name.clone(), sig).is_some() {
            diags.push(Diagnostic {
                severity: Severity::Warning,
                message: format!(
                    "duplicate definition of proc {name}; later \
                     definition wins"
                ),
                span: proc.name_span,
            });
        }
    }
    table
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
        // Don't validate inside proc/set declarations themselves —
        // those are declarations, not calls.
        CommandKind::Proc(_) | CommandKind::Set | CommandKind::Src(_) => {
            return;
        }
    };
    let Some(sig) = table.get(call_name) else {
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

    // Required-args check. An arg is required when it has no
    // `@default` to fall back to — the user must supply a value.
    // (`@required` is still recognized for documentation but is now
    // implied by the absence of `@default` and adds nothing.)
    for arg in &sig.args {
        if seen.contains_key(&arg.name) {
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
    fn unknown_proc_is_not_validated() {
        let src = "axis_interface -has_tkeep 1\n";
        // No proc declaration anywhere — call sites to undeclared
        // commands aren't an htcl error (could be a Vivado builtin).
        assert!(diags(src).is_empty());
    }
}
