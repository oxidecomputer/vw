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
//! - `vhdl_dependency_sources` — every VHDL file shipped by any
//!   transitive dep (regular `[dependencies]` only), grouped by
//!   target library. Consumed by `design.htcl` to feed
//!   `read_vhdl -library <lib> …`.
//! - `vhdl_dependency_sources_with_test` — same, but the entry
//!   workspace's `[test-dependencies]` are also included.
//!   Consumed by `test/*.htcl` where test-deps are in scope.
//! - `vhdl_design_sources` — every VHDL file under
//!   `<workspace>/hdl/`. Consumed alongside dependency sources
//!   to compile the workspace's own design.
//! - `vhdl_ip_sources` — every generated IP wrapper under
//!   `<workspace>/target/ip/**/*.vhd`. Populated by
//!   `vw::make_wrapper`. Kept separate from design sources
//!   because wrappers have their own regen lifecycle and
//!   typically compile into a distinct library (`ip`).

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
        "vhdl_dependency_sources" => {
            vhdl_dependency_sources(
                workspace_root,
                /*include_test=*/ false,
            )
            .await
        }
        "vhdl_dependency_sources_with_test" => {
            vhdl_dependency_sources(workspace_root, /*include_test=*/ true)
                .await
        }
        "vhdl_design_sources" => vhdl_design_sources(workspace_root),
        "vhdl_ip_sources" => vhdl_ip_sources(workspace_root),
        other => Err(format!("unknown RPC method: {other}")),
    }
}

/// `vhdl_dependency_sources` — return every transitive-dep VHDL
/// file, grouped by target library, as a JSON object of the shape
/// `{"library_name": ["/abs/path/a.vhd", "/abs/path/b.vhd"], …}`.
/// Grouped-by-library because the primary consumer is a Vivado
/// `read_vhdl -library <lib> $files` loop — the tuple-per-file
/// shape would force the caller to bucket, which we can do here.
async fn vhdl_dependency_sources(
    workspace_root: Option<&std::path::Path>,
    include_test: bool,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    // Auto-fetch missing git deps. `workspace_has_unlocked_git_deps`
    // is a cheap disk-only check — no network — so the common
    // "already up-to-date" path stays cheap. When it returns
    // true we invoke the same fetch machinery `vw update` uses;
    // downstream enumeration then sees a fully-populated
    // `vw.lock` and `~/.vw/deps` layout.
    let unlocked = vw_lib::workspace_has_unlocked_git_deps(&ws, include_test)
        .map_err(|e| format!("checking lockfile: {e}"))?;
    if unlocked {
        tracing::info!(
            workspace = %ws,
            "auto-updating workspace: git deps missing from vw.lock",
        );
        // Look up netrc credentials the same way `vw update` does
        // so a workspace with private git deps (e.g. an
        // organization's internal GitHub repo, gitea, …) doesn't
        // 401 when auto-update kicks in. `None` is fine when
        // every git URL is public — libgit2 falls back to
        // unauthenticated clone.
        let creds =
            vw_lib::get_access_credentials_for_workspace(&ws, include_test);
        vw_lib::update_workspace_with_token(&ws, creds)
            .await
            .map_err(|e| format!("auto-updating workspace: {e}"))?;
    }
    let sources = vw_lib::vhdl_dependency_sources_with_test(&ws, include_test)
        .map_err(|e| format!("enumerating VHDL dep sources: {e}"))?;
    let mut by_library: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for src in sources {
        by_library
            .entry(src.library)
            .or_default()
            .push(Value::String(src.path.to_string_lossy().into_owned()));
    }
    let obj: serde_json::Map<String, Value> = by_library
        .into_iter()
        .map(|(lib, files)| (lib, Value::Array(files)))
        .collect();
    Ok(Value::Object(obj))
}

/// `vhdl_design_sources` — return every VHDL file under
/// `<workspace>/hdl/` as a JSON array of absolute-path strings.
/// Empty array when the workspace has no `hdl/` dir yet.
fn vhdl_design_sources(
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let paths = vw_lib::vhdl_design_sources(&ws)
        .map_err(|e| format!("enumerating VHDL design sources: {e}"))?;
    Ok(paths_to_json_array(paths))
}

/// `vhdl_ip_sources` — return every generated IP wrapper under
/// `<workspace>/target/ip/**/*.vhd` as a JSON array of
/// absolute-path strings. Empty array when nothing has been
/// wrapped yet.
fn vhdl_ip_sources(
    workspace_root: Option<&std::path::Path>,
) -> Result<Value, String> {
    let ws = workspace_root_or_error(workspace_root)?;
    let paths = vw_lib::vhdl_ip_sources(&ws)
        .map_err(|e| format!("enumerating VHDL IP sources: {e}"))?;
    Ok(paths_to_json_array(paths))
}

fn paths_to_json_array(paths: Vec<std::path::PathBuf>) -> Value {
    Value::Array(
        paths
            .into_iter()
            .map(|p| Value::String(p.to_string_lossy().into_owned()))
            .collect(),
    )
}

/// Small helper: workspace-root paths served through the RPC are
/// std `Path`, but `vw_lib` takes `Utf8Path`. Do the conversion in
/// one place and normalize the error to the RPC message shape.
fn workspace_root_or_error(
    workspace_root: Option<&std::path::Path>,
) -> Result<camino::Utf8PathBuf, String> {
    let raw = workspace_root.ok_or_else(|| {
        "no workspace root: entry file has no `vw.toml` in its parent chain"
            .to_string()
    })?;
    camino::Utf8PathBuf::from_path_buf(raw.to_path_buf()).map_err(|p| {
        format!("workspace root is not valid UTF-8: {}", p.display())
    })
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
