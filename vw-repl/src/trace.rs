// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Stack-frame rewriting for Vivado error / warning messages.
//!
//! Vivado reports errors with frames like
//!   `  at <input>:14 in ::configure_cips`
//!
//! where `<input>` is the scratch path the lowerer ships and the
//! line number is body-relative inside the proc. This module maps
//! those back to the original htcl source:
//!   `  at ip/cips.htcl:69 in ::configure_cips`
//!
//! Both the REPL (driven by a multi-batch [`crate::session::Session`])
//! and the `vw run` CLI driver (single batch) feed messages through
//! the same `resolve_stack_frames_with` machinery — they differ only
//! in how they answer the "where does proc P live?" question, supplied
//! as a closure.

use std::path::Path;

use crate::lower::ProcLocation;

/// One stack-frame line after rewriting. Callers dedupe adjacent
/// frames that resolve to the same `(proc, line)` because Vivado
/// often emits two frames per logical site (one for the `proc`'s
/// `kwargs` wrapper, one for the real body).
pub struct RewrittenFrame {
    pub proc: String,
    pub line: u32,
    pub formatted: String,
}

/// Walk a message line-by-line, rewriting any `at <input>:N in
/// ::proc` frames using `lookup`. Lines that don't match the
/// stack-frame grammar (regular message prose) pass through
/// unchanged. Adjacent frames that resolve to the same
/// `(proc, line)` are collapsed — the Vivado kwargs-wrapper +
/// body-call doubling becomes a single rendered frame.
pub fn resolve_stack_frames_with<F>(
    msg: &str,
    lookup: F,
    input_file: Option<&Path>,
) -> String
where
    F: Fn(&str) -> Option<ProcLocation>,
{
    let mut out = String::with_capacity(msg.len());
    let mut last_resolved_key: Option<(String, u32)> = None;
    for (i, line) in msg.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let Some(rewritten) = rewrite_stack_line(line, &lookup, input_file)
        else {
            out.push_str(line);
            last_resolved_key = None;
            continue;
        };
        let key = (rewritten.proc.clone(), rewritten.line);
        if last_resolved_key.as_ref() == Some(&key) {
            if out.ends_with('\n') {
                out.pop();
            }
            continue;
        }
        last_resolved_key = Some(key);
        out.push_str(&rewritten.formatted);
    }
    out
}

/// Parse a single line like `  at <input>:14 in ::configure_cips`
/// and rewrite it to point at the user's actual htcl source.
/// Returns `None` when the line isn't a stack frame (regular
/// message text) or when the proc isn't one we know about (Vivado
/// builtins, dynamic procs, etc.) — caller passes such lines
/// through unchanged.
pub fn rewrite_stack_line<F>(
    line: &str,
    lookup: F,
    input_file: Option<&Path>,
) -> Option<RewrittenFrame>
where
    F: Fn(&str) -> Option<ProcLocation>,
{
    // Grammar emitted by `vw::format_frame`:
    //   "  at <input>:N in ::procname"  ← lookup ProcLocation by name
    //   "  at <file>:N in ::procname"   ← already absolute
    //   "  at <file>:N"                 ← anonymous eval / top-level
    //   "  at <procname>"               ← location-less
    let rest = line.strip_prefix("  at ")?;
    let (loc_str, proc_part) = match rest.split_once(" in ") {
        Some((l, p)) => (l, Some(p.trim().to_string())),
        None => (rest, None),
    };
    let (file_part, line_part) = loc_str.rsplit_once(':')?;
    let body_line: u32 = line_part.parse().ok()?;

    // Top-level `<input>:N` frame (no proc).
    let Some(proc) = proc_part else {
        if file_part != "<input>" {
            return None;
        }
        let path = input_file?;
        return Some(RewrittenFrame {
            proc: String::new(),
            line: body_line,
            formatted: format!("  at {}:{body_line}", display_path(path)),
        });
    };

    // Already-absolute frames don't need rewriting; pass through
    // (dedup downstream still benefits from parsed proc+line).
    if file_part != "<input>" {
        return Some(RewrittenFrame {
            proc,
            line: body_line,
            formatted: line.to_string(),
        });
    }
    // `<input>:N in ::proc` — Tcl reports "line N of the proc
    // body." Resolve through the lookup. Tcl always reports
    // fully-qualified names (leading `::`); the proc table
    // indexes them without (see `lower::qualify`), so strip
    // before lookup.
    let lookup_name = proc.strip_prefix("::").unwrap_or(&proc);
    let loc = lookup(lookup_name)?;
    let (abs_line, _content) = loc.resolve_body_line(body_line)?;
    let path_str = match loc.file.as_deref() {
        Some(p) => display_path(p),
        None => match input_file {
            Some(p) => display_path(p),
            None => "<input>".to_string(),
        },
    };
    Some(RewrittenFrame {
        proc: proc.clone(),
        line: abs_line,
        formatted: format!("  at {path_str}:{abs_line} in {proc}"),
    })
}

/// Pretty-print a file path for diagnostics: prefer the cwd-
/// relative form (`ip/cips.htcl`) when the path is under the
/// current working directory, then home-relative (`~/src/…`),
/// then the absolute form. Matches the REPL's scrollback so
/// vw run + vw repl render the same way.
pub fn display_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home_path = Path::new(&home);
        if let Ok(rel) = path.strip_prefix(home_path) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}
