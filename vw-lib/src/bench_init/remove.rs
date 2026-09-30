// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Deleting a testbench.
//!
//! The counterpart to `init`, and it exists for the same reason: a testbench
//! is not only its own files. A cosim or mixed-signal bench is also a member
//! of the bench cargo workspace, and deleting the directory by hand leaves
//! that member behind — pointing at nothing, which stops cargo loading the
//! workspace manifest at all and so breaks *every* bench rather than the one
//! that was meant to go.
//!
//! Removal is planned before it is done. [`plan`] says exactly what would be
//! deleted without touching anything, so a caller can show it and ask; only
//! [`apply`] removes anything.

use camino::{Utf8Path, Utf8PathBuf};

use crate::{Result, VwError};

/// Which sort of bench a caller means.
///
/// Named rather than inferred so that `vw mist remove fifo` refuses to delete
/// a Rust cosim bench that happens to be called `fifo`: the command a
/// developer typed says what they think is there, and acting on something
/// else because the name matched is how the wrong thing gets deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchKind {
    Vhdl,
    Cosim,
    Mist,
}

impl BenchKind {
    fn describe(self) -> &'static str {
        match self {
            BenchKind::Vhdl => "pure VHDL testbench",
            BenchKind::Cosim => "Rust cosim testbench",
            BenchKind::Mist => "mixed-signal testbench",
        }
    }

    fn command(self) -> &'static str {
        match self {
            BenchKind::Vhdl => "vw bench remove",
            BenchKind::Cosim => "vw cosim remove",
            BenchKind::Mist => "vw mist remove",
        }
    }
}

/// What removing a bench would delete.
#[derive(Clone, Debug)]
pub struct Removal {
    /// What `vw bench run` calls it.
    pub name: String,
    pub kind: BenchKind,
    /// Everything to be deleted, in the order it will go.
    pub paths: Vec<Utf8PathBuf>,
    /// How many files are under `paths`, for a caller that wants to say.
    pub file_count: usize,
    /// The bench cargo workspace member to drop, if there is one.
    pub member: Option<String>,
}

/// Work out what removing `name` would take with it. Deletes nothing.
pub fn plan(
    workspace_dir: &Utf8Path,
    kind: BenchKind,
    name: &str,
) -> Result<Removal> {
    let bench_dir = workspace_dir.join("bench");
    let (name, mut paths, member) = match kind {
        BenchKind::Vhdl => {
            let base = super::base_name(name);
            let entity = format!("{base}_tb");
            let file = bench_dir.join(format!("{entity}.vhd"));
            if !file.exists() {
                return Err(not_found(workspace_dir, kind, &base));
            }
            (entity, vec![file], None)
        }
        BenchKind::Cosim | BenchKind::Mist => {
            let dir = bench_dir.join(name);
            if !dir.is_dir() {
                return Err(not_found(workspace_dir, kind, name));
            }
            check_kind(&dir, kind, name)?;
            (name.to_string(), vec![dir], Some(name.to_string()))
        }
    };

    // Build output is regenerated on the next run and belongs to the bench,
    // so leaving it behind would only be litter with nothing to produce it
    // again.
    let output = crate::bench_output_dir(workspace_dir, &name);
    if output.exists() {
        paths.push(output);
    }

    let file_count = paths.iter().map(count_files).sum();

    Ok(Removal {
        name,
        kind,
        paths,
        file_count,
        member,
    })
}

/// Carry out a [`plan`].
pub fn apply(workspace_dir: &Utf8Path, removal: &Removal) -> Result<()> {
    for path in &removal.paths {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path.as_std_path())
        } else {
            std::fs::remove_file(path.as_std_path())
        };
        // Already gone is the outcome that was wanted.
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(VwError::FileSystem {
                    message: format!("removing {path}: {e}"),
                })
            }
        }
    }

    // Last, and only once the files are actually gone: a member left pointing
    // at a directory that still exists is harmless, one pointing at nothing
    // is not.
    if let Some(member) = &removal.member {
        super::workspace::unregister_member(workspace_dir, member)?;
    }
    Ok(())
}

/// Refuse to delete a bench of a different kind than the caller asked for.
fn check_kind(dir: &Utf8Path, kind: BenchKind, name: &str) -> Result<()> {
    let actual = if dir.join("mist.toml").exists() {
        BenchKind::Mist
    } else if dir.join("cosim.toml").exists()
        || super::cosim::is_cosim_crate(dir)
    {
        BenchKind::Cosim
    } else {
        return Err(VwError::Config {
            message: format!(
                "bench/{name} is not a testbench — it has neither a \
                 cosim.toml, a mist.toml, nor a cdylib crate. Delete it by \
                 hand if that is what you meant."
            ),
        });
    };

    if actual != kind {
        return Err(VwError::Config {
            message: format!(
                "bench/{name} is a {}, not a {} — use `{} {name}`",
                actual.describe(),
                kind.describe(),
                actual.command(),
            ),
        });
    }
    Ok(())
}

/// A "no such bench" that says what *is* there, since a wrong name and a
/// wrong command look identical from the outside.
fn not_found(workspace_dir: &Utf8Path, kind: BenchKind, name: &str) -> VwError {
    let bench_dir = workspace_dir.join("bench");
    let elsewhere = match kind {
        // A directory of that name is the other two kinds' shape.
        BenchKind::Vhdl => bench_dir.join(name).is_dir().then(|| {
            format!(
                " — bench/{name} is a directory, so try `vw cosim remove \
                 {name}` or `vw mist remove {name}`"
            )
        }),
        // A file of that name is a pure VHDL bench.
        _ => bench_dir
            .join(format!("{}_tb.vhd", super::base_name(name)))
            .exists()
            .then(|| {
                format!(
                    " — bench/{}_tb.vhd is a pure VHDL testbench, so try `vw \
                     bench remove {name}`",
                    super::base_name(name),
                )
            }),
    };

    VwError::Config {
        message: format!(
            "no {} called '{name}' under bench/{}",
            kind.describe(),
            elsewhere.unwrap_or_default(),
        ),
    }
}

/// How many files are under a path, counting the path itself if it is one.
fn count_files(path: &Utf8PathBuf) -> usize {
    if path.is_file() {
        return 1;
    }
    let Ok(entries) = std::fs::read_dir(path.as_std_path()) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match Utf8PathBuf::from_path_buf(entry.path()) {
            Ok(path) => count_files(&path),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VhdlStandard;

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let ws =
            Utf8PathBuf::from_path_buf(guard.path().to_path_buf()).unwrap();
        std::fs::write(ws.join("vw.toml"), "[workspace]\nname = \"d\"\n")
            .unwrap();
        (guard, ws)
    }

    /// The whole point: the crate goes and so does its workspace membership.
    /// A member left pointing at nothing stops cargo loading the bench
    /// workspace, which breaks every other bench.
    #[test]
    fn removing_a_cosim_bench_takes_its_workspace_member_with_it() {
        let (_guard, ws) = workspace();
        super::super::cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            VhdlStandard::Vhdl2019,
        )
        .unwrap();
        super::super::cosim::init(
            &ws,
            "other",
            None,
            &[],
            VhdlStandard::Vhdl2019,
        )
        .unwrap();

        let removal = plan(&ws, BenchKind::Cosim, "fifo").unwrap();
        assert_eq!(removal.member.as_deref(), Some("fifo"));
        assert!(removal.file_count >= 4);
        // Planning alone changes nothing.
        assert!(ws.join("bench/fifo").exists());

        apply(&ws, &removal).unwrap();
        assert!(!ws.join("bench/fifo").exists());
        assert!(ws.join("bench/other").exists());

        let manifest =
            std::fs::read_to_string(ws.join("bench/Cargo.toml")).unwrap();
        assert!(!manifest.contains("\"fifo\""));
        assert!(manifest.contains("\"other\""));
    }

    /// A pure VHDL bench is one file and no membership.
    #[test]
    fn removing_a_vhdl_bench_takes_its_one_file() {
        let (_guard, ws) = workspace();
        super::super::vhdl::init(&ws, "widget", None, VhdlStandard::Vhdl2019)
            .unwrap();

        // Typed either way, it is the same bench.
        let removal = plan(&ws, BenchKind::Vhdl, "widget_tb").unwrap();
        assert_eq!(removal.name, "widget_tb");
        assert_eq!(removal.member, None);

        apply(&ws, &removal).unwrap();
        assert!(!ws.join("bench/widget_tb.vhd").exists());
    }

    /// Asking the wrong command to delete something is refused, with the
    /// right one named. Deleting the wrong bench is not recoverable from the
    /// tool.
    #[test]
    fn the_wrong_command_will_not_delete_the_right_bench() {
        let (_guard, ws) = workspace();
        super::super::cosim::init(
            &ws,
            "fifo",
            None,
            &[],
            VhdlStandard::Vhdl2019,
        )
        .unwrap();

        let error = plan(&ws, BenchKind::Mist, "fifo").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Rust cosim"), "{message}");
        assert!(message.contains("vw cosim remove fifo"), "{message}");

        assert!(ws.join("bench/fifo").exists());
    }

    /// A name that is not there says so, and points at the bench that is.
    #[test]
    fn a_missing_bench_says_what_is_actually_there() {
        let (_guard, ws) = workspace();
        super::super::vhdl::init(&ws, "widget", None, VhdlStandard::Vhdl2019)
            .unwrap();

        let error = plan(&ws, BenchKind::Cosim, "widget").unwrap_err();
        assert!(
            error.to_string().contains("vw bench remove widget"),
            "{error}",
        );

        assert!(plan(&ws, BenchKind::Vhdl, "nothing").is_err());
    }

    /// Build output belongs to the bench and is regenerated on the next run,
    /// so it goes too rather than being left with nothing to produce it.
    #[test]
    fn build_output_is_removed_along_with_the_bench() {
        let (_guard, ws) = workspace();
        super::super::vhdl::init(&ws, "widget", None, VhdlStandard::Vhdl2019)
            .unwrap();
        let output = crate::bench_output_dir(&ws, "widget_tb");
        std::fs::create_dir_all(output.as_std_path()).unwrap();
        std::fs::write(output.join("widget_tb.fst"), "wave").unwrap();

        let removal = plan(&ws, BenchKind::Vhdl, "widget").unwrap();
        assert!(removal.paths.contains(&output));

        apply(&ws, &removal).unwrap();
        assert!(!output.exists());
    }
}
