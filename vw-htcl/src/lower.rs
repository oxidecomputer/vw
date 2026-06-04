// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Lower htcl to plain Tcl for the EDA backend.
//!
//! Phase 2 lowering:
//!
//! - Structured `proc` declarations emit `proc name {arg1 arg2 ...}
//!   body`, where the arg list is the declared canonical order with
//!   no attributes (Vivado's Tcl doesn't understand `@default` etc.).
//! - Call sites to a known structured proc rewrite their `-flag
//!   value` form to a positional list in the canonical order, with
//!   defaults filled in for omitted args.
//! - Everything else (comments, unknown commands, calls to commands
//!   without a structured signature) passes through verbatim.
//!
//! Limitation: only top-level proc declarations and top-level call
//! sites are lowered. Calls *inside* a proc body are not rewritten —
//! the body text is shipped as-is. Phase 3+ will recursively lower
//! nested commands once we have static analysis of proc bodies.

use std::collections::HashMap;

use crate::ast::{
    Command, CommandKind, Document, Proc, ProcSignature, Stmt, Word, WordPart,
};

pub type SignatureTable<'a> = HashMap<String, &'a ProcSignature>;

/// Walk `doc` and collect every top-level proc's signature.
pub fn signature_table(doc: &Document) -> SignatureTable<'_> {
    let mut table = HashMap::new();
    for stmt in &doc.stmts {
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
        table.insert(name, sig);
    }
    table
}

/// Lower one top-level command into its Tcl equivalent for the EDA
/// backend.
pub fn lower_command(
    cmd: &Command,
    source: &str,
    table: &SignatureTable<'_>,
) -> String {
    match &cmd.kind {
        CommandKind::Proc(proc) => lower_proc_decl(proc, source),
        // `src` is a module import; by the time we lower we expect the
        // [`crate::loader`] flatten pass to have already inlined every
        // import's contents and dropped the `src` statements. Anything
        // that slips through here we render as a no-op comment so the
        // emitted Tcl is still well-formed.
        CommandKind::Src(import) => {
            let path = import.path.as_deref().unwrap_or("<dynamic>");
            format!("# vw: unresolved `src {path}` — loader bypass")
        }
        _ => {
            let call_name = cmd.words.first().and_then(Word::as_text);
            if let Some(name) = call_name {
                if let Some(sig) = table.get(name) {
                    return lower_call(name, cmd, sig, source);
                }
            }
            // Verbatim — strip a trailing `;` that would be redundant
            // on its own line.
            cmd.span.slice(source).trim_end_matches(';').to_string()
        }
    }
}

fn lower_proc_decl(proc: &Proc, source: &str) -> String {
    let name = proc.name.as_deref().unwrap_or("");
    let body = proc.body_span.slice(source);
    let args_list = match proc.signature.as_ref() {
        Some(sig) => sig
            .args
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(" "),
        None => proc.args_span.slice(source).to_string(),
    };
    format!("proc {name} {{{args_list}}} {{{body}}}")
}

fn lower_call(
    name: &str,
    cmd: &Command,
    sig: &ProcSignature,
    source: &str,
) -> String {
    // Collect keyword args. Anything that doesn't look like a `-flag
    // value` pair is silently dropped here — the validator already
    // diagnosed it.
    let mut values: HashMap<String, String> = HashMap::new();
    let mut idx = 1usize;
    while idx < cmd.words.len() {
        let word = &cmd.words[idx];
        let flag_name = match word.as_text() {
            Some(t) if t.starts_with('-') => &t[1..],
            _ => {
                idx += 1;
                continue;
            }
        };
        let Some(value_word) = cmd.words.get(idx + 1) else {
            idx += 1;
            continue;
        };
        let raw = value_word.span.slice(source);
        values.insert(flag_name.to_string(), raw.to_string());
        idx += 2;
    }

    let mut positional = Vec::with_capacity(sig.args.len());
    for arg in &sig.args {
        if let Some(v) = values.remove(&arg.name) {
            positional.push(v);
        } else if let Some(default) = arg.attribute("default") {
            let lit = default
                .values
                .first()
                .map(|v| v.to_tcl_literal())
                .unwrap_or_else(|| "{}".to_string());
            positional.push(lit);
        } else {
            // No value, no default. Validator should have flagged
            // this; emit an empty list so the Tcl proc at least gets
            // the right arity.
            positional.push("{}".to_string());
        }
    }
    format!("{name} {}", positional.join(" "))
}

/// Helper retained for symmetry with future analyzers that want to
/// inspect a word's literal form without re-walking its parts.
#[allow(dead_code)]
fn word_text(word: &Word) -> Option<String> {
    let mut out = String::new();
    for part in &word.parts {
        match part {
            WordPart::Text { value, .. } => out.push_str(value),
            WordPart::Escape { value, .. } => out.push(*value),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn lowered(src: &str) -> Vec<String> {
        let parsed = parse(src);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let table = signature_table(&parsed.document);
        parsed
            .document
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Command(c) => Some(lower_command(c, src, &table)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn proc_decl_drops_attributes() {
        let src = "proc f {\n  @default(0) a\n  @default(1) b\n} { puts hi }\n";
        let out = lowered(src);
        assert_eq!(out[0], "proc f {a b} { puts hi }");
    }

    #[test]
    fn call_with_all_flags_reorders_to_positional() {
        let src = "proc f {\n  a\n  b\n} { puts hi }\nf -b 22 -a 11\n";
        let out = lowered(src);
        assert_eq!(out[1], "f 11 22");
    }

    #[test]
    fn call_with_omitted_arg_uses_default() {
        let src = "proc f {\n  @default(7) a\n  b\n} { puts hi }\nf -b 22\n";
        let out = lowered(src);
        assert_eq!(out[1], "f 7 22");
    }

    #[test]
    fn unknown_command_passes_through() {
        let src = "puts \"hello $x\"\n";
        let out = lowered(src);
        assert_eq!(out[0], "puts \"hello $x\"");
    }

    #[test]
    fn string_default_quotes_correctly() {
        let src = "proc f {\n  @default(\"hi\") greeting\n} { puts hi }\nf\n";
        let out = lowered(src);
        assert_eq!(out[1], "f \"hi\"");
    }
}
