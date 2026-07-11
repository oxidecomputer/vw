// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Shared RPC handlers wired into every Vivado spawn (`vw run`,
//! `vw repl`, `vw test`). Each method here answers a `vw::` htcl
//! proc whose answer lives on the tool side rather than in Vivado.
//!
//! Methods:
//! - `workspace_root` — the discovered `vw.toml` parent dir.
//! - `diff_files` — read two files and return a unified diff of
//!   their contents. Used by `test::assert_file_eq` to render a
//!   readable failure message instead of a two-line "files differ"
//!   note.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::rpc::{FnHandler, RpcHandler};

/// Build the FnHandler used by every Vivado spawn. `workspace_root`
/// is the workspace-root path to serve for `vw::workspace_root`;
/// pass `None` when the caller couldn't discover one (the RPC
/// method then returns an error instead of a bogus path).
pub fn make_handler(workspace_root: Option<PathBuf>) -> Arc<dyn RpcHandler> {
    let workspace_root = workspace_root.map(Arc::new);
    FnHandler::new(move |method: String, args: Value| {
        let ws = workspace_root.clone();
        async move {
            dispatch(&method, args, ws.as_deref().map(|p| p.as_ref())).await
        }
    })
}

async fn dispatch(
    method: &str,
    args: Value,
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    match method {
        "workspace_root" => workspace_root
            .map(|p| Value::String(p.to_string_lossy().into_owned()))
            .ok_or_else(|| {
                "no workspace root: entry file has no `vw.toml` in its \
                 parent chain"
                    .to_string()
            }),
        "diff_files" => diff_files(args),
        other => Err(format!("unknown RPC method: {other}")),
    }
}

/// `diff_files` — inputs `{actual: <path>, expected: <path>}`;
/// output is a JSON string carrying the unified diff, or an empty
/// string when the files are byte-equal. Paths are used verbatim
/// (the htcl side already resolves them relative to workspace
/// root before dispatching).
fn diff_files(args: Value) -> Result<Value, String> {
    let obj = args.as_object().ok_or_else(|| {
        "diff_files: args must be an object with `actual` and \
         `expected` string fields"
            .to_string()
    })?;
    let actual = obj
        .get("actual")
        .and_then(Value::as_str)
        .ok_or_else(|| "diff_files: missing string `actual`".to_string())?;
    let expected = obj
        .get("expected")
        .and_then(Value::as_str)
        .ok_or_else(|| "diff_files: missing string `expected`".to_string())?;
    let a_bytes = std::fs::read(actual)
        .map_err(|e| format!("reading actual `{actual}`: {e}"))?;
    let b_bytes = std::fs::read(expected)
        .map_err(|e| format!("reading expected `{expected}`: {e}"))?;
    if a_bytes == b_bytes {
        return Ok(Value::String(String::new()));
    }
    // Prefer text mode when both files are valid UTF-8 — the
    // typical case for VHDL/htcl/SDC/XDC. When either side is
    // binary, fall through to a short "binary files differ"
    // marker so we don't spew hex.
    let (Ok(a_text), Ok(b_text)) =
        (std::str::from_utf8(&a_bytes), std::str::from_utf8(&b_bytes))
    else {
        return Ok(Value::String(format!(
            "binary files differ ({} bytes actual, {} bytes expected)",
            a_bytes.len(),
            b_bytes.len(),
        )));
    };
    let diff = render_unified_diff(a_text, b_text, expected, actual);
    Ok(Value::String(diff))
}

/// Render a colored unified diff between `expected` and `actual`.
/// The header lines put `expected` on `-` (removed) and `actual`
/// on `+` (added), which matches the "you wanted X but got Y"
/// narrative that assertion messages want.
fn render_unified_diff(
    actual: &str,
    expected: &str,
    expected_path: &str,
    actual_path: &str,
) -> String {
    use colored::Colorize;
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(expected, actual);
    let mut out = String::new();
    out.push_str(&format!("--- {}\n", expected_path).red().to_string());
    out.push_str(&format!("+++ {}\n", actual_path).green().to_string());
    for group in diff.grouped_ops(3) {
        // Each group is a run of consecutive ops sharing context —
        // similar's own recommended unit for rendering per-hunk
        // headers.
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                let mut line = String::new();
                line.push_str(sign);
                for (emphasized, value) in change.iter_strings_lossy() {
                    if emphasized {
                        // Inline-changed subrun — bold within the
                        // colored line to draw the eye to the exact
                        // byte-run that differs.
                        match change.tag() {
                            ChangeTag::Delete => {
                                line.push_str(
                                    &value.on_red().white().bold().to_string(),
                                );
                            }
                            ChangeTag::Insert => {
                                line.push_str(
                                    &value
                                        .on_green()
                                        .white()
                                        .bold()
                                        .to_string(),
                                );
                            }
                            ChangeTag::Equal => line.push_str(&value),
                        }
                    } else {
                        line.push_str(&value);
                    }
                }
                // `iter_inline_changes` already emits the trailing
                // newline for line-based diffs; only add one if
                // it's missing so the last line of a hunk renders
                // cleanly.
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                let colored_line = match change.tag() {
                    ChangeTag::Delete => line.red().to_string(),
                    ChangeTag::Insert => line.green().to_string(),
                    ChangeTag::Equal => line.dimmed().to_string(),
                };
                out.push_str(&colored_line);
            }
        }
    }
    out
}
