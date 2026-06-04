// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Recursive `src` import resolution.
//!
//! Reads an entry-point .htcl file, parses it, resolves every `src`
//! statement via [`crate::src_path::Resolver`], and recursively pulls
//! in each imported module's contents. Idempotent on canonical
//! (realpath'd) file paths — a file imported by N callers loads
//! exactly once.
//!
//! The output is a single flat [`LoadedProgram`] carrying:
//!
//! - the concatenated source text (imports first, in topological
//!   order, then the entry file's non-`src` content), which downstream
//!   stages (lower, the analyzer, `vw run`) consume as if it were one
//!   document;
//! - the set of canonical paths that were loaded, for cache
//!   invalidation and tooling.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ast::{CommandKind, Stmt};
use crate::parser::parse;
use crate::src_path::{ResolveError, Resolver};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("resolving `src {raw}` from {importer}: {source}")]
    Resolve {
        importer: PathBuf,
        raw: String,
        #[source]
        source: ResolveError,
    },
    #[error(
        "`src` import at {importer}:{line} has a non-literal path (it \
         contains `$var` or `[cmd]` substitution); module paths must \
         be a plain string"
    )]
    DynamicPath { importer: PathBuf, line: u32 },
    #[error("parse errors in {path}")]
    Parse {
        path: PathBuf,
        errors: Vec<crate::parser::ParseError>,
    },
}

/// Hooks called as the loader makes progress. Lets the CLI surface
/// real-time `Sourcing …` / `Checking …` lines without baking display
/// concerns into the loader.
///
/// Events fire in dependency-first order, which matches Cargo's
/// "compile deps before the top crate" convention: for each `src`
/// import we hit, [`on_source`](Self::on_source) fires immediately,
/// the import is loaded (recursing through *its* dependencies first),
/// and only then does [`on_parsed`](Self::on_parsed) fire for that
/// file. The entry file's `on_parsed` fires last.
pub trait LoadObserver {
    /// A `src <raw>` statement is about to be resolved and loaded.
    fn on_source(&mut self, _raw: &str) {}
    /// `file` finished parsing. `raw` is the original `src` text when
    /// this file was reached through an import (so callers can render
    /// `amd-htcl/cpm5` rather than the full filesystem path); `None`
    /// for the entry file.
    fn on_parsed(&mut self, _file: &Path, _raw: Option<&str>) {}
}

struct NoopObserver;
impl LoadObserver for NoopObserver {}

#[derive(Debug, Default)]
pub struct LoadedProgram {
    /// Flattened htcl source — every loaded file's non-`src` content,
    /// concatenated. Downstream stages (lower, the analyzer in CLI
    /// mode, `vw run`) consume this as if it were one document.
    pub source: String,
    /// Files seen, in the order [`load_file`] first visits them
    /// (importer-first, depth-first). Each entry carries the file's
    /// canonical path and its original on-disk text so callers can
    /// map a span in [`source`](Self::source) back to a line/column
    /// in the file it actually came from.
    pub files: Vec<LoadedFile>,
    /// Per-region map from byte ranges in [`source`](Self::source)
    /// to `(file_index, file_offset)`. Regions are emitted in order
    /// as content is concatenated, so the slice is sorted by
    /// `flat_start` and non-overlapping — `locate` does a binary
    /// search.
    pub regions: Vec<SourceRegion>,
}

#[derive(Debug, Clone)]
pub struct LoadedFile {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceRegion {
    /// Inclusive byte start in the flattened source.
    pub flat_start: u32,
    /// Exclusive byte end in the flattened source.
    pub flat_end: u32,
    /// Index into [`LoadedProgram::files`].
    pub file_index: u32,
    /// Byte offset of the start of this region in the originating
    /// file's source.
    pub file_offset: u32,
}

impl LoadedProgram {
    /// Map a byte offset in [`source`](Self::source) back to its
    /// originating file's index and the byte offset within that file.
    pub fn locate(&self, offset: u32) -> Option<(usize, u32)> {
        // `regions` is sorted by `flat_start`; find the last region
        // whose start is at or before `offset` and verify the offset
        // falls inside it.
        let idx = self.regions.partition_point(|r| r.flat_start <= offset);
        if idx == 0 {
            return None;
        }
        let region = &self.regions[idx - 1];
        if offset >= region.flat_end {
            return None;
        }
        Some((
            region.file_index as usize,
            region.file_offset + (offset - region.flat_start),
        ))
    }

    /// Map a span in the flattened source to `(file_index,
    /// file_local_span)`. Assumes the span lies within a single
    /// originating file's contribution — true for diagnostics emitted
    /// against a single word/command, which is the use case we care
    /// about.
    pub fn locate_span(
        &self,
        span: crate::span::Span,
    ) -> Option<(usize, crate::span::Span)> {
        let (file_index, file_start) = self.locate(span.start)?;
        let length = span.end.saturating_sub(span.start);
        Some((
            file_index,
            crate::span::Span::new(file_start, file_start + length),
        ))
    }
}

/// Read `entry` and recursively resolve its imports. Each file is
/// loaded at most once; circular imports (a → b → a) short-circuit on
/// the second visit.
pub fn load(
    entry: &Path,
    resolver: &Resolver,
) -> Result<LoadedProgram, LoadError> {
    let mut noop = NoopObserver;
    load_with_observer(entry, resolver, &mut noop)
}

/// Like [`load`], but reports progress through `observer` so the CLI
/// can print `Sourcing …` and `Checking …` lines.
pub fn load_with_observer(
    entry: &Path,
    resolver: &Resolver,
    observer: &mut dyn LoadObserver,
) -> Result<LoadedProgram, LoadError> {
    let entry = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let mut state = State {
        program: LoadedProgram::default(),
        loaded: HashSet::new(),
        in_progress: HashSet::new(),
        resolver,
        observer,
    };
    state.load_file(&entry, None)?;
    Ok(state.program)
}

struct State<'r, 'o> {
    program: LoadedProgram,
    loaded: HashSet<PathBuf>,
    in_progress: HashSet<PathBuf>,
    resolver: &'r Resolver,
    observer: &'o mut dyn LoadObserver,
}

impl State<'_, '_> {
    fn load_file(
        &mut self,
        path: &Path,
        reached_via: Option<&str>,
    ) -> Result<(), LoadError> {
        if self.loaded.contains(path) || self.in_progress.contains(path) {
            return Ok(());
        }
        self.in_progress.insert(path.to_path_buf());

        let source = fs::read_to_string(path).map_err(|e| LoadError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let parsed = parse(&source);
        if !parsed.errors.is_empty() {
            return Err(LoadError::Parse {
                path: path.to_path_buf(),
                errors: parsed.errors,
            });
        }

        // Register the file up front so we have a stable index for
        // every chunk we emit on its behalf.
        let file_index = self.program.files.len() as u32;
        self.program.files.push(LoadedFile {
            path: path.to_path_buf(),
            source: source.clone(),
        });

        // Walk the parsed document, copying text in span order. Any
        // `src` statement triggers a recursion so the imported content
        // lands in the flat source before we continue the importer's
        // remaining text. Each pushed slice gets a `SourceRegion`
        // entry so locations in the flat source can be mapped back.
        let mut cursor = 0usize;
        let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));
        for stmt in &parsed.document.stmts {
            let Stmt::Command(cmd) = stmt else { continue };
            let CommandKind::Src(import) = &cmd.kind else {
                continue;
            };
            self.emit_chunk(
                &source,
                cursor,
                cmd.span.start as usize,
                file_index,
            );
            cursor = cmd.span.end as usize;
            // Skip the trailing newline that terminated the `src`
            // command so we don't leave a stray blank line behind.
            if source.as_bytes().get(cursor) == Some(&b'\n') {
                cursor += 1;
            }

            let Some(raw) = import.path.as_deref() else {
                let line = line_of(&source, cmd.span.start) + 1;
                return Err(LoadError::DynamicPath {
                    importer: path.to_path_buf(),
                    line,
                });
            };
            let resolved =
                self.resolver.resolve(parent_dir, raw).map_err(|source| {
                    LoadError::Resolve {
                        importer: path.to_path_buf(),
                        raw: raw.to_string(),
                        source,
                    }
                })?;
            if !self.loaded.contains(&resolved)
                && !self.in_progress.contains(&resolved)
            {
                self.observer.on_source(raw);
            }
            self.load_file(&resolved, Some(raw))?;
        }
        // Tail after the last `src`.
        self.emit_chunk(&source, cursor, source.len(), file_index);
        if !self.program.source.ends_with('\n') {
            // Synthetic newline so subsequent files don't run on; no
            // region for it — it didn't come from any input file.
            self.program.source.push('\n');
        }

        self.in_progress.remove(path);
        self.loaded.insert(path.to_path_buf());
        self.observer.on_parsed(path, reached_via);
        Ok(())
    }

    /// Push `source[start..end]` onto the flat source and record a
    /// region mapping that byte range back to the file it came from.
    fn emit_chunk(
        &mut self,
        source: &str,
        start: usize,
        end: usize,
        file_index: u32,
    ) {
        if start >= end {
            return;
        }
        let flat_start = self.program.source.len() as u32;
        self.program.source.push_str(&source[start..end]);
        let flat_end = self.program.source.len() as u32;
        self.program.regions.push(SourceRegion {
            flat_start,
            flat_end,
            file_index,
            file_offset: start as u32,
        });
    }
}

fn line_of(source: &str, byte: u32) -> u32 {
    source[..(byte as usize).min(source.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn loads_a_single_file_unchanged() {
        let dir = workspace();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "puts hi\n").unwrap();
        let prog = load(&entry, &Resolver::new()).unwrap();
        assert_eq!(prog.source.trim(), "puts hi");
        assert_eq!(prog.files.len(), 1);
    }

    #[test]
    fn imports_local_file_and_drops_src_statement() {
        let dir = workspace();
        fs::write(dir.path().join("lib.htcl"), "proc f {} { puts hi }\n")
            .unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src lib\nf\n").unwrap();

        let prog = load(&entry, &Resolver::new()).unwrap();
        // Imported content first, no `src` statement, then the importer.
        assert_eq!(
            prog.source, "proc f {} { puts hi }\nf\n",
            "actual: {:?}",
            prog.source
        );
        assert_eq!(prog.files.len(), 2);
    }

    #[test]
    fn idempotent_across_diamond_imports() {
        // main → a, b ; a → c ; b → c — c must load exactly once.
        let dir = workspace();
        fs::write(dir.path().join("c.htcl"), "proc c {} {}\n").unwrap();
        fs::write(dir.path().join("a.htcl"), "src c\nproc a {} {}\n").unwrap();
        fs::write(dir.path().join("b.htcl"), "src c\nproc b {} {}\n").unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src a\nsrc b\n").unwrap();

        let prog = load(&entry, &Resolver::new()).unwrap();
        let occurrences = prog.source.matches("proc c {}").count();
        assert_eq!(occurrences, 1, "c loaded multiple times: {}", prog.source);
    }

    #[test]
    fn cycle_does_not_loop_forever() {
        let dir = workspace();
        fs::write(dir.path().join("a.htcl"), "src b\nproc a {} {}\n").unwrap();
        fs::write(dir.path().join("b.htcl"), "src a\nproc b {} {}\n").unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src a\n").unwrap();
        let prog = load(&entry, &Resolver::new()).unwrap();
        assert!(prog.source.contains("proc a"));
        assert!(prog.source.contains("proc b"));
    }

    #[test]
    fn named_dependency_resolves_through_the_cache() {
        let dir = workspace();
        let dep_root = dir.path().join("cache").join("xilinx-ip-deadbeef");
        fs::create_dir_all(&dep_root).unwrap();
        fs::write(dep_root.join("cpm5.htcl"), "proc create_cpm5 {} {}\n")
            .unwrap();
        let resolver = Resolver::new().with_dep("xilinx-ip", dep_root);
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src @xilinx-ip/cpm5\ncreate_cpm5\n").unwrap();
        let prog = load(&entry, &resolver).unwrap();
        assert!(prog.source.contains("proc create_cpm5"));
        assert!(prog.source.contains("\ncreate_cpm5\n"));
    }

    #[test]
    fn observer_fires_in_dependency_order() {
        // entry → a → c ; entry → b
        // Expect: source a, parse a (after source c, parse c),
        //         source b, parse b, parse entry.
        let dir = workspace();
        fs::write(dir.path().join("c.htcl"), "proc c {} {}\n").unwrap();
        fs::write(dir.path().join("a.htcl"), "src c\nproc a {} {}\n").unwrap();
        fs::write(dir.path().join("b.htcl"), "proc b {} {}\n").unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src a\nsrc b\n").unwrap();

        #[derive(Default)]
        struct Recorder {
            events: Vec<String>,
        }
        impl LoadObserver for Recorder {
            fn on_source(&mut self, raw: &str) {
                self.events.push(format!("source {raw}"));
            }
            fn on_parsed(&mut self, file: &Path, raw: Option<&str>) {
                let label = match raw {
                    Some(r) => r.to_string(),
                    None => file
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string(),
                };
                self.events.push(format!("parse {label}"));
            }
        }

        let mut rec = Recorder::default();
        load_with_observer(&entry, &Resolver::new(), &mut rec).unwrap();
        assert_eq!(
            rec.events,
            vec![
                "source a",
                "source c",
                "parse c",
                "parse a",
                "source b",
                "parse b",
                "parse main",
            ]
        );
    }

    #[test]
    fn observer_suppresses_source_for_already_loaded_imports() {
        // Diamond: main → a → c ; main → b → c. `c` is encountered
        // twice via `src` but only loaded once, so "Sourcing c"
        // should fire exactly once.
        let dir = workspace();
        fs::write(dir.path().join("c.htcl"), "proc c {} {}\n").unwrap();
        fs::write(dir.path().join("a.htcl"), "src c\nproc a {} {}\n").unwrap();
        fs::write(dir.path().join("b.htcl"), "src c\nproc b {} {}\n").unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src a\nsrc b\n").unwrap();

        #[derive(Default)]
        struct Counter {
            source_c: usize,
            parse_c: usize,
        }
        impl LoadObserver for Counter {
            fn on_source(&mut self, raw: &str) {
                if raw == "c" {
                    self.source_c += 1;
                }
            }
            fn on_parsed(&mut self, file: &Path, _raw: Option<&str>) {
                if file.file_stem().and_then(|s| s.to_str()) == Some("c") {
                    self.parse_c += 1;
                }
            }
        }
        let mut counter = Counter::default();
        load_with_observer(&entry, &Resolver::new(), &mut counter).unwrap();
        assert_eq!(counter.source_c, 1);
        assert_eq!(counter.parse_c, 1);
    }

    #[test]
    fn regions_map_each_byte_back_to_its_originating_file() {
        // entry uses `set` from one local file and `puts` from another.
        let dir = workspace();
        fs::write(dir.path().join("a.htcl"), "proc a {} {}\n").unwrap();
        fs::write(dir.path().join("b.htcl"), "proc b {} {}\n").unwrap();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src a\nputs hello\nsrc b\nputs done\n").unwrap();
        let prog = load(&entry, &Resolver::new()).unwrap();

        // Pick a byte in the middle of `puts hello` — should map back
        // to the entry file (main.htcl).
        let puts_hello_at_flat =
            prog.source.find("puts hello").expect("puts hello in flat") as u32;
        let (idx, file_offset) =
            prog.locate(puts_hello_at_flat).expect("locate puts hello");
        assert_eq!(
            prog.files[idx].path.file_name().and_then(|s| s.to_str()),
            Some("main.htcl")
        );
        // In main.htcl the line `puts hello` sits right after `src a\n`,
        // so file_offset is at byte 6 (`s`=0,1,2,r=3,c=4,a=5,\n=6).
        // Actually 'src a\n' = 6 bytes (s,r,c,space,a,\n), so puts starts at 6.
        assert_eq!(file_offset, 6);

        // Pick a byte in the middle of `proc a` — should map to a.htcl.
        let proc_a_at_flat =
            prog.source.find("proc a").expect("proc a in flat") as u32;
        let (idx_a, _) = prog.locate(proc_a_at_flat).expect("locate proc a");
        assert_eq!(
            prog.files[idx_a].path.file_name().and_then(|s| s.to_str()),
            Some("a.htcl")
        );
    }

    #[test]
    fn unknown_dep_surfaces_helpful_error() {
        let dir = workspace();
        let entry = dir.path().join("main.htcl");
        fs::write(&entry, "src @nope/cpm5\n").unwrap();
        let err = load(&entry, &Resolver::new()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown dependency"), "{msg}");
    }
}
