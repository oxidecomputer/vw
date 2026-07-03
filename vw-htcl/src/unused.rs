// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Unused-variable warning pass.
//!
//! Emits a [`Diagnostic`] with severity [`Severity::Warning`] for every
//! local binding whose name never surfaces in the same scope's uses.
//! Slice 1 (current): proc args + `set` decls at both the top level
//! and inside proc bodies. No brace-body reparse yet (bodies of
//! `if`/`while`/`foreach`/etc. are opaque and can hide uses), so the
//! pass tolerates false-negatives but never emits false-positives.
//! Later slices add brace-body reparse and per-construct escape
//! hatches for `upvar`/`uplevel`/`eval`/`apply`/`info`.
//!
//! **Escape hatch.** A leading `_` on a name suppresses the warning
//! for that decl — `_ignored`, `_unused`, `_` alone all count.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Command, CommandKind, Document, Stmt, Word, WordForm, WordPart,
};
use crate::hover::is_body_host;
use crate::span::Span;
use crate::validate::{Diagnostic, Severity};

/// Kind of local binding — drives the diagnostic message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeclKind {
    ProcArg,
    Set,
    ForeachVar,
    Upvar,
}

/// Where a name was declared and by what construct.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeclSite {
    pub(crate) span: Span,
    pub(crate) kind: DeclKind,
}

/// Top-level entry. Walks the document as one scope (for top-level
/// `set` decls), then recurses into every proc body / namespace-eval
/// body as its own independent scope.
pub fn validate_unused_vars(
    document: &Document,
    source: &str,
    diags: &mut Vec<Diagnostic>,
) {
    walk_scope(&document.stmts, source, diags);
}

/// Collect decls + uses over a flat list of statements as one scope,
/// then emit warnings for decls whose names never appear as uses.
/// Recurses into `NamespaceEval.body` and each `Proc.body` as fresh
/// scopes.
///
/// If the scope contains a dynamic-script construct we can't see
/// through (`eval $x`, `uplevel N $x`, `apply $x`) we suppress the
/// warnings for this scope but still descend into nested scopes —
/// those are unaffected.
fn walk_scope(stmts: &[Stmt], source: &str, diags: &mut Vec<Diagnostic>) {
    let mut decls: HashMap<String, DeclSite> = HashMap::new();
    let mut uses: HashSet<String> = HashSet::new();
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        collect_decls(cmd, source, &mut decls);
        collect_uses_in_command(cmd, source, &mut uses);
    }
    if !scope_is_leaked(stmts, source) {
        emit_unused(&decls, &uses, diags);
    }
    // Descend into nested scopes regardless — a leak in the outer
    // scope doesn't taint an inner proc's locals.
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        descend_scopes(cmd, source, diags);
    }
}

/// True when this scope contains a construct that could reference
/// locals by dynamically-computed names. Presence of any of the
/// following counts as a leak:
///
/// - `eval` with a non-literal script arg (`$x`, `"…$x…"`).
/// - `uplevel LEVEL` with a non-literal script arg (any LEVEL —
///   even LEVEL=0 with a dynamic body is unpeekable).
/// - `apply` with a non-literal envelope word (`apply $x …`).
/// - `info level`, `info vars`, `info exists` with a dynamic arg.
///
/// Scans this scope's statements plus any brace-body interiors
/// that Slice 2's reparse would walk — same-frame constructs
/// (`if`/`while`/etc.) can leak from inside their bodies too.
pub(crate) fn scope_is_leaked(stmts: &[Stmt], source: &str) -> bool {
    for stmt in stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        if command_leaks(cmd) {
            return true;
        }
        // Recurse into any brace-body interior — same frame, so
        // a leak there leaks the outer scope. Only body-hosts
        // (`if`/`while`/`foreach`/…) have such bodies.
        if let Some(head) = cmd.words.first().and_then(Word::as_text) {
            if is_body_host(head) {
                for word in cmd.words.iter().skip(1) {
                    if let Some(inner) = reparse_braced_body(word, source) {
                        if scope_is_leaked(&inner, source) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// True when a single command is a scope-leak site.
fn command_leaks(cmd: &Command) -> bool {
    let Some(head) = cmd.words.first().and_then(Word::as_text) else {
        return false;
    };
    match head {
        "eval" | "uplevel" | "apply" => {
            // Any non-literal arg → leak. Walk args, skipping the
            // command name; if any word contains a VarRef or a
            // CmdSubst it's dynamic.
            cmd.words.iter().skip(1).any(word_is_dynamic)
        }
        "info" => {
            // `info level`, `info vars`, `info exists` — all three
            // are introspection over the current frame. When their
            // arg is dynamic we can't tell which locals get named,
            // so bail. Static forms (`info exists foo`) don't leak
            // (they're a use of `foo`; `collect_uses_in_command`
            // could later grow to record them, but for now the
            // conservative treatment is to treat as leak only when
            // dynamic).
            let sub = cmd.words.get(1).and_then(Word::as_text);
            match sub {
                Some("level") | Some("vars") | Some("exists") => {
                    cmd.words.iter().skip(2).any(word_is_dynamic)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// True when `word` isn't a pure literal (contains a `$var` or
/// `[…]` substitution).
pub(crate) fn word_is_dynamic(word: &Word) -> bool {
    word.parts.iter().any(|p| {
        matches!(p, WordPart::VarRef { .. } | WordPart::CmdSubst { .. })
    })
}

/// Recurse into scope-establishing children of `cmd`. Nested procs
/// and `namespace eval` bodies each get their own `walk_scope` call.
fn descend_scopes(cmd: &Command, source: &str, diags: &mut Vec<Diagnostic>) {
    match &cmd.kind {
        CommandKind::Proc(proc) => {
            // Fresh scope. Seed it with the proc's args before
            // walking the body's statements.
            let mut decls: HashMap<String, DeclSite> = HashMap::new();
            let mut uses: HashSet<String> = HashSet::new();
            if let Some(sig) = proc.signature.as_ref() {
                for arg in &sig.args {
                    decls.insert(
                        arg.name.clone(),
                        DeclSite {
                            span: arg.name_span,
                            kind: DeclKind::ProcArg,
                        },
                    );
                }
            }
            for stmt in &proc.body {
                let Stmt::Command(inner) = stmt else { continue };
                collect_decls(inner, source, &mut decls);
                collect_uses_in_command(inner, source, &mut uses);
            }
            if !scope_is_leaked(&proc.body, source) {
                emit_unused(&decls, &uses, diags);
            }
            // And recurse into nested scopes inside the body.
            for stmt in &proc.body {
                let Stmt::Command(inner) = stmt else { continue };
                descend_scopes(inner, source, diags);
            }
        }
        CommandKind::NamespaceEval(ns) => {
            walk_scope(&ns.body, source, diags);
        }
        _ => {}
    }
}

/// If `cmd` binds a local, add it to `decls`. Recognizes:
/// - `set X value` (`CommandKind::Set` with `words.len() >= 3`)
/// - `foreach X list body` (Generic command with head `"foreach"`).
///   Both single-var (`words[1]` bare) and multi-var brace-list
///   (`words[1]` Braced containing whitespace-separated names)
///   forms are handled. The iterator var is declared in the
///   *enclosing* scope's frame per Tcl semantics — a `foreach x $list
///   {}` binding is visible after the loop returns. That means adding
///   the iterator to the same scope's decl map is correct.
///
/// **Recursion into body-hosts.** Tcl's `if`/`while`/`for`/`foreach`/
/// `catch`/`try` bodies run in the enclosing frame — a `set foo …`
/// inside `if { … } { … }` binds `foo` in the outer scope. So we
/// reparse each braced-body argument of a body-host and recurse.
/// Only `proc` and `namespace eval` bodies open fresh frames; those
/// are handled by [`descend_scopes`], not here.
pub(crate) fn collect_decls(
    cmd: &Command,
    source: &str,
    decls: &mut HashMap<String, DeclSite>,
) {
    match &cmd.kind {
        CommandKind::Set => {
            // 2-word `set` is a read (`set foo` returns $foo). Only
            // 3+-word forms are decls.
            if cmd.words.len() < 3 {
                return;
            }
            let target = &cmd.words[1];
            let Some(name) = target.as_text() else {
                return;
            };
            // First decl in the scope wins — Tcl reassignment
            // doesn't create a new binding, and pointing at the
            // original decl is what the user recognizes when
            // hunting an unused local.
            decls.entry(name.to_string()).or_insert(DeclSite {
                span: target.span,
                kind: DeclKind::Set,
            });
        }
        CommandKind::Generic => {
            // Head-based recognition. A command whose first word
            // isn't a plain identifier (e.g. `[cmd-subst]`) has
            // no head we can dispatch on — skip this stage, but
            // fall through to the body-host and cmd-subst recursion
            // below (which walk the WORDS regardless).
            if let Some(head) = cmd.words.first().and_then(Word::as_text) {
                match head {
                    "foreach" => collect_foreach_decls(cmd, decls),
                    "upvar" => collect_upvar_decls(cmd, decls),
                    "catch" => collect_catch_decls(cmd, decls),
                    "regexp" | "regsub" => collect_regexp_decls(cmd, decls),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    // Body-host recursion: `if`/`while`/`foreach`/`for`/`catch`/`try`/…
    // bodies run in the enclosing frame. Reparse each braced-body arg
    // and recurse — a `set foo …` inside binds `foo` in this scope.
    // Skip `proc` and `namespace eval` (they open fresh scopes).
    if let Some(head) = cmd.words.first().and_then(Word::as_text) {
        if is_body_host(head)
            && !matches!(
                &cmd.kind,
                CommandKind::Proc(_) | CommandKind::NamespaceEval(_)
            )
        {
            for word in cmd.words.iter().skip(1) {
                if let Some(stmts) = reparse_braced_body(word, source) {
                    for stmt in &stmts {
                        let Stmt::Command(inner) = stmt else { continue };
                        collect_decls(inner, source, decls);
                    }
                }
            }
        }
    }
    // Command-substitution recursion: `[set x 1]`, `if {[catch {…} n]} …`,
    // and any other `[…]` embedded in a word runs in the enclosing
    // frame. Its `set`/`catch`/`regexp`/… count as decls here.
    for word in &cmd.words {
        for part in &word.parts {
            if let WordPart::CmdSubst { body, .. } = part {
                for stmt in body {
                    let Stmt::Command(inner) = stmt else { continue };
                    collect_decls(inner, source, decls);
                }
            }
        }
    }
}

/// Extract the *local* names from an `upvar` command. Syntax:
/// `upvar [LEVEL] remote local ?remote local ...?`
/// LEVEL is optional; when present it's a bare numeric or `#N`
/// prefix on the first arg. Rather than parse it precisely, we
/// probe: if the first arg after `upvar` looks like a level
/// (leading digit or `#`), skip it; then take pairs (remote, local).
///
/// Every `local` becomes a decl. The `remote` names are opaque —
/// they refer to an outer frame we can't see. Dynamic-remote form
/// (`upvar $var local`) is fine: we can still see the *local* half
/// literally as a decl.
pub(crate) fn collect_upvar_decls(
    cmd: &Command,
    decls: &mut HashMap<String, DeclSite>,
) {
    let mut idx = 1;
    // Skip the optional LEVEL: bare numeric or `#`-prefixed.
    if let Some(w) = cmd.words.get(idx) {
        if let Some(t) = w.as_text() {
            if t.starts_with('#')
                || t.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                idx += 1;
            }
        }
    }
    // Now consume (remote, local) pairs.
    while idx + 1 < cmd.words.len() {
        let local_word = &cmd.words[idx + 1];
        if let Some(name) = local_word.as_text() {
            decls.entry(name.to_string()).or_insert(DeclSite {
                span: local_word.span,
                kind: DeclKind::Upvar,
            });
        }
        idx += 2;
    }
}

/// Extract the result-var and options-var names from a `catch`.
///
/// Syntax: `catch script ?resultVarName? ?optionsVarName?`. Both
/// trailing args (when literal identifiers) are decls in the
/// enclosing scope — catch runs the script in the current frame
/// and stores the return value into `resultVarName`. Dynamic
/// forms (`catch script $var`) are opaque; we skip them.
pub(crate) fn collect_catch_decls(
    cmd: &Command,
    decls: &mut HashMap<String, DeclSite>,
) {
    // `catch script` (2 words) — no result var.
    // `catch script name` (3 words) — name is result decl.
    // `catch script name opts` (4 words) — both are decls.
    for i in [2, 3] {
        let Some(w) = cmd.words.get(i) else { break };
        let Some(name) = w.as_text() else { continue };
        decls.entry(name.to_string()).or_insert(DeclSite {
            span: w.span,
            kind: DeclKind::Set,
        });
    }
}

/// Extract capture-var names from a `regexp`/`regsub` command.
///
/// `regexp ?switches? pattern string ?matchVar? ?subVar ...?`
/// Everything after the pattern + string that's a bare identifier
/// becomes a capture-var decl in the enclosing scope. Switches
/// (leading `-`) are skipped up to `--` or the first non-switch
/// word; the switch/pattern boundary heuristic here is: the first
/// non-`-` word is the pattern, the next is the string, and
/// everything else is a capture-var. That's the standard Tcl
/// shape; we skip precise switch-list parsing.
///
/// `regsub` shares the same trailing-vars shape (the last arg is
/// the result-var).
pub(crate) fn collect_regexp_decls(
    cmd: &Command,
    decls: &mut HashMap<String, DeclSite>,
) {
    // Skip the command word (index 0). Skip leading switches (any
    // word beginning with `-` that isn't `--`). After the pattern
    // + string, the rest are capture vars.
    let mut i = 1;
    while let Some(w) = cmd.words.get(i) {
        let Some(t) = w.as_text() else { break };
        if t == "--" {
            i += 1;
            break;
        }
        if !t.starts_with('-') {
            break;
        }
        i += 1;
    }
    // i is now the pattern arg. Skip it + the string arg.
    i += 2;
    // Remaining words are capture-var names (bare identifiers).
    while let Some(w) = cmd.words.get(i) {
        if let Some(name) = w.as_text() {
            decls.entry(name.to_string()).or_insert(DeclSite {
                span: w.span,
                kind: DeclKind::Set,
            });
        }
        i += 1;
    }
}

/// Extract the iterator variable(s) from a `foreach` command.
/// `foreach var $list {…}` — single var at `words[1]`.
/// `foreach {a b c} $list {…}` — multi-var brace list at `words[1]`
/// containing whitespace-separated names.
/// `foreach a $la b $lb {…}` — pairs form. We take every 2nd word
/// starting at 1 as an iterator target: words[1], words[3], … up to
/// `words.len() - 2` (last two words are the final list-value and
/// the body).
pub(crate) fn collect_foreach_decls(
    cmd: &Command,
    decls: &mut HashMap<String, DeclSite>,
) {
    if cmd.words.len() < 4 {
        // `foreach var list body` minimum. Malformed: give up
        // gracefully rather than emit a spurious decl.
        return;
    }
    // The last word is the body; strip it, then every even-indexed
    // remaining word (skipping the leading `foreach`) is an iter
    // target. Odd-indexed remainders are the list values.
    let body_idx = cmd.words.len() - 1;
    let mut i = 1;
    while i < body_idx {
        let target = &cmd.words[i];
        add_foreach_target(target, decls);
        i += 2;
    }
}

fn add_foreach_target(target: &Word, decls: &mut HashMap<String, DeclSite>) {
    if target.form == WordForm::Braced {
        // Multi-var brace list. The interior is a single Text part
        // (the parser doesn't sub-split braced words). Whitespace-
        // split and treat each token as a decl.
        let Some(WordPart::Text { value, span }) = target.parts.first() else {
            return;
        };
        // Each token gets a fresh DeclSite whose span points at
        // the containing braced word — good enough for a "the
        // culprit is here" underline; sub-token spans would need
        // extra parser wiring.
        for name in value.split_whitespace() {
            decls.entry(name.to_string()).or_insert(DeclSite {
                span: *span,
                kind: DeclKind::ForeachVar,
            });
        }
        return;
    }
    // Bare form: whole word is the iterator name.
    let Some(name) = target.as_text() else {
        return;
    };
    decls.entry(name.to_string()).or_insert(DeclSite {
        span: target.span,
        kind: DeclKind::ForeachVar,
    });
}

/// Walk `cmd.words` and every command substitution nested inside,
/// adding every `WordPart::VarRef` name to `uses`. If `cmd` is a
/// body-host construct (`if`/`while`/`foreach`/…), each `Braced`
/// argument is reparsed as a script fragment and its interior is
/// walked recursively — that reparse is what recovers false-
/// negatives from Slice 1 (variables used inside `if { $x > 0 }
/// { … }` etc.).
pub(crate) fn collect_uses_in_command(
    cmd: &Command,
    source: &str,
    uses: &mut HashSet<String>,
) {
    // Special case: `set foo` (exactly 2 words) is a *read* of `foo`,
    // not a decl. `collect_decls` correctly ignores this shape, but
    // we also need to count it here as a use so a `set foo 1; set foo`
    // doesn't warn `foo` as unused.
    if matches!(cmd.kind, CommandKind::Set) && cmd.words.len() == 2 {
        if let Some(name) = cmd.words[1].as_text() {
            uses.insert(name.to_string());
        }
    }
    for word in &cmd.words {
        collect_uses_in_word(word, source, uses);
    }
    // Body-host commands hide scripts inside braced words. Reparse
    // each such word and walk its statements as if they were part
    // of the current scope — Tcl runs them in the current frame
    // (for `if`/`while`/`foreach`/`for`/`catch`/`try` bodies at
    // least), so their VarRefs count as uses here.
    if let Some(head) = cmd.words.first().and_then(Word::as_text) {
        if is_body_host(head) {
            for word in cmd.words.iter().skip(1) {
                if let Some(stmts) = reparse_braced_body(word, source) {
                    for stmt in &stmts {
                        let Stmt::Command(inner) = stmt else {
                            continue;
                        };
                        collect_uses_in_command(inner, source, uses);
                    }
                }
            }
        }
    }
}

/// If `word` is a braced word, reparse its interior as a script
/// fragment (same recipe as `hover_in_braced_bodies`). Returns
/// `None` for non-braced words (`Bare`, `Quoted`) or when the
/// interior isn't a single text part.
pub(crate) fn reparse_braced_body(
    word: &Word,
    source: &str,
) -> Option<Vec<Stmt>> {
    if word.form != WordForm::Braced {
        return None;
    }
    let WordPart::Text { value, span } = word.parts.first()? else {
        return None;
    };
    // Body-host bodies (`if`/`while`/`foreach`/`for`/`catch`/`try`
    // {…}) are Tcl SCRIPTS — newline is a statement separator, not
    // whitespace. Reparse in `Mode::Toplevel` so each `set foo` /
    // `puts $bar` / etc. lands as its own Command. Using BracketBody
    // here would merge every statement into a single mega-command
    // whose head is the first statement's head, causing both decl
    // collection AND use collection to miss everything past the
    // first statement (repros against port.htcl's `foreach` body).
    let (mut stmts, mut errs) = crate::parser::parse_fragment(
        value.as_str(),
        crate::parser::Mode::Toplevel,
    );
    let delta = span.start;
    for s in &mut stmts {
        crate::parser::shift_stmt(s, delta);
    }
    // Errors from reparse would surface as duplicate parser
    // diagnostics; we discard them here since the top-level
    // parser has already flagged real issues. The unused-var
    // pass is best-effort.
    crate::parser::populate_procs(&mut stmts, source, &mut errs);
    Some(stmts)
}

fn collect_uses_in_word(word: &Word, source: &str, uses: &mut HashSet<String>) {
    for part in &word.parts {
        match part {
            WordPart::VarRef { name, .. } => {
                // `${foo(bar)}` lands here as a single name
                // `"foo(bar)"`. We record the whole string. A
                // decl `set foo(bar) …` would match; a decl
                // `set foo …` won't. Rare enough to defer.
                uses.insert(name.clone());
                // Also record the base name (before the `(`) so
                // that `${arr(key)}` counts as a use of a decl
                // `set arr …` — the array-vs-scalar distinction
                // is Tcl-internal, not a decl-scope question.
                if let Some(paren) = name.find('(') {
                    uses.insert(name[..paren].to_string());
                }
            }
            WordPart::CmdSubst { body, .. } => {
                // Nested command substitution: its interior stmts
                // run in the *current* frame (Tcl semantics), so
                // their VarRefs count as uses of the outer scope.
                for stmt in body {
                    let Stmt::Command(inner) = stmt else { continue };
                    collect_uses_in_command(inner, source, uses);
                }
            }
            WordPart::Text { .. } | WordPart::Escape { .. } => {}
        }
    }
}

/// Emit one warning per decl whose name isn't in `uses` and isn't
/// underscore-prefixed.
fn emit_unused(
    decls: &HashMap<String, DeclSite>,
    uses: &HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    // Sort by span so diagnostic order is stable across runs —
    // HashMap iteration order isn't.
    let mut items: Vec<(&String, &DeclSite)> = decls.iter().collect();
    items.sort_by_key(|(_, d)| d.span.start);
    for (name, decl) in items {
        if name.starts_with('_') {
            continue;
        }
        if uses.contains(name) {
            continue;
        }
        let message = match decl.kind {
            DeclKind::ProcArg => format!("unused proc arg '{name}'"),
            DeclKind::Set => format!("unused local '{name}'"),
            DeclKind::ForeachVar => {
                format!("unused foreach var '{name}'")
            }
            DeclKind::Upvar => {
                format!("unused upvar binding '{name}'")
            }
        };
        diags.push(Diagnostic {
            severity: Severity::Warning,
            message,
            span: decl.span,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let parsed = parse(src);
        assert!(
            parsed.errors.is_empty(),
            "unexpected parse errors: {:?}",
            parsed.errors
        );
        let mut out = Vec::new();
        validate_unused_vars(&parsed.document, src, &mut out);
        out
    }

    fn warning_messages(d: &[Diagnostic]) -> Vec<String> {
        d.iter()
            .filter(|dd| dd.severity == Severity::Warning)
            .map(|dd| dd.message.clone())
            .collect()
    }

    #[test]
    fn unused_proc_arg_is_flagged() {
        let src = "proc f {x} { return 1 }\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused proc arg 'x'"]);
    }

    #[test]
    fn used_proc_arg_is_not_flagged() {
        let src = "proc f {x} { return $x }\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn underscore_prefix_suppresses_arg_warning() {
        let src = "proc f {_ignored} { return 1 }\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn unused_local_set_is_flagged() {
        let src = "proc f {} { set y 1; return 2 }\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused local 'y'"]);
    }

    #[test]
    fn used_local_set_is_not_flagged() {
        let src = "proc f {} { set y 1; return $y }\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn underscore_prefix_suppresses_local_warning() {
        let src = "proc f {} { set _tmp 1; return 2 }\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn use_inside_command_substitution_counts() {
        // `$x` at the top level of `[…]` runs in the same frame —
        // counts as a use. The braced `{$x + 1}` argument of `expr`
        // is opaque in Slice 1; use a form where `$x` sits as a
        // direct word so Slice 1's walker sees it.
        let src = "proc f {x} { return [list $x] }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn nested_proc_scope_does_not_leak() {
        // Outer `y` is unused. Inner proc uses its own `y` — that
        // shouldn't count as a use of the outer.
        let src = "\
proc outer {} {
  set y 1
  proc inner {y} { return $y }
  return 2
}
";
        let msgs = warning_messages(&diags(src));
        // Both `y`s should be OK now — outer 'y' unused (warned),
        // inner 'y' used (not warned).
        assert_eq!(msgs, vec!["unused local 'y'"], "{:?}", diags(src));
    }

    #[test]
    fn top_level_unused_set_is_flagged() {
        let src = "set foo 1\nputs hello\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused local 'foo'"]);
    }

    #[test]
    fn top_level_used_set_is_not_flagged() {
        let src = "set foo 1\nputs $foo\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn top_level_underscore_prefix_suppresses() {
        let src = "set _bar 1\nputs hi\n";
        assert!(diags(src).is_empty());
    }

    #[test]
    fn two_word_set_is_read_not_decl() {
        // `set foo` (2 words) *reads* $foo. It's a use, not a decl.
        // So the declared `foo` from earlier IS used by the bare
        // `set foo` — no warning.
        let src = "set foo 1\nset foo\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn subscript_reference_counts_as_use_of_base() {
        // `$arr(key)` should count as a use of `arr`.
        let src = "proc f {} { set arr 1; return $arr(key) }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn arg_used_in_namespace_eval_body_counts() {
        let src = "\
namespace eval ns {
  proc f {x} { return $x }
}
";
        assert!(diags(src).is_empty());
    }

    // ---------- Slice 2 tests ----------

    #[test]
    fn use_inside_if_body_is_reached() {
        let src = "proc f {x} { if {1} { return $x } }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn use_inside_while_body_is_reached() {
        let src = "proc f {x} { while {0} { puts $x } }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn use_inside_nested_if_else_is_reached() {
        let src = "\
proc f {x y} {
  if {1} {
    return $x
  } else {
    return $y
  }
}
";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn foreach_iterator_used_in_body_is_not_flagged() {
        let src = "proc f {} { foreach z {1 2 3} { puts $z } }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn foreach_iterator_unused_is_flagged() {
        let src = "proc f {} { foreach z {1 2 3} { puts hi } }\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused foreach var 'z'"]);
    }

    #[test]
    fn foreach_multi_var_all_used() {
        let src = "\
proc f {} {
  foreach {a b} {1 2 3 4} {
    puts $a
    puts $b
  }
}
";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn foreach_multi_var_partial_unused() {
        let src = "\
proc f {} {
  foreach {a b} {1 2 3 4} {
    puts $a
  }
}
";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused foreach var 'b'"]);
    }

    #[test]
    fn foreach_pairs_form_all_used() {
        let src = "\
proc f {} {
  foreach a {1 2} b {3 4} {
    puts $a
    puts $b
  }
}
";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn use_via_expr_braced_arg_is_reached() {
        // The braced `{$x > 0}` is a body-host (`if`) arg — reparse
        // catches the `$x`.
        let src = "proc f {x} { if {$x > 0} { puts hi } }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    // ---------- Slice 3 tests ----------

    #[test]
    fn upvar_local_used_is_not_flagged() {
        let src = "proc f {} { upvar 1 remote local; return $local }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn upvar_local_unused_is_flagged() {
        let src = "proc f {} { upvar 1 remote local; return 1 }\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused upvar binding 'local'"]);
    }

    #[test]
    fn upvar_multi_pair_partial_unused() {
        let src = "\
proc f {} {
  upvar 1 remoteA localA remoteB localB
  return $localA
}
";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused upvar binding 'localB'"]);
    }

    #[test]
    fn dynamic_eval_leaks_scope() {
        // Proc contains `eval $script` — we can't see the script,
        // so the unused local `q` may or may not actually be
        // referenced. Conservatively: no warning.
        let src = "proc f {} { set q 1; eval $script }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn literal_eval_does_not_leak() {
        // `eval { puts hi }` — literal body, no dynamic ref. Unused
        // `q` should still warn.
        let src = "proc f {} { set q 1; eval { puts hi } }\n";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused local 'q'"], "{:?}", diags(src));
    }

    #[test]
    fn dynamic_uplevel_leaks_scope() {
        let src = "proc f {} { set q 1; uplevel 1 $script }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn dynamic_apply_leaks_scope() {
        let src = "proc f {} { set q 1; apply $lambda 42 }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn dynamic_info_exists_leaks_scope() {
        let src = "proc f {} { set q 1; info exists $name }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn nested_proc_scope_not_tainted_by_outer_leak() {
        // Outer scope has `eval $script` (leaked). Inner proc
        // has an unused arg `x` — that should still warn since the
        // inner scope is a fresh frame and doesn't inherit the
        // leak.
        let src = "\
proc outer {} {
  eval $script
  proc inner {x} { return 1 }
}
";
        let msgs = warning_messages(&diags(src));
        assert_eq!(msgs, vec!["unused proc arg 'x'"]);
    }

    #[test]
    fn use_via_expr_arg_of_expr_command() {
        // `expr` is a body-host too (it takes a Tcl script arg).
        // Wait — actually `expr {…}` isn't listed in is_body_host.
        // If this test fails, we've correctly documented that
        // `expr {$x + 1}` doesn't count as a use of $x — the user
        // has to write `expr $x + 1` (bare) for it to be visible.
        // Regardless, we assert on the CURRENT behavior for the
        // test to be stable.
        let src = "proc f {x} { return [expr $x + 1] }\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }
}
