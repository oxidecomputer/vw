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
    /// Declared return type of the expression this command
    /// evaluates, when knowable from static analysis. `None` for
    /// expressions whose head we couldn't resolve to a known proc
    /// (untyped calls, control flow, raw Tcl, etc.). The App uses
    /// this to suppress the Result push entirely on `unit` and to
    /// skip the heuristic fallback formatter on every other typed
    /// case (since the wrapped `tcl` already returns a formatted
    /// string from the type's `repr` proc).
    pub expected_return_type: Option<vw_htcl::TypeExpr>,
    /// True when the top-level command is `set VAR <expr>` — a
    /// binding operation. The App suppresses the Result echo for
    /// these: the user picked a name to hold the value; they
    /// didn't ask to see it. `puts $VAR` is the explicit
    /// "show me" form when they want the value displayed.
    ///
    /// Applies to the LITERAL `set` command only. `[set x y]`
    /// inside a bracketed expression doesn't count — that's a
    /// nested Tcl call, not a top-level binding.
    pub is_set_binding: bool,
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
    /// Top-level statements that lived directly in the entry file
    /// (the user's `--load` target, or the typed REPL input),
    /// regardless of whether they lowered to any Tcl. Captured so
    /// the `--load` echo path can show `src` directives next to
    /// the calls that produce Tcl — without this, `src @vivado-cmd`
    /// would never get its `›` echo because its lowering is empty
    /// (consumed at load time by the loader).
    pub entry_top_level: Vec<Origin>,
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
    // before we ship anything. Prior-batch signatures + types +
    // enums + top-level var names are merged in so calls, type
    // refs, and `$var` references to prior-batch state all
    // resolve. These are hard errors (not pre-flight warnings);
    // routing them back as `LowerError` keeps the App's existing
    // error-rendering path unchanged.
    let prior_sigs = session.signature_table();
    let prior_types = session.type_decl_table();
    let prior_vars = session.top_level_var_names();
    // Prior-batch variable TYPES for the putr rewrite. Without
    // this seed, `putr $prior_var` at a fresh prompt would fall
    // through to plain `puts` (the var came from a previous
    // batch's `set`, invisible to the current parse alone) and
    // dump the raw tagged Tcl list instead of dispatching
    // through the type's `repr`.
    let prior_var_types = session.top_level_var_types();

    // Build the `putr` rewrite map: for every `putr <expr>`
    // command in the document, the value's replacement Tcl. The
    // lowering consults this map per-command via
    // `vw_htcl::lower_command_with_putr`. Empty when the source
    // contained no `putr` calls; safe (and cheap) to build
    // unconditionally.
    let putr_map = vw_htcl::putr::rewrite_with_extras(
        &program.source,
        &parsed.document,
        &prior_sigs,
        &prior_var_types,
    );
    // Names of every dep the workspace resolver knows about.
    // Passed to the validator so `src @<name>` where `<name>`
    // isn't in vw.toml fires a spanned Error diagnostic.
    let dep_names: std::collections::HashSet<String> =
        resolver.deps().map(|(name, _)| name.to_string()).collect();
    let validator_diags = vw_htcl::validate_with_all_extras_and_vars(
        &parsed.document,
        &program.source,
        &prior_sigs,
        &prior_types,
        &std::collections::HashMap::new(),
        &prior_vars,
        &dep_names,
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
    // Parse the *raw* input (not `program.source`) to capture every
    // top-level statement as the user wrote it, including `src`
    // directives. The loader rewrites `src` into the imported file's
    // content before parsing `program.source`, so the loader-expanded
    // document no longer contains a Stmt::Command for `src @foo`.
    // We need that statement to drive the `--load` echo path.
    let entry_top_level: Vec<Origin> = {
        let entry_parsed = vw_htcl::parse(input);
        let entry_idx = LineIndex::new(input);
        let mut out = Vec::new();
        for stmt in &entry_parsed.document.stmts {
            let vw_htcl::Stmt::Command(cmd) = stmt else {
                continue;
            };
            let (line, _) = entry_idx.range(cmd.span);
            let snippet = input[cmd.span.start as usize..cmd.span.end as usize]
                .trim_end()
                .to_string();
            out.push(Origin {
                file: None,
                line: line.line + 1,
                snippet,
                via: Vec::new(),
            });
        }
        out
    };

    let mut commands = Vec::new();
    let mut extern_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    // Auto-emit machinery for enums + overload dispatchers. Both
    // ship as synthetic PreparedCommand entries up front so the
    // user's statements (which may construct enum values or call
    // overloaded procs) find the supporting Tcl already in scope.
    // The classification + overload-table build also re-runs the
    // multi-decl signature collection — diagnostics from THAT pass
    // already fired through the validator above, so we discard
    // them here.
    let mut _ignored_diags = Vec::new();
    let enum_decl_table =
        vw_htcl::build_enum_decl_table(&parsed.document, &mut _ignored_diags);
    // Merge prior-batch type declarations so wrap_with_repr can
    // see newtypes declared in earlier `src @lib` batches (e.g.
    // `type Properties = dict<string,Property>` from
    // @vivado-cmd, when the user types
    // `util::props -object $cips` at a later REPL prompt).
    // Without this merge, the wrap can't recurse into
    // Properties's underlying to ship
    // `dict_string_Property::repr`, and the user's
    // `Properties::repr` body fails with `invalid command
    // name`.
    let mut type_decl_table = session.type_decl_table();
    let batch_type_decls =
        vw_htcl::build_type_decl_table(&parsed.document, &mut _ignored_diags);
    for (name, td) in batch_type_decls {
        type_decl_table.insert(name, td);
    }
    let newtype_names: std::collections::HashSet<String> =
        type_decl_table.keys().cloned().collect();
    let (_full_sig_table, overload_table) =
        vw_htcl::build_signature_table_with_overloads(
            &parsed.document,
            &newtype_names,
            &mut _ignored_diags,
        );
    for ed in enum_decl_table.values() {
        let prelude = vw_htcl::emit_enum_prelude(ed);
        if prelude.is_empty() {
            continue;
        }
        commands.push(PreparedCommand {
            tcl: prelude,
            origin: Origin {
                file: None,
                line: 0,
                snippet: format!(
                    "<enum {}>",
                    ed.name.as_deref().unwrap_or("?")
                ),
                via: Vec::new(),
            },
            expected_return_type: None,
            is_set_binding: false,
        });
    }
    for info in overload_table.values() {
        let dispatcher = vw_htcl::emit_dispatcher(info);
        commands.push(PreparedCommand {
            tcl: dispatcher,
            origin: Origin {
                file: None,
                line: 0,
                snippet: format!("<dispatcher {}>", info.public_name),
                via: Vec::new(),
            },
            expected_return_type: None,
            is_set_binding: false,
        });
    }

    // Eagerly emit the primitive repr prelude (string/int/bool/
    // unit) so user procs that call e.g. `extern::string::repr`
    // from inside their bodies see those procs in scope. Without
    // this, the primitives are only emitted by `wrap_with_repr`
    // at top-level REPL eval sites, leaving inner uses dead.
    for proc in vw_htcl::repr::emit_primitive_prelude() {
        commands.push(PreparedCommand {
            tcl: proc,
            origin: Origin {
                file: None,
                line: 0,
                snippet: "<primitive repr>".into(),
                via: Vec::new(),
            },
            expected_return_type: None,
            is_set_binding: false,
        });
    }

    // Eagerly emit monomorphized generic reprs for every declared
    // type alias whose underlying is a generic
    // (`dict<…>` / `list<…>`). Without this, a user-written
    // `T::repr` body that delegates to the compiler-synthesized
    // monomorphized name (e.g. `Properties::repr` calling
    // `extern::dict_string_Property::repr`) errors at runtime
    // when invoked from inside a proc body — `wrap_with_repr`
    // only emits the monomorphization chain at top-level REPL
    // eval sites, not for inner uses. By emitting here, the
    // procs are in scope everywhere within the session.
    //
    // Dedup-by-text within the batch prevents shipping the same
    // monomorphization more than once when two type aliases
    // resolve to the same underlying generic.
    let mut emitted_mono_reprs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for td in type_decl_table.values() {
        let Some(underlying) = td.underlying.as_ref() else {
            continue;
        };
        if !matches!(underlying, vw_htcl::TypeExpr::Generic { .. }) {
            continue;
        }
        let emission =
            vw_htcl::repr::emit_repr_with_types(underlying, &type_decl_table);
        for proc in emission.procs {
            if !emitted_mono_reprs.insert(proc.clone()) {
                continue;
            }
            commands.push(PreparedCommand {
                tcl: proc,
                origin: Origin {
                    file: None,
                    line: 0,
                    snippet: format!(
                        "<mono repr for {}>",
                        td.name.as_deref().unwrap_or("?")
                    ),
                    via: Vec::new(),
                },
                expected_return_type: None,
                is_set_binding: false,
            });
        }
    }

    for stmt in &parsed.document.stmts {
        let vw_htcl::Stmt::Command(cmd) = stmt else {
            continue;
        };
        let (line_one_based, _) = line_index.range(cmd.span);
        let origin = build_origin(
            &program,
            cmd.span,
            line_one_based.line + 1,
            &scratch.path,
        );
        // If this command is a proc that's been classified as an
        // overload specialization, lower it under its mangled name
        // so the dispatcher (shipped above) can find it. Otherwise
        // take the normal path.
        let lowered_raw =
            match overload_specialization_mangle(cmd, &overload_table) {
                Some(mangled) => {
                    let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
                        unreachable!()
                    };
                    vw_htcl::lower_proc_decl_with_name(
                        proc,
                        &program.source,
                        &table,
                        Some(&mangled),
                        &putr_map,
                    )
                }
                None => vw_htcl::lower_command_with_putr(
                    cmd,
                    &program.source,
                    &table,
                    &putr_map,
                ),
            };
        let rewritten = vw_htcl::rewrite_externs(&lowered_raw);
        for name in rewritten.names {
            extern_names.insert(name);
        }
        if rewritten.text.trim().is_empty() {
            continue;
        }
        // Resolve the command's expected return type and, if any,
        // wrap the lowered Tcl so it dispatches through the type's
        // `repr` proc. The wrapped form runs the user's expression
        // into a sentinel local then formats via the repr; the
        // sentinel-binding step preserves `set var [...]`-style
        // bindings (the user's `$var` still gets the raw value).
        let expected_return_type = resolve_return_type(cmd, &table);
        let final_tcl = match expected_return_type.as_ref() {
            Some(ty) => wrap_with_repr(&rewritten.text, ty, &type_decl_table),
            None => rewritten.text,
        };
        let is_set_binding = matches!(cmd.kind, vw_htcl::CommandKind::Set);
        commands.push(PreparedCommand {
            tcl: final_tcl,
            origin,
            expected_return_type,
            is_set_binding,
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
        entry_top_level,
    })
}

/// Walk every proc declaration (top-level + nested inside
/// `namespace eval` blocks) and record its body's source location
/// keyed by the proc's qualified name. Same recursion shape as
/// `vw_htcl::validate::collect_signatures` — kept in sync by
/// convention rather than refactor so this crate stays a leaf
/// consumer of vw-htcl.
pub fn build_proc_locations(
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

/// Return the declared return type of `cmd`'s head call, when we
/// can resolve it from the signature table. Currently handles two
/// shapes:
///
/// - Direct call: `proc-name arg arg …` → look up `proc-name`'s
///   return type in the table.
/// - Bracket-bound assignment: `set var [proc-name …]` → look up
///   the inner bracketed call's return type (since `set` returns
///   the value being set, which is the type of the inner call).
///
/// Anything else (control flow, variable substitution, raw Tcl,
/// unknown commands) returns `None`. The App falls back to the
/// untyped-display path for those.
fn resolve_return_type(
    cmd: &vw_htcl::ast::Command,
    table: &std::collections::HashMap<String, &vw_htcl::ProcSignature>,
) -> Option<vw_htcl::TypeExpr> {
    let head = cmd.words.first()?.as_text()?;
    if head == "set" {
        // `set var [EXPR]` → recurse into the bracketed
        // expression on the third word (words[2]). Other `set`
        // shapes (set var literal, set var $other) leave the
        // type unknown — we'd need real expression type-inference
        // to do better, and that's out of scope for v1.
        let val_word = cmd.words.get(2)?;
        // Look for a CmdSubst part — `[…]` — at the top of the
        // value word. If found, recurse into the bracketed
        // command's first statement.
        for part in &val_word.parts {
            if let vw_htcl::WordPart::CmdSubst { body, .. } = part {
                let vw_htcl::Stmt::Command(inner) = body.first()? else {
                    continue;
                };
                return resolve_return_type(inner, table);
            }
        }
        return None;
    }
    let sig = table.get(head)?;
    sig.return_type.clone()
}

/// Wrap the lowered Tcl `inner` so that, after evaluating it, the
/// result is fed through `<ty>::repr` (or the appropriate
/// monomorphized generic repr) to produce a display string.
///
/// Prepends:
///   1. The primitive prelude (`string` / `int` / `bool` / `unit`
///      triplets) — cheap to redefine per-eval; Tcl `proc`
///      redefinition is idempotent.
///   2. Any per-instantiation generic reprs needed for `ty`, in
///      topological order so each proc is defined before its
///      dependents call it.
///   3. `set __vw_result [<inner>]` — captures the user expression's
///      raw value into a sentinel local. This preserves any
///      `set var [...]` bindings the user wrote, since `set`'s
///      side effect runs before our sentinel-capture wraps it.
///   4. `<dispatch> $__vw_result` — calls the type's repr proc on
///      the captured value. The eval returns this formatted string.
fn wrap_with_repr(
    inner: &str,
    ty: &vw_htcl::TypeExpr,
    types: &std::collections::HashMap<String, &vw_htcl::TypeDecl>,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for p in vw_htcl::repr::emit_primitive_prelude() {
        out.push_str(&p);
    }
    // Walks the dispatch type's underlying when `ty` is a newtype
    // — necessary for `Properties` (newtype wrapping
    // `dict<string,Property>`) so the body of `Properties::repr`
    // can call the monomorphized `dict_string_Property::repr`.
    let emission = vw_htcl::repr::emit_repr_with_types(ty, types);
    for p in &emission.procs {
        out.push_str(p);
    }
    writeln!(out, "set __vw_result [{}]", inner.trim_end())
        .expect("writeln to String never fails");
    // All reprs (compiler-emitted primitives + generics + user-
    // written newtype reprs + auto-generated enum reprs) share a
    // single `{args}` envelope that uses `::vw::kwargs` to bind
    // `$v`. The dispatch site always calls them as
    // `<dispatch> -v <val>` so the kwargs envelope binds
    // uniformly regardless of which class of repr is being
    // invoked.
    write!(out, "{} -v $__vw_result", emission.dispatch)
        .expect("write to String never fails");
    out
}

/// If `cmd` is a top-level `proc` whose name appears in the
/// overload table AND whose first arg is a qualified-variant
/// annotation, return the mangled internal name that this
/// specialization should lower under. Otherwise `None`.
///
/// This is what reroutes user-written `proc handle_prop {v:
/// Property::Scalar} { … }` from emitting under the literal
/// `handle_prop` name (which would collide with the synthesized
/// dispatcher) to emitting under `__handle_prop__Scalar` (which
/// the dispatcher's switch arm calls).
fn overload_specialization_mangle(
    cmd: &vw_htcl::Command,
    overloads: &vw_htcl::OverloadTable,
) -> Option<String> {
    let vw_htcl::CommandKind::Proc(proc) = &cmd.kind else {
        return None;
    };
    let name = proc.name.as_deref()?;
    if !overloads.contains_key(name) {
        return None;
    }
    let sig = proc.signature.as_ref()?;
    let first = sig.args.first()?;
    let vw_htcl::TypeExpr::Qualified { variant, .. } =
        first.type_annotation.as_ref()?
    else {
        return None;
    };
    Some(vw_htcl::mangle_specialization(name, variant))
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

    /// User-statement commands only — strips the synthetic
    /// prelude entries (enum reprs, overload dispatchers,
    /// primitive reprs, monomorphized generic reprs) the
    /// preparer ships before each batch. Tests that assert
    /// command count / shape only care about what the user
    /// wrote, not the prelude scaffolding.
    fn user_commands(prep: &Prepared) -> Vec<&PreparedCommand> {
        prep.commands
            .iter()
            .filter(|c| !c.origin.snippet.starts_with('<'))
            .collect()
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
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 1, "{:?}", cmds);
        assert!(
            cmds[0].tcl.contains("create_project -name foo"),
            "{}",
            cmds[0].tcl
        );
        assert!(!cmds[0].tcl.contains("extern::"), "{}", cmds[0].tcl);
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
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 1, "{:?}", cmds);
        assert!(
            cmds[0].tcl.contains("vivado::current_project"),
            "{}",
            cmds[0].tcl
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
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].tcl.contains("puts hello"));
        // Input is at line 1 of the buffer.
        assert_eq!(cmds[0].origin.line, 1);
        assert!(cmds[0].origin.file.is_none());
        assert_eq!(cmds[0].origin.snippet, "puts hello");
    }

    #[test]
    fn each_statement_gets_its_own_origin() {
        let dir = tempfile::tempdir().unwrap();
        let prep =
            prepare("set x 1\nset y 2\nset z 3", dir.path(), &empty_session())
                .unwrap();
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].origin.line, 1);
        assert_eq!(cmds[1].origin.line, 2);
        assert_eq!(cmds[2].origin.line, 3);
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
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 1);
        let origin = &cmds[0].origin;
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
        let cmds = user_commands(&prep);
        assert_eq!(cmds.len(), 2);
        for cmd in &cmds {
            let file = cmd.origin.file.as_ref().expect("import has file");
            assert!(file.ends_with("dep/module.htcl"), "{:?}", file);
        }
        // Line numbers point into the imported file.
        assert_eq!(cmds[0].origin.line, 1);
        assert_eq!(cmds[1].origin.line, 2);
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

    // --- typed-expression wrap (step 3) ----------------------------

    #[test]
    fn typed_proc_call_wraps_with_repr_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        // A proc annotated dict<string,string>, called bare. The
        // wrap should:
        //   - capture the call's result into __vw_result
        //   - invoke the monomorphized dict repr proc on it
        // PreparedCommand.expected_return_type carries the type.
        let prep = prepare(
            "proc props {} dict<string,string> { return {} }\n\
             props\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        // Two commands: proc decl (drops to empty Tcl) + call.
        // proc decl ships as a regular command; the call should
        // be wrapped.
        let call = prep
            .commands
            .iter()
            .find(|c| c.tcl.contains("__vw_result"))
            .expect("expected the `props` call to be repr-wrapped");
        assert!(
            call.tcl.contains("set __vw_result [props]"),
            "tcl: {}",
            call.tcl
        );
        assert!(
            call.tcl
                .contains("dict_string_string::repr -v $__vw_result"),
            "tcl: {}",
            call.tcl
        );
        // Primitive prelude is included so the dict repr's
        // element calls (string::repr) resolve. Both the
        // primitive procs and the monomorphized generic procs
        // are wrapped in explicit `namespace eval` blocks so
        // Tcl's namespace-conflict heuristic doesn't reject the
        // declaration (the bare `proc string::repr` form trips
        // over Tcl's built-in `string` command).
        assert!(
            call.tcl.contains("namespace eval string"),
            "expected primitive prelude in wrapped tcl: {}",
            call.tcl
        );
        // Plus the dict repr itself.
        assert!(
            call.tcl.contains("namespace eval dict_string_string"),
            "expected monomorphized dict repr: {}",
            call.tcl
        );
        // The expected_return_type rides along for App-side use.
        let ty = call
            .expected_return_type
            .as_ref()
            .expect("expected_return_type set");
        match ty {
            vw_htcl::TypeExpr::Generic { name, args, .. } => {
                assert_eq!(name, "dict");
                assert_eq!(args.len(), 2);
            }
            _ => panic!("expected Generic, got {:?}", ty),
        }
    }

    #[test]
    fn set_var_call_inherits_inner_return_type() {
        // `set cips [props]` — `set` returns the value being set,
        // so its type is whatever `props` returns. The wrap should
        // bind `$cips` correctly AND dispatch on the inner call's
        // declared type.
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "proc props {} dict<string,string> { return {} }\n\
             set x [props]\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        let set_cmd = prep
            .commands
            .iter()
            .find(|c| c.tcl.contains("__vw_result"))
            .expect("expected the `set x [...]` to be repr-wrapped");
        assert!(
            set_cmd.tcl.contains("set __vw_result [set x [props]]"),
            "expected the original set to be inner-wrapped: {}",
            set_cmd.tcl
        );
        assert!(
            set_cmd
                .tcl
                .contains("dict_string_string::repr -v $__vw_result"),
            "tcl: {}",
            set_cmd.tcl
        );
    }

    #[test]
    fn unannotated_call_is_not_wrapped() {
        // No return type → no wrap, no `__vw_result` capture,
        // and `expected_return_type` is None.
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "proc plain {} { return whatever }\n\
             plain\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        let plain_call = prep
            .commands
            .iter()
            .find(|c| c.tcl.trim() == "plain")
            .expect("expected raw `plain` call without wrap");
        assert!(plain_call.expected_return_type.is_none());
        assert!(
            !plain_call.tcl.contains("__vw_result"),
            "unannotated calls shouldn't get the repr wrap: {}",
            plain_call.tcl
        );
    }

    #[test]
    fn unit_typed_call_is_wrapped_with_unit_dispatch() {
        // `unit`-typed expressions still get wrapped — the wrap
        // returns the empty string from `unit::repr`. The App's
        // EvalDone handler is what suppresses the Result push;
        // the lowerer is uniform.
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "proc do_thing {} unit { puts hi }\n\
             do_thing\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        let call = prep
            .commands
            .iter()
            .find(|c| c.tcl.contains("__vw_result"))
            .expect("expected repr-wrap on unit-typed call");
        assert!(call.tcl.contains("unit::repr -v $__vw_result"));
        let ty = call.expected_return_type.as_ref().unwrap();
        match ty {
            vw_htcl::TypeExpr::Named { name, .. } => {
                assert_eq!(name, "unit");
            }
            _ => panic!(),
        }
    }

    // --- enum / overload pipeline (step 5) -------------------------

    #[test]
    fn enum_decl_ships_namespace_eval_prelude() {
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "enum Direction = {\n  North\n  South\n}\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        // The prelude is shipped as a synthetic PreparedCommand.
        let prelude = prep
            .commands
            .iter()
            .find(|c| c.tcl.contains("namespace eval Direction"))
            .expect("expected enum prelude in prepared commands");
        assert!(prelude.tcl.contains("proc North {}"));
        assert!(prelude.tcl.contains("proc South {}"));
        assert!(prelude.tcl.contains("proc tag {v}"));
        assert!(prelude.tcl.contains("proc payload {v}"));
        assert!(prelude.tcl.contains("proc repr {args}"));
    }

    #[test]
    fn overload_set_ships_dispatcher_and_mangled_specializations() {
        let dir = tempfile::tempdir().unwrap();
        let prep = prepare(
            "enum E = {\n  A: string\n  B: int\n}\n\
             proc f {v: E::A} string { return $v }\n\
             proc f {v: E::B} string { return $v }\n",
            dir.path(),
            &empty_session(),
        )
        .unwrap();
        // Dispatcher emitted with the switch body. The dispatcher
        // takes the standard kwargs envelope (`{args}`), walks
        // kwargs for `-v <enum-value>`, then switches on the
        // tag.
        let dispatcher = prep
            .commands
            .iter()
            .find(|c| {
                c.tcl.contains("proc f {args}")
                    && c.tcl.contains("switch")
                    && c.tcl.contains("__f__")
            })
            .expect("expected dispatcher for `f`");
        assert!(dispatcher.tcl.contains("__f__A"));
        assert!(dispatcher.tcl.contains("__f__B"));
        // Specializations emitted under mangled names — look for
        // the `proc __f__A` declaration (rather than `proc f`).
        assert!(
            prep.commands
                .iter()
                .any(|c| c.tcl.contains("proc __f__A {args}")),
            "expected specialization under __f__A: tcls={:?}",
            prep.commands.iter().map(|c| &c.tcl).collect::<Vec<_>>()
        );
        assert!(
            prep.commands
                .iter()
                .any(|c| c.tcl.contains("proc __f__B {args}")),
            "expected specialization under __f__B"
        );
        // The user-visible name `f` should NOT appear as a
        // user-procedure declaration — only as the dispatcher.
        // (The dispatcher's body has `proc f {v args}` which we
        // already accounted for above; what we're guarding
        // against is a leaked `proc f {args} { ::vw::kwargs ... }`
        // specialization.)
        let leaked_f = prep
            .commands
            .iter()
            .filter(|c| c.tcl.contains("proc f {args} { ::vw::kwargs"))
            .count();
        assert_eq!(
            leaked_f, 0,
            "specialization should NOT have leaked under public name `f`"
        );
    }

    #[test]
    fn cross_batch_newtype_recursion_emits_generic_repr() {
        // Reproduces the user-reported regression: batch 1
        // declares `type Properties = dict<string,string>` and a
        // proc returning Properties; batch 2 calls that proc.
        // The wrap_with_repr in batch 2 should walk Properties's
        // underlying (the dict generic) and emit
        // `dict_string_string::repr` so the user's
        // `Properties::repr` body can find it.
        //
        // Pre-fix: type_decl_table was per-batch, so batch 2
        // couldn't see Properties; the recursion didn't fire;
        // dict_string_string::repr was never emitted; the
        // user's body errored with `invalid command name`.
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        let first = prepare(
            "type Properties = {dict<string,string>}\n\
             proc Properties::repr {v} { return $v }\n\
             proc Properties::from {v} { return $v }\n\
             proc Properties::to {v} { return $v }\n\
             proc get_props {} Properties { return {a 1 b 2} }\n",
            dir.path(),
            &session,
        )
        .unwrap();
        session.commit(first.batch);

        // Batch 2: just the call. parsed.document doesn't have
        // the type decl — it must come from `session`.
        let second = prepare("get_props\n", dir.path(), &session).unwrap();
        let call = second
            .commands
            .iter()
            .find(|c| c.tcl.contains("__vw_result"))
            .expect("expected wrapped call to get_props");
        // The wrap must include the monomorphized dict repr
        // (reached by recursing through Properties's underlying).
        assert!(
            call.tcl.contains("namespace eval dict_string_string"),
            "expected dict_string_string::repr in wrap (newtype \
             recursion across batches): {}",
            call.tcl
        );
        // And the top-level dispatch goes through Properties::repr
        // with the `-v` form, not positional.
        assert!(
            call.tcl.contains("Properties::repr -v $__vw_result"),
            "expected Properties::repr dispatch via -v form: {}",
            call.tcl
        );
    }
}
