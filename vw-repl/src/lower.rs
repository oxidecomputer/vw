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

use crate::session::{Session, SessionBatch};

struct NoopObserver;
impl vw_htcl::LoadObserver for NoopObserver {}

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
    /// The parsed program + proc map for this batch. Stays out of
    /// the session document until every command in [`commands`]
    /// succeeds — at which point the App calls
    /// [`Session::commit`](crate::session::Session::commit) to
    /// fold it into the running session. On failure the batch is
    /// dropped, which is what keeps a half-applied state from
    /// polluting the analyzer.
    pub batch: SessionBatch,
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
    session: &Session,
) -> Result<Prepared, LowerError> {
    let mut noop = NoopObserver;
    prepare_with_observer(input, cwd, session, &mut noop)
}

/// Same as [`prepare`], with an extra hook the loader fires per
/// parsed file. Used by the perf regression test to assert that a
/// new batch only parses its own content (plus any transitive
/// `src` imports), never the entire prior-session prelude.
pub fn prepare_with_observer(
    input: &str,
    cwd: &Path,
    session: &Session,
    observer: &mut dyn vw_htcl::LoadObserver,
) -> Result<Prepared, LowerError> {
    let workspace_dir = find_workspace_dir(cwd);
    let resolver = build_resolver(workspace_dir.as_deref());

    let scratch_dir = workspace_dir
        .as_deref()
        .map(Utf8Path::as_std_path)
        .unwrap_or(cwd);

    // The scratch contains ONLY the new input — never a prepended
    // prelude. Prior batches contribute parsed signatures and proc
    // locations directly via `session`, so we never re-parse the
    // entire session on each keystroke. This is what keeps the
    // REPL responsive after several `src @lib` imports have built
    // up hundreds of thousands of lines of wrapper declarations.
    let scratch = ScratchFile::new(scratch_dir, input)?;

    let program = vw_htcl::load_program_with_observer(
        &scratch.path,
        &resolver,
        observer,
    )?;
    let parsed = vw_htcl::parse(&program.source);

    if let Some(err) = parsed.errors.first() {
        let idx = LineIndex::new(&program.source);
        let (start, _) = idx.range(err.span);
        let where_ =
            render_location(&program, err.span, start.line + 1, &scratch.path);
        return Err(LowerError::Parse(format!("{where_}: {}", err.message)));
    }

    // Validator runs first so unknown-keyword-call errors land
    // before we ship anything. Prior-batch signatures are merged
    // in so calls to wrappers from earlier inputs resolve. These
    // are hard errors (not pre-flight warnings); routing them back
    // as `LowerError` keeps the App's existing error-rendering
    // path unchanged.
    let prior_sigs = session.signature_table();
    let validator_diags = vw_htcl::validate_with_signatures(
        &parsed.document,
        &program.source,
        &prior_sigs,
    );
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

    // Build the lowering table by merging prior-batch signatures
    // with the new doc's own. The new doc's entries shadow prior
    // ones (Tcl's "second `proc` redefines" semantics) — done by
    // starting from the prior table and `extend`-ing with the new
    // doc's table, since `extend` overwrites on key collision.
    let mut table = prior_sigs;
    table.extend(vw_htcl::signature_table(&parsed.document));
    let line_index = LineIndex::new(&program.source);
    let mut commands = Vec::new();
    let mut extern_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
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

    let procs = build_proc_locations(&parsed.document, &program, &scratch.path);
    // The dedicated pre-flight `collect_warnings` is gone — the
    // validator now treats "unknown call with `-flag` args" as a
    // hard error and the REPL has already returned via `LowerError`
    // above when one fires.
    let warnings: Vec<PrepareWarning> = Vec::new();

    Ok(Prepared {
        commands,
        batch: SessionBatch {
            program,
            document: parsed.document,
            procs,
        },
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

    fn empty_session() -> Session {
        Session::new()
    }

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
            &empty_session(),
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
        let prep = prepare(
            "extern::create_project -name foo\n",
            dir.path(),
            &empty_session(),
        )
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
    fn prior_batch_procs_resolve_in_next_batch() {
        // Reproduces the REPL "src @lib then call" pattern: a
        // wrapper declared in a previous batch should be visible
        // to the analyzer/lowering when we lower a bare call in
        // the next batch — and the new batch should ship only
        // its own statement (not re-emit the wrapper).
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        // Batch 1: declare the wrapper. Commit so it joins the
        // session — same flow the App follows on successful eval.
        let first = prepare(
            "namespace eval vivado {\n  \
                proc current_project {\n    \
                  @enum(0, 1) @default(0) quiet\n    \
                  @enum(0, 1) @default(0) verbose\n    \
                  @default(\"\") project\n  \
                } {\n    \
                  set cmd [list ::current_project]\n    \
                  return [{*}$cmd]\n  \
                }\n\
              }\n",
            dir.path(),
            &session,
        )
        .unwrap();
        // The first batch ships its own declaration to the worker
        // exactly once — that's what makes the wrapper exist in
        // Tcl. Subsequent batches must NOT re-emit it.
        assert!(
            first
                .commands
                .iter()
                .any(|c| c.tcl.contains("namespace eval")),
            "first batch must ship the namespace decl: {:?}",
            first.commands
        );
        session.commit(first.batch);

        // Batch 2: bare call to the wrapper. Should ship as-is
        // (htcl is keyword-only at the call site; the wrapper
        // parses its own kwargs at runtime via the ::vw::kwargs
        // prelude), with no rewriting and no re-emission of the
        // prior batch's declaration.
        let prep =
            prepare("vivado::current_project\n", dir.path(), &session).unwrap();
        assert_eq!(prep.commands.len(), 1, "{:?}", prep.commands);
        assert!(
            prep.commands[0].tcl.contains("vivado::current_project"),
            "{}",
            prep.commands[0].tcl
        );
        // And nothing in the new batch's source mentions the
        // wrapper body — we never re-parsed the prior batch.
        assert!(
            !prep.batch.program.source.contains("namespace eval vivado"),
            "{}",
            prep.batch.program.source
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
            &empty_session(),
        )
        .unwrap();
        assert!(prep.warnings.is_empty(), "{:?}", prep.warnings);
    }

    #[test]
    fn lowers_plain_proc_call_to_tcl() {
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare("puts hello", dir.path(), &empty_session()).unwrap();
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
            prepare("set x 1\nset y 2\nset z 3", dir.path(), &empty_session())
                .unwrap();
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
        let prep = prepare("src @dep", dir.path(), &empty_session()).unwrap();
        let loc = prep
            .batch
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

        let prep = prepare("src @mid", dir.path(), &empty_session()).unwrap();
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

        let prep = prepare("src @dep", dir.path(), &empty_session()).unwrap();
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

    #[test]
    fn second_batch_parses_only_its_own_files() {
        // Regression guard against the lag bug: after `src @dep`
        // commits, a subsequent bare call must NOT cause the
        // loader to re-parse the dep's files. We assert by hooking
        // the loader's per-file observer and counting parses on
        // each batch.
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            dep.join("module.htcl"),
            "namespace eval lib {\n  \
              proc f { @default(0) x } { return $x }\n\
            }\n",
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

        #[derive(Default)]
        struct Counter {
            parsed: Vec<PathBuf>,
        }
        impl vw_htcl::LoadObserver for Counter {
            fn on_parsed(&mut self, file: &Path, _raw: Option<&str>) {
                self.parsed.push(file.to_path_buf());
            }
        }

        let mut session = Session::new();

        // First batch: imports the dep. Two files parse — the
        // entry scratch and the dep's module.htcl.
        let mut counter = Counter::default();
        let first = prepare_with_observer(
            "src @dep\n",
            dir.path(),
            &session,
            &mut counter,
        )
        .unwrap();
        assert_eq!(
            counter.parsed.len(),
            2,
            "first batch should parse entry + dep, got {:?}",
            counter.parsed
        );
        session.commit(first.batch);

        // Second batch: bare call to the wrapper. The prior
        // batch's signatures are merged in via `session`, so the
        // loader must NOT re-read the dep's file — only the new
        // scratch parses.
        let mut counter = Counter::default();
        let _second = prepare_with_observer(
            "lib::f -x 1\n",
            dir.path(),
            &session,
            &mut counter,
        )
        .unwrap();
        assert_eq!(
            counter.parsed.len(),
            1,
            "second batch should parse only the new input, got {:?}",
            counter.parsed
        );
        // And the one file parsed is the scratch, not the dep.
        let only = &counter.parsed[0];
        assert!(
            !only.starts_with(&dep),
            "the dep's files must not be re-parsed on a fresh \
             batch: {:?}",
            only
        );
    }

    #[test]
    fn prior_batch_proc_location_survives_for_drilldown() {
        // The user-reported bug: `src @vivado-cmd` declares
        // `vivado::create_bd_design` in batch A, then a later
        // `vivado::create_bd_design -name metroid` fires in batch
        // B and the Tcl error frame names that proc. The
        // proc-location lookup must resolve to the REAL .htcl
        // file the wrapper came from — not the disposable scratch
        // path of either batch.
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("vivado_cmd");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            dep.join("module.htcl"),
            "namespace eval vivado {\n  \
              proc create_bd_design {\n    \
                @default(\"\") name\n  \
              } {\n    \
                set cmd [list ::create_bd_design]\n    \
                return [{*}$cmd]\n  \
              }\n\
            }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("vw.toml"),
            format!(
                "[workspace]\nname=\"t\"\nversion=\"0.1.0\"\n\n\
                 [dependencies.vivado-cmd]\npath = \"{}\"\n",
                dep.display()
            ),
        )
        .unwrap();

        let mut session = Session::new();
        // Batch A: pull the wrapper in.
        let first = prepare("src @vivado-cmd\n", dir.path(), &session).unwrap();
        session.commit(first.batch);

        // Batch B: call the wrapper. Look up its location through
        // the session — which is exactly the path the App's error
        // renderer takes when resolving a Tcl drill-down frame.
        let _second = prepare(
            "vivado::create_bd_design -name metroid\n",
            dir.path(),
            &session,
        )
        .unwrap();
        let loc = session.lookup_proc("vivado::create_bd_design").expect(
            "wrapper from a prior `src @vivado-cmd` batch must be \
             reachable through session.lookup_proc",
        );
        // The crucial assertion: the file pointer is the REAL
        // imported .htcl, not `None` (the scratch) and not some
        // huge synthetic offset.
        let file = loc.file.as_ref().expect(
            "wrapper from imported module must carry its real \
             .htcl path, not the disposable scratch",
        );
        assert!(
            file.ends_with("vivado_cmd/module.htcl"),
            "expected the imported module path, got {:?}",
            file
        );
        // And `body_start_line` is the file-local line of the
        // proc body opener — small, not a combined-scratch offset.
        assert!(
            loc.body_start_line < 100,
            "body_start_line should be a small file-local number, \
             got {}",
            loc.body_start_line
        );
    }
}
