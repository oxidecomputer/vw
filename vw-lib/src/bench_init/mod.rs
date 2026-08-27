// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Creating a testbench.
//!
//! Three kinds of bench live under `bench/`, and until now all three were
//! made by hand:
//!
//! | | what it is | `vw bench run` finds it by |
//! |---|---|---|
//! | pure VHDL | one file, an entity with no ports | an entity named `*_tb` |
//! | Rust cosim | a wrapper entity plus a `cdylib` crate driving it | the same |
//! | mixed-signal | a `mist.toml`, a Xyce circuit, and a bridge crate | the directory |
//!
//! The cosim one is the worst of them: a crate that has to be a `cdylib`, a
//! `build.rs` that has to find the GPI libraries, a registration in the bench
//! cargo workspace, and a wrapper entity that has to obey two unwritten rules
//! about what a cosim top level may contain. Every one of those is a
//! separate way to end up with a bench that builds and then does nothing, and
//! the failures are far from their causes.
//!
//! So none of it is written by hand any more. `vw bench init`, `vw cosim
//! init` and `vw mist init` each produce a bench that runs, and passes,
//! before a line of test logic exists — which is the point. A developer finds
//! out the plumbing works first, and is then left with only the part that is
//! actually their job.
//!
//! Pass `--dut` (or `--entity`) and the design's interface is read out of the
//! workspace and wired up: a signal per port with the port's own type, the
//! generic map, the port map, a Rust handle per port. See [`entity`].

pub mod cosim;
pub mod entity;
pub mod mist;
pub mod remove;
pub mod vhdl;
pub mod workspace;

use camino::{Utf8Path, Utf8PathBuf};

use entity::{DesignTypes, EntityInterface, Interface};

use crate::{Result, VwError};

/// What an `init` produced.
#[derive(Clone, Debug)]
pub struct Created {
    /// What `vw bench run` will call it — the entity name for a VHDL or
    /// cosim bench, the directory name for a mixed-signal one.
    pub name: String,
    /// Every file written, in the order they were written.
    pub files: Vec<Utf8PathBuf>,
    /// Whether the bench cargo workspace gained a member.
    pub registered: bool,
    /// Whether this brought an existing bench up to date rather than making
    /// a new one. `vw cosim init` is re-runnable, and saying "created" about
    /// a bench somebody has been working in for a week reads as a warning.
    pub updated: bool,
    /// What is left for the developer to do, in the order to do it.
    pub next_steps: Vec<String>,
}

// ===========================================================================
// Names
// ===========================================================================

/// Strip a `_tb` the developer may have typed.
///
/// `vw bench init fifo` and `vw bench init fifo_tb` mean the same thing, and
/// the second is the more natural thing to type when the file it produces is
/// going to be called `fifo_tb.vhd`. Accepting both and normalizing is
/// cheaper than being right about which one people will use.
fn base_name(name: &str) -> String {
    let trimmed = name.trim();
    let lower = trimmed.to_lowercase();
    if lower.ends_with("_tb") && trimmed.len() > 3 {
        trimmed[..trimmed.len() - 3].to_string()
    } else {
        trimmed.to_string()
    }
}

/// A name that will be a VHDL entity has to be a VHDL identifier.
///
/// Checked here rather than left to nvc: a bench named `2fast` produces four
/// files and a workspace registration before anything tries to compile it,
/// and the failure would arrive with no hint that the name caused it.
fn check_vhdl_identifier(name: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(VwError::Config {
            message: format!("'{name}' cannot be a testbench name: {why}"),
        })
    };

    match name.chars().next() {
        None => return invalid("it is empty"),
        Some(c) if !c.is_ascii_alphabetic() => {
            return invalid("a VHDL identifier has to start with a letter")
        }
        _ => {}
    }
    if let Some(c) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_')
    {
        return invalid(&format!(
            "a VHDL identifier is letters, digits and underscores — '{c}' is \
             not one of them"
        ));
    }
    if name.ends_with('_') {
        return invalid("a VHDL identifier cannot end in an underscore");
    }
    if name.contains("__") {
        return invalid(
            "a VHDL identifier cannot have two underscores in a row",
        );
    }
    Ok(())
}

/// A name that will only ever be a directory and a crate.
///
/// Looser than [`check_vhdl_identifier`] because a mixed-signal bench has no
/// entity of its own — `tx-eq` is a perfectly good name for one.
fn check_crate_name(name: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(VwError::Config {
            message: format!("'{name}' cannot be a testbench name: {why}"),
        })
    };

    match name.chars().next() {
        None => return invalid("it is empty"),
        Some(c) if !c.is_ascii_alphabetic() => {
            return invalid("a crate name has to start with a letter")
        }
        _ => {}
    }
    if let Some(c) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
    {
        return invalid(&format!(
            "a crate name is letters, digits, underscores and hyphens — \
             '{c}' is not one of them"
        ));
    }
    Ok(())
}

/// Refuse to write into a directory that is already a bench.
fn check_available(dir: &Utf8Path) -> Result<()> {
    if dir.exists() {
        return Err(VwError::Config {
            message: format!(
                "{dir} already exists — pick another name, or delete it if \
                 that bench is finished with"
            ),
        });
    }
    Ok(())
}

/// A port name as a Rust field name.
///
/// VHDL identifiers are already valid Rust ones with one exception: a handful
/// of them are Rust keywords. Those get a trailing underscore, while the name
/// used to look the signal up stays what the VHDL says.
fn rust_field(port: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "box", "break", "const", "continue", "crate",
        "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl",
        "in", "let", "loop", "match", "mod", "move", "mut", "override", "priv",
        "pub", "ref", "return", "self", "static", "struct", "super", "trait",
        "true", "type", "typeof", "unsafe", "unsized", "use", "virtual",
        "where", "while", "yield",
    ];
    if KEYWORDS.contains(&port) {
        format!("{port}_")
    } else {
        port.to_string()
    }
}

// ===========================================================================
// Shared rendering
// ===========================================================================

/// The `library` / `use` clauses a generated VHDL file opens with.
///
/// The DUT's own clauses come first and verbatim, because the signals below
/// them are declared with the DUT's types and those types are only visible
/// through them. `ieee.std_logic_1164` and `numeric_std` are added when the
/// DUT did not already bring them in — a bench uses both whatever the design
/// does.
fn vhdl_context(dut: Option<&EntityInterface>) -> Vec<String> {
    let mut clauses: Vec<String> =
        dut.map(|d| d.context.clone()).unwrap_or_default();

    let has = |clauses: &[String], needle: &str| {
        clauses.iter().any(|c| c.to_lowercase().contains(needle))
    };
    if !has(&clauses, "library ieee") {
        clauses.insert(0, "library ieee;".to_string());
    }
    if !has(&clauses, "std_logic_1164") {
        clauses.push("use ieee.std_logic_1164.all;".to_string());
    }
    if !has(&clauses, "numeric_std") {
        clauses.push("use ieee.numeric_std.all;".to_string());
    }
    clauses
}

/// A VHDL literal that zeroes a signal of this subtype, if there is an
/// obvious one.
///
/// Resolved through the design's subtype declarations rather than read off
/// the port: `Nibble` takes `(others => '0')` and `Pam4Symbol` does too,
/// while nothing about either name says as much. A record, an array of
/// composites, or a type that could not be found gets nothing — guessing
/// wrong is worse than leaving it to the developer.
fn zero_literal(subtype: &str, types: &DesignTypes) -> Option<String> {
    zero_literal_at(subtype, types, 0)
}

fn zero_literal_at(
    subtype: &str,
    types: &DesignTypes,
    depth: usize,
) -> Option<String> {
    match types.resolve(subtype).as_str() {
        "boolean" => Some("false".to_string()),
        "std_logic" | "std_ulogic" | "bit" => Some("'0'".to_string()),
        "std_logic_vector" | "std_ulogic_vector" | "bit_vector"
        | "unsigned" | "signed" => Some("(others => '0')".to_string()),
        "integer" | "natural" | "positive" => Some("0".to_string()),
        // A record's elements need not have the same type, so `(others =>
        // …)` will not do it — but a named aggregate will, and every element
        // is known. Worth the trouble: an uninitialized record input reaches
        // the design as all-'U' and floods the run with metavalue warnings
        // before any check gets to fail.
        _ if depth < 4 => {
            let fields = types.record(subtype)?;
            let elements: Option<Vec<String>> = fields
                .iter()
                .map(|field| {
                    zero_literal_at(&field.subtype, types, depth + 1)
                        .map(|value| format!("{} => {value}", field.name))
                })
                .collect();
            let elements = elements?;
            (!elements.is_empty()).then(|| format!("({})", elements.join(", ")))
        }
        _ => None,
    }
}

/// A stand-in for a generic the entity declared without a default.
///
/// `1` rather than `0` for the integer family: most such generics are widths
/// or counts, where zero is either illegal (`positive`) or degenerate.
fn placeholder(generic: &Interface, types: &DesignTypes) -> Option<String> {
    match types.resolve(&generic.subtype).as_str() {
        "integer" | "natural" | "positive" => Some("1".to_string()),
        other => zero_literal(other, types),
    }
}

// ===========================================================================
// Repair
// ===========================================================================

/// Put back what every cosim bench needs and nobody edits.
///
/// Run before the benches build, alongside the mixed-signal scaffolding in
/// [`crate::ensure_bench_scaffolds`], and for the same reason: the file it
/// restores is derived from where `rust-cosim` put its libraries, so a copy
/// that is missing (a fresh clone, a `git clean -fdx`) or was written by an
/// older `vw` fails at link time with an error that says nothing about any of
/// that.
///
/// Only files carrying vw's own marker are rewritten — see [`cosim::heal`].
pub fn heal_cosim_scaffolds(workspace_dir: &Utf8Path) -> Result<()> {
    for dir in cosim_crates(&workspace_dir.join("bench")) {
        cosim::heal(&dir)?;
    }
    Ok(())
}

/// Every cosim crate under `bench/`, at any depth.
///
/// Depth matters: benches are commonly grouped into subdirectories by the
/// part of the design they cover, and a grouped bench is no less in need of a
/// `build.rs` than a top-level one.
fn cosim_crates(bench_dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![bench_dir.to_owned()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            // `target` is cargo's, and a dot-directory is nobody's business.
            if name == "target" || name.starts_with('.') {
                continue;
            }
            if cosim::is_cosim_crate(&path) {
                found.push(path);
            } else {
                pending.push(path);
            }
        }
    }

    found.sort();
    found
}

// ===========================================================================
// Files
// ===========================================================================

/// Write a file, refusing to replace one that is already there.
///
/// Used for everything a bench owns. `init` writing over a testbench somebody
/// has been working on would be unrecoverable, and the cost of being wrong
/// about a name is one error message.
fn write_new_file(path: &Utf8Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Err(VwError::Config {
            message: format!(
                "{path} already exists — pick another name, or delete it if \
                 that bench is finished with"
            ),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent.as_std_path())?;
    }
    std::fs::write(path.as_std_path(), contents).map_err(|e| {
        VwError::FileSystem {
            message: format!("writing {path}: {e}"),
        }
    })?;
    Ok(())
}

/// Write a file only when its content would change. Returns whether it did.
///
/// Used for everything a bench does *not* own. Skipping an unchanged write
/// keeps the mtime still, which keeps cargo's incremental build hot — this
/// runs before every bench, and a `build.rs` touched each time would rebuild
/// the crate each time.
fn write_file(path: &Utf8Path, contents: &str) -> Result<bool> {
    if std::fs::read_to_string(path.as_std_path())
        .is_ok_and(|existing| existing == contents)
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent.as_std_path())?;
    }
    std::fs::write(path.as_std_path(), contents).map_err(|e| {
        VwError::FileSystem {
            message: format!("writing {path}: {e}"),
        }
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings of the same intent land in the same place.
    #[test]
    fn a_trailing_tb_is_optional() {
        assert_eq!(base_name("fifo"), "fifo");
        assert_eq!(base_name("fifo_tb"), "fifo");
        assert_eq!(base_name("fifo_TB"), "fifo");
        // Not a suffix, just a short name.
        assert_eq!(base_name("tb"), "tb");
    }

    /// A name that cannot be a VHDL entity is caught before four files and a
    /// workspace registration exist.
    #[test]
    fn names_that_cannot_be_entities_are_refused() {
        assert!(check_vhdl_identifier("fifo").is_ok());
        assert!(check_vhdl_identifier("fifo2").is_ok());
        assert!(check_vhdl_identifier("2fifo").is_err());
        assert!(check_vhdl_identifier("tx-eq").is_err());
        assert!(check_vhdl_identifier("fifo_").is_err());
        assert!(check_vhdl_identifier("fi__fo").is_err());
        assert!(check_vhdl_identifier("").is_err());
    }

    /// A mixed-signal bench is a directory, not an entity, so a hyphen is
    /// fine there and only there.
    #[test]
    fn a_mixed_signal_name_may_be_hyphenated() {
        assert!(check_crate_name("tx-eq").is_ok());
        assert!(check_crate_name("tx eq").is_err());
        assert!(check_crate_name("1tx").is_err());
    }

    /// A port that happens to be a Rust keyword still gets a field, and the
    /// name the simulator is asked for is unchanged.
    #[test]
    fn keyword_ports_get_a_usable_field_name() {
        assert_eq!(rust_field("data"), "data");
        assert_eq!(rust_field("type"), "type_");
        assert_eq!(rust_field("loop"), "loop_");
    }

    /// An unchanged file is not rewritten — the whole reason cargo does not
    /// rebuild every bench on every run.
    #[test]
    fn writing_the_same_content_twice_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("f")).unwrap();

        assert!(write_file(&path, "one").unwrap());
        assert!(!write_file(&path, "one").unwrap());
        assert!(write_file(&path, "two").unwrap());
    }

    /// A cosim bench is five files in three places plus a registration, and
    /// the point of the command is that all of them appear together. A
    /// missing one is a bench that builds and then does nothing.
    #[test]
    fn a_cosim_init_produces_a_complete_bench() {
        let guard = tempfile::tempdir().unwrap();
        let ws =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "[workspace]\nname = \"d\"\n")
            .unwrap();

        let created = cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            None,
            crate::VhdlStandard::Vhdl2019,
        )
        .unwrap();

        assert_eq!(created.name, "fifo");
        assert!(created.registered);
        for expected in [
            "bench/Cargo.toml",
            "bench/.gitignore",
            "bench/.cargo/config.toml",
            "bench/fifo/cosim.toml",
            "bench/fifo/Cargo.toml",
            "bench/fifo/src/lib.rs",
            "bench/fifo/build.rs",
            "bench/fifo/.gitignore",
        ] {
            assert!(ws.join(expected).exists(), "{expected} was not written",);
        }

        // The crate has to be a member or it does not build, and a member
        // that is not on disk stops the whole workspace loading.
        let manifest =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        assert!(manifest.contains("\"fifo\""));

        // ...and a second bench joins it rather than replacing it.
        cosim::init(
            &ws,
            "other",
            None,
            &[],
            None,
            crate::VhdlStandard::Vhdl2019,
        )
        .unwrap();
        let manifest =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        assert!(manifest.contains("\"fifo\""));
        assert!(manifest.contains("\"other\""));
    }

    /// A bench grows a piece at a time. Re-running adds what was asked for
    /// and regenerates, and the test written against the last one survives —
    /// which is the whole reason the generated half is a separate file.
    #[test]
    fn a_second_init_adds_to_the_bench_rather_than_replacing_it() {
        let guard = tempfile::tempdir().unwrap();
        let ws =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "[workspace]\nname = \"d\"\n")
            .unwrap();

        cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            None,
            crate::VhdlStandard::Vhdl2019,
        )
        .unwrap();
        std::fs::write(ws.join("bench/fifo/src/lib.rs"), "// my test").unwrap();

        // Running again is not an error, and does not touch what is mine.
        cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            Some(250e6),
            crate::VhdlStandard::Vhdl2019,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(ws.join("bench/fifo/src/lib.rs")).unwrap(),
            "// my test",
            "the test is written once and then left alone",
        );
        // ...while what the command line said is recorded and acted on.
        let config =
            std::fs::read_to_string(ws.join("bench/fifo/cosim.toml")).unwrap();
        assert!(config.contains("clock = 2.5e8"), "{config}");
        // The generated half is there and is not the file I edited.
        assert!(ws.join("bench/fifo/src/generated.rs").exists());
    }

    /// A directory that is not a cosim bench is still refused: only one this
    /// command made is one it may add to.
    #[test]
    fn init_will_not_adopt_a_directory_it_did_not_make() {
        let guard = tempfile::tempdir().unwrap();
        let ws =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "[workspace]\nname = \"d\"\n")
            .unwrap();
        std::fs::create_dir_all(ws.join("bench/fifo").as_std_path()).unwrap();
        std::fs::write(ws.join("bench/fifo/notes.md"), "mine").unwrap();

        assert!(cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            None,
            crate::VhdlStandard::Vhdl2019,
        )
        .is_err());
    }

    /// The repair pass finds benches wherever they are filed, since they are
    /// commonly grouped by the part of the design they cover.
    #[test]
    fn nested_cosim_crates_are_found_for_repair() {
        let guard = tempfile::tempdir().unwrap();
        let ws =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();
        let nested = ws.join("bench/parsers/ipv4_test");
        std::fs::create_dir_all(nested.as_std_path()).unwrap();
        std::fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname = \"ipv4_test\"\n[lib]\ncrate-type = [\"cdylib\"]\n",
        )
        .unwrap();

        heal_cosim_scaffolds(&ws).unwrap();
        assert!(nested.join("build.rs").exists());
    }

    /// Nothing an `init` writes may land on top of existing work.
    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("f")).unwrap();

        write_new_file(&path, "mine").unwrap();
        assert!(write_new_file(&path, "theirs").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");
    }
}
