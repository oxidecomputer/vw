// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Lower a REPL input buffer to a sequence of `(htcl-origin, Tcl)`
//! commands the Vivado worker can evaluate one at a time.
//!
//! Shipping one statement per `eval` (rather than a single
//! concatenated script) is what lets us render Vivado errors against
//! htcl source. The loader's [`vw_htcl::LoadedProgram::locate_span`]
//! tells us which `.htcl` file each top-level statement came from;
//! we keep that mapping alongside the lowered Tcl so the REPL can
//! report `× <htcl-file>:<line> <command>` instead of a Tcl stack
//! trace pointing into our shim.

use std::io::Write;
use std::path::{Path, PathBuf};

use camino::{Utf8Path, Utf8PathBuf};
use vw_htcl::{LineIndex, Resolver};

#[derive(Debug, thiserror::Error)]
pub enum LowerError {
    #[error("writing scratch input file: {0}")]
    Io(#[from] std::io::Error),
    #[error("loading htcl: {0}")]
    Load(#[from] vw_htcl::LoadError),
    #[error("{0}")]
    Parse(String),
}

/// Where in the loaded htcl tree a particular command came from.
/// Drives the error renderer in the App.
#[derive(Clone, Debug)]
pub struct Origin {
    /// `.htcl` file the command was declared in, when known. `None`
    /// only when the input itself wasn't backed by a real file
    /// (e.g. interactive REPL input lowered before any imports).
    pub file: Option<PathBuf>,
    /// 1-based line number in `file` (or in the input buffer when
    /// `file` is `None`).
    pub line: u32,
    /// First line of the command as written by the user — used as
    /// the "what was running" line in the error renderer.
    pub snippet: String,
    /// The chain of `src` imports that brought this command's file
    /// into scope, ordered nearest-first (so the last frame is the
    /// entry file / user input). Empty when the command lives
    /// directly in the entry, since there's nothing to chain.
    pub via: Vec<OriginFrame>,
}

/// One frame in the `via` chain: a `src` statement in some
/// importing file, captured as that importer's path, the line the
/// `src` lives on, and the snippet of that line (so the user sees
/// `src ip/cips` and not just `src`).
#[derive(Clone, Debug)]
pub struct OriginFrame {
    pub file: Option<PathBuf>,
    pub line: u32,
    pub snippet: String,
}

#[derive(Clone, Debug)]
pub struct PreparedCommand {
    pub tcl: String,
    pub origin: Origin,
}

#[derive(Debug)]
pub struct Prepared {
    /// Each top-level statement in the loaded program, in source
    /// order. The worker fires `eval` once per item and stops at
    /// the first failure.
    pub commands: Vec<PreparedCommand>,
    /// The htcl source to commit to the session document iff every
    /// command succeeds. Includes whatever the loader pulled in.
    pub committed_source: String,
    /// `name → proc body location` for every proc defined in the
    /// loaded program, top-level and namespaced. The error
    /// renderer uses this to translate Tcl's `(procedure "X" line
    /// N)` frames back to absolute htcl file:line locations.
    pub procs: std::collections::HashMap<String, ProcLocation>,
    /// Pre-flight findings worth surfacing to the user *before* we
    /// ship anything to Vivado. The most common one is "this call
    /// uses `-flag` keyword args but the proc isn't a loaded htcl
    /// wrapper" — Vivado's underlying builtin almost always parses
    /// the arguments differently, and the resulting error message
    /// makes no sense without that context.
    pub warnings: Vec<PrepareWarning>,
}

#[derive(Clone, Debug)]
pub struct PrepareWarning {
    pub origin: Origin,
    pub message: String,
}

/// Where a proc's body lives in htcl source. `body_start_line` is
/// the 1-based absolute line of the first body line in `file`; line
/// N of the proc's body is `body_start_line + N - 1` in `file` and
/// `body_lines[N - 1]` carries that line's text.
#[derive(Clone, Debug)]
pub struct ProcLocation {
    pub file: Option<PathBuf>,
    pub body_start_line: u32,
    pub body_lines: Vec<String>,
}

impl ProcLocation {
    /// Resolve a 1-based body line into a renderable
    /// (absolute_line, content) pair. Returns `None` when the
    /// reported line is past the end of the body — happens when
    /// Tcl points at a line we can't account for (synthesized
    /// content, off-by-one in some wrapper, etc.); the caller
    /// gracefully skips the frame.
    pub fn resolve_body_line(&self, n: u32) -> Option<(u32, String)> {
        let idx = n.checked_sub(1)? as usize;
        let content = self.body_lines.get(idx).cloned()?;
        Some((self.body_start_line + idx as u32, content))
    }
}

pub fn prepare(
    input: &str,
    cwd: &Path,
    session_prelude: &str,
) -> Result<Prepared, LowerError> {
    let workspace_dir = find_workspace_dir(cwd);
    let resolver = build_resolver(workspace_dir.as_deref());

    let scratch_dir = workspace_dir
        .as_deref()
        .map(Utf8Path::as_std_path)
        .unwrap_or(cwd);

    // Prepend the session prelude — the committed source of every
    // prior batch — so the analyzer sees every proc/namespace
    // that's already been declared in the running Tcl session. The
    // prelude is appended-to-disk only inside the scratch file;
    // the lowering output strips it back out and only ships the
    // new statements, which avoids re-defining wrappers on every
    // input.
    let (combined, pending_start) = combine_session(session_prelude, input);
    let scratch = ScratchFile::new(scratch_dir, &combined)?;

    let program = vw_htcl::load_program(&scratch.path, &resolver)?;
    let parsed = vw_htcl::parse(&program.source);

    if let Some(err) = parsed.errors.first() {
        let idx = LineIndex::new(&program.source);
        let (start, _) = idx.range(err.span);
        let where_ =
            render_location(&program, err.span, start.line + 1, &scratch.path);
        return Err(LowerError::Parse(format!("{where_}: {}", err.message)));
    }

    // Validator runs first so unknown-keyword-call errors land
    // before we ship anything. These are now hard errors (not just
    // pre-flight warnings); routing them back as `LowerError` keeps
    // the App's existing error-rendering path unchanged.
    let validator_diags = vw_htcl::validate(&parsed.document, &program.source);
    if let Some(first_err) = validator_diags
        .iter()
        .find(|d| matches!(d.severity, vw_htcl::Severity::Error))
    {
        let idx = LineIndex::new(&program.source);
        let (start, _) = idx.range(first_err.span);
        let where_ = render_location(
            &program,
            first_err.span,
            start.line + 1,
            &scratch.path,
        );
        return Err(LowerError::Parse(format!(
            "{where_}: {}",
            first_err.message
        )));
    }

    let table = vw_htcl::signature_table(&parsed.document);
    let line_index = LineIndex::new(&program.source);
    let mut commands = Vec::new();
    let mut extern_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        // Skip statements that came from the session prelude —
        // they're already declared in the running Tcl. Only ship
        // statements from the new input (and anything it
        // transitively `src`-imports, which the loader appends
        // after the user's text).
        if cmd.span.start < pending_start as u32 {
            continue;
        }
        let lowered_raw = vw_htcl::lower_command(cmd, &program.source, &table);
        let rewritten = vw_htcl::rewrite_externs(&lowered_raw);
        for name in rewritten.names {
            extern_names.insert(name);
        }
        if rewritten.text.trim().is_empty() {
            continue;
        }
        let (line_one_based, _) = line_index.range(cmd.span);
        let origin = build_origin(
            &program,
            cmd.span,
            line_one_based.line + 1,
            &scratch.path,
        );
        commands.push(PreparedCommand {
            tcl: rewritten.text,
            origin,
        });
    }

    // No prelude needed in the current architecture: wrappers
    // live in the `vivado::` namespace and `extern::name` rewrites
    // to `::name`, which Tcl resolves at the global root regardless
    // of the calling namespace. We still drain `extern_names` so
    // the analyzer can grow future per-extern bookkeeping without
    // rewiring this path.
    let _ = extern_names;

    // The session document grows with the NEW content only —
    // anything we lowered from past the `pending_start` cut. That
    // way `with_pending` on the next batch reconstructs the full
    // declaration history without double-counting.
    let committed_source = program
        .source
        .get(pending_start..)
        .map(|s| s.to_string())
        .unwrap_or_default();

    let procs = build_proc_locations(&parsed.document, &program, &scratch.path);
    // The dedicated pre-flight `collect_warnings` is gone — the
    // validator now treats "unknown call with `-flag` args" as a
    // hard error and the REPL has already returned via `LowerError`
    // above when one fires.
    let warnings: Vec<PrepareWarning> = Vec::new();

    Ok(Prepared {
        commands,
        committed_source,
        procs,
        warnings,
    })
}

/// Walk every proc declaration (top-level + nested inside
/// `namespace eval` blocks) and record its body's source location
/// keyed by the proc's qualified name. Same recursion shape as
/// `vw_htcl::validate::collect_signatures` — kept in sync by
/// convention rather than refactor so this crate stays a leaf
/// consumer of vw-htcl.
fn build_proc_locations(
    doc: &vw_htcl::Document,
    program: &vw_htcl::LoadedProgram,
    scratch_path: &Path,
) -> std::collections::HashMap<String, ProcLocation> {
    use std::collections::HashMap;
    let mut out: HashMap<String, ProcLocation> = HashMap::new();
    collect_procs(&doc.stmts, "", program, scratch_path, &mut out);
    out
}

fn collect_procs(
    stmts: &[vw_htcl::Stmt],
    prefix: &str,
    program: &vw_htcl::LoadedProgram,
    scratch_path: &Path,
    out: &mut std::collections::HashMap<String, ProcLocation>,
) {
    use vw_htcl::CommandKind;
    for stmt in stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        match &cmd.kind {
            CommandKind::Proc(proc) => {
                let Some(name) = proc.name.as_deref() else {
                    continue;
                };
                let qualified = qualify(prefix, name);
                if let Some(loc) =
                    proc_body_location(program, proc.body_span, scratch_path)
                {
                    out.insert(qualified, loc);
                }
            }
            CommandKind::NamespaceEval(ns) => {
                let Some(name) = ns.name.as_deref() else {
                    continue;
                };
                let nested = qualify(prefix, name);
                collect_procs(&ns.body, &nested, program, scratch_path, out);
            }
            _ => {}
        }
    }
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn proc_body_location(
    program: &vw_htcl::LoadedProgram,
    body_span: vw_htcl::Span,
    scratch_path: &Path,
) -> Option<ProcLocation> {
    let (file_index, file_span) = program.locate_span(body_span)?;
    let file = &program.files[file_index];
    let file_path = if file.path == scratch_path {
        None
    } else {
        Some(file.path.clone())
    };
    // Tcl's `(procedure "X" line N)` counts the line **containing
    // the opening `{`** as line 1, the next line as line 2, etc.
    // `body_span.start` is the byte right after the `{`, so the
    // `{` itself sits at `file_span.start - 1`. The line at that
    // byte is what Tcl calls "line 1." When the proc body is on a
    // single line (`proc f {x} {puts $x}`) that line is also the
    // content line.
    let brace_pos = file_span.start.saturating_sub(1);
    let body_start_line = file_line_at(&file.source, brace_pos);
    // For the body_lines vector we want every file line from the
    // one with the `{` up to (and including) the one with the
    // matching `}` — so `resolve_body_line(N)` returns the
    // corresponding source. Anything past the body is irrelevant.
    let body_end_line =
        file_line_at(&file.source, file_span.end.saturating_sub(1));
    let body_lines: Vec<String> = file
        .source
        .lines()
        .skip(body_start_line.saturating_sub(1) as usize)
        .take((body_end_line - body_start_line + 1) as usize)
        .map(str::to_string)
        .collect();
    Some(ProcLocation {
        file: file_path,
        body_start_line,
        body_lines,
    })
}

fn build_origin(
    program: &vw_htcl::LoadedProgram,
    span: vw_htcl::Span,
    flat_line: u32,
    scratch_path: &Path,
) -> Origin {
    // Full span — for a multi-line `set proj [ … ]` the snippet
    // includes every line of the command so the trace shows what
    // the user actually wrote, not just `set proj [`. The renderer
    // is responsible for indenting continuation lines.
    let snippet = program.source[span.start as usize..span.end as usize]
        .trim_end()
        .to_string();

    if let Some((file_index, file_span)) = program.locate_span(span) {
        let file = &program.files[file_index];
        let file_path = if file.path == scratch_path {
            None
        } else {
            Some(file.path.clone())
        };
        let file_line = file_line_at(&file.source, file_span.start);
        let via = build_via_chain(program, file_index, scratch_path);
        return Origin {
            file: file_path,
            line: file_line,
            snippet,
            via,
        };
    }
    Origin {
        file: None,
        line: flat_line,
        snippet,
        via: Vec::new(),
    }
}

/// Walk the loader's import chain from the leaf file back toward
/// the entry, turning each [`vw_htcl::ImportEdge`] into a renderable
/// frame. Nearest first.
fn build_via_chain(
    program: &vw_htcl::LoadedProgram,
    leaf_file: usize,
    scratch_path: &Path,
) -> Vec<OriginFrame> {
    program
        .ancestry(leaf_file)
        .map(|edge| {
            let importer = &program.files[edge.importer_file];
            let line = file_line_at(&importer.source, edge.src_span.start);
            let snippet = first_line(
                &importer.source,
                edge.src_span.start as usize,
                edge.src_span.end as usize,
            );
            OriginFrame {
                file: if importer.path == scratch_path {
                    None
                } else {
                    Some(importer.path.clone())
                },
                line,
                snippet,
            }
        })
        .collect()
}

fn first_line(source: &str, start: usize, end: usize) -> String {
    let line_end = source[start..].find('\n').map(|n| start + n).unwrap_or(end);
    source[start..line_end].trim().to_string()
}

fn file_line_at(source: &str, offset: u32) -> u32 {
    let upto = offset.min(source.len() as u32) as usize;
    1 + source[..upto].bytes().filter(|b| *b == b'\n').count() as u32
}

fn render_location(
    program: &vw_htcl::LoadedProgram,
    span: vw_htcl::Span,
    flat_line: u32,
    scratch_path: &Path,
) -> String {
    if let Some((file_index, file_span)) = program.locate_span(span) {
        let file = &program.files[file_index];
        if file.path != scratch_path {
            let line = file_line_at(&file.source, file_span.start);
            return format!("{}:{line}", file.path.display());
        }
    }
    format!("(input):{flat_line}")
}

fn find_workspace_dir(start: &Path) -> Option<Utf8PathBuf> {
    let mut cur = Utf8PathBuf::from_path_buf(start.to_path_buf()).ok()?;
    loop {
        if cur.join("vw.toml").exists() {
            return Some(cur);
        }
        let parent = cur.parent()?.to_path_buf();
        if parent == cur {
            return None;
        }
        cur = parent;
    }
}

/// Concatenate the session prelude with the new input. Returns the
/// combined text plus the byte offset of where the new input
/// begins — the lowering uses that cutoff to decide which
/// statements are new (and must be shipped to Vivado) vs already-
/// declared (analyzer reads them for signature lookup but doesn't
/// re-emit them).
fn combine_session(prelude: &str, input: &str) -> (String, usize) {
    if prelude.is_empty() {
        return (input.to_string(), 0);
    }
    let mut out = String::with_capacity(prelude.len() + input.len() + 1);
    out.push_str(prelude);
    if !prelude.ends_with('\n') {
        out.push('\n');
    }
    let pending_start = out.len();
    out.push_str(input);
    (out, pending_start)
}

fn build_resolver(workspace_dir: Option<&Utf8Path>) -> Resolver {
    let mut resolver = Resolver::new();
    let Some(ws) = workspace_dir else {
        return resolver;
    };
    if let Ok(paths) = vw_lib::transitive_dep_cache_paths(ws) {
        for (name, path) in paths {
            resolver = resolver.with_dep(name, path);
        }
    }
    resolver
}

struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    fn new(dir: &Path, contents: &str) -> std::io::Result<Self> {
        let name = format!(".vw-repl-input-{}.htcl", std::process::id());
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path)?;
        f.write_all(contents.as_bytes())?;
        Ok(Self { path })
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keyword_call_inside_bracket_errors() {
        // Mirrors the metroid project.htcl shape: a call to an
        // unknown proc with keyword args, nested inside a `[ … ]`
        // substitution. The validator now treats this as a hard
        // error so the lowering returns `Err` and nothing ships
        // to Vivado — the user is forced to either `src` a
        // wrapper module or write `extern::create_project`.
        let dir = tempfile::tempdir().unwrap();
        let err = prepare(
            "set proj [\n  create_project\n    -in_memory 1\n    -name foo\n]\n",
            dir.path(),
            "",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("create_project"), "{msg}");
        assert!(msg.contains("extern::"), "{msg}");
    }

    #[test]
    fn extern_prefixed_call_is_accepted() {
        // The opt-out: `extern::create_project` is explicitly a
        // raw Tcl call, no wrapper required. Lowering strips the
        // prefix so the bare native resolves through Tcl's global
        // namespace at runtime — no rename plumbing, no prelude.
        let dir = tempfile::tempdir().unwrap();
        let prep =
            prepare("extern::create_project -name foo\n", dir.path(), "")
                .unwrap();
        assert_eq!(prep.commands.len(), 1, "{:?}", prep.commands);
        assert!(
            prep.commands[0].tcl.contains("create_project -name foo"),
            "{}",
            prep.commands[0].tcl
        );
        assert!(
            !prep.commands[0].tcl.contains("extern::"),
            "{}",
            prep.commands[0].tcl
        );
    }

    #[test]
    fn session_prelude_brings_prior_procs_into_scope() {
        // Reproduces the REPL "src @lib then call" pattern: a
        // wrapper declared in a previous batch (now in the session
        // prelude) should be visible to the analyzer when we
        // lower a bare call in the next batch — and the lowering
        // should ship only the new statement, not redefine the
        // wrapper.
        let dir = tempfile::tempdir().unwrap();
        let prelude = "\
namespace eval vivado {
  proc current_project {
    @enum(0, 1) @default(0) quiet
    @enum(0, 1) @default(0) verbose
    @default(\"\") project
  } {
    set cmd [list ::current_project]
    return [{*}$cmd]
  }
}
";
        let prep =
            prepare("vivado::current_project\n", dir.path(), prelude).unwrap();
        // Only the new call ships — the wrapper declaration from
        // the prelude is already defined in Tcl and shouldn't get
        // re-emitted.
        assert_eq!(prep.commands.len(), 1, "{:?}", prep.commands);
        // And the lowered call uses the wrapper's keyword→
        // positional rewrite, supplying defaults for omitted args.
        assert!(
            prep.commands[0].tcl.contains("vivado::current_project 0 0"),
            "{}",
            prep.commands[0].tcl
        );
    }

    #[test]
    fn known_keyword_call_is_not_errored() {
        // When the called proc IS in scope, no error fires.
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "proc create_project { @default(\"\") name } { }\n\
             set proj [ create_project -name foo ]\n",
            dir.path(),
            "",
        )
        .unwrap();
        assert!(prep.warnings.is_empty(), "{:?}", prep.warnings);
    }

    #[test]
    fn lowers_plain_proc_call_to_tcl() {
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare("puts hello", dir.path(), "").unwrap();
        assert_eq!(prep.commands.len(), 1);
        assert!(prep.commands[0].tcl.contains("puts hello"));
        // Input is at line 1 of the buffer.
        assert_eq!(prep.commands[0].origin.line, 1);
        assert!(prep.commands[0].origin.file.is_none());
        assert_eq!(prep.commands[0].origin.snippet, "puts hello");
    }

    #[test]
    fn each_statement_gets_its_own_origin() {
        let dir = tempfile::tempdir().unwrap();
        let prep =
            prepare("set x 1\nset y 2\nset z 3", dir.path(), "").unwrap();
        assert_eq!(prep.commands.len(), 3);
        assert_eq!(prep.commands[0].origin.line, 1);
        assert_eq!(prep.commands[1].origin.line, 2);
        assert_eq!(prep.commands[2].origin.line, 3);
    }

    #[test]
    fn proc_body_line_resolution_matches_tcl_line_counting() {
        // Tcl counts the proc-body line **containing the opening
        // `{`** as line 1 — so a `(procedure "ip::check" line 2)`
        // frame should point at the first content line of the body,
        // not the line after it.
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep");
        std::fs::create_dir_all(&dep).unwrap();
        // Lines 1-2: blank + the namespace header; line 3 has `{`
        // (the proc body opener); content lives on lines 4+.
        std::fs::write(
            dep.join("module.htcl"),
            "namespace eval foo {\n  proc bar {} {\n    puts hi\n    error oh-no\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("vw.toml"),
            format!(
                "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.dep]\npath = \"{}\"\n",
                dep.display()
            ),
        )
        .unwrap();
        let prep = prepare("src @dep", dir.path(), "").unwrap();
        let loc = prep
            .procs
            .get("foo::bar")
            .expect("expected foo::bar in proc map");
        // The proc body opens on file line 2 (the `} {`-style line
        // here is just `proc bar {} {`), so Tcl line 1 → line 2 of
        // the file.
        assert_eq!(loc.body_start_line, 2);
        // Tcl line 2 → file line 3 → `puts hi`.
        let (line, content) = loc.resolve_body_line(2).unwrap();
        assert_eq!(line, 3);
        assert_eq!(content.trim(), "puts hi");
        // Tcl line 3 → file line 4 → `error oh-no`.
        let (line, content) = loc.resolve_body_line(3).unwrap();
        assert_eq!(line, 4);
        assert_eq!(content.trim(), "error oh-no");
    }

    #[test]
    fn origin_via_chain_walks_back_through_src_imports() {
        // entry → mid → leaf, all via `src`. A command in `leaf`
        // should carry a 2-frame via chain (mid → entry/input).
        let dir = tempfile::tempdir().unwrap();
        let mid_dep = dir.path().join("mid_dep");
        let leaf_dep = dir.path().join("leaf_dep");
        std::fs::create_dir_all(&mid_dep).unwrap();
        std::fs::create_dir_all(&leaf_dep).unwrap();
        std::fs::write(leaf_dep.join("module.htcl"), "set leaf_var 1\n")
            .unwrap();
        std::fs::write(mid_dep.join("module.htcl"), "src @leaf\n").unwrap();
        std::fs::write(
            dir.path().join("vw.toml"),
            format!(
                "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.mid]\npath = \"{}\"\n\
                 [dependencies.leaf]\npath = \"{}\"\n",
                mid_dep.display(),
                leaf_dep.display()
            ),
        )
        .unwrap();

        let prep = prepare("src @mid", dir.path(), "").unwrap();
        assert_eq!(prep.commands.len(), 1);
        let origin = &prep.commands[0].origin;
        // Leaf-most command lives in leaf_dep/module.htcl.
        assert!(
            origin
                .file
                .as_ref()
                .unwrap()
                .ends_with("leaf_dep/module.htcl"),
            "{:?}",
            origin.file
        );
        // The via chain: leaf was imported by mid (line 1), and mid
        // was imported by the entry input (line 1).
        assert_eq!(origin.via.len(), 2, "{:?}", origin.via);
        assert!(origin.via[0]
            .file
            .as_ref()
            .unwrap()
            .ends_with("mid_dep/module.htcl"));
        assert_eq!(origin.via[0].snippet, "src @leaf");
        // The outermost frame is the user's input (file = None).
        assert!(origin.via[1].file.is_none(), "{:?}", origin.via[1].file);
        assert_eq!(origin.via[1].snippet, "src @mid");
    }

    #[test]
    fn src_imported_statements_resolve_to_imported_file() {
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            dep.join("module.htcl"),
            "proc hello {} { puts world }\nhello\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("vw.toml"),
            format!(
                "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.dep]\npath = \"{}\"\n",
                dep.display()
            ),
        )
        .unwrap();

        let prep = prepare("src @dep", dir.path(), "").unwrap();
        // Two commands from the imported file: `proc hello` and the
        // bare `hello` call. Both must carry the imported file's
        // path as origin.
        assert_eq!(prep.commands.len(), 2);
        for cmd in &prep.commands {
            let file = cmd.origin.file.as_ref().expect("import has file");
            assert!(file.ends_with("dep/module.htcl"), "{:?}", file);
        }
        // Line numbers point into the imported file.
        assert_eq!(prep.commands[0].origin.line, 1);
        assert_eq!(prep.commands[1].origin.line, 2);
    }
}
