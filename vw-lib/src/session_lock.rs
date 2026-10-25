// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! One vw session at a time per build directory.
//!
//! Everything under `target/<variant>/` assumes a single owner. Preparing a
//! session can wipe the persisted Vivado project; configuring IP rewrites it;
//! each stage wipes and rewrites its own checkpoint directory; IP synthesis
//! has worker Vivados reading the project while it runs. A second session on
//! the same directory — another `vw run`, a `vw repl`, `vw check
//! --ip-generate` — would do any of that to the first one's project
//! underneath it. So a session takes this lock before touching the
//! directory and keeps it until its Vivado is gone.
//!
//! It is an advisory file lock, so the operating system drops it when the
//! holder exits however it exits, and a crashed session never leaves the
//! directory locked.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};

use crate::{build_dir, Result, VwError};

/// Held for as long as a session owns its build directory. Dropping it
/// releases the directory.
#[derive(Debug)]
pub struct BuildLock {
    _file: File,
    path: Utf8PathBuf,
}

impl BuildLock {
    /// The lock file.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

/// Take the build directory for `variant` (or the workspace's only one) for
/// this session.
///
/// Fails without waiting when another session holds it. Waiting would turn
/// "you have a REPL open on this variant" into a command that hangs with no
/// explanation for as long as the REPL stays open; the error says who has it
/// instead.
pub fn lock_build_dir(
    workspace_dir: &Utf8Path,
    variant: Option<&str>,
) -> Result<BuildLock> {
    let dir = build_dir(workspace_dir, variant);
    fs::create_dir_all(dir.as_std_path())?;
    let path = dir.join(".vw-session.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.as_std_path())?;
    match file.try_lock() {
        Ok(()) => {
            // Who holds it, for the next session's error message. Best
            // effort: the lock is what matters, not this note.
            let since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = file.set_len(0).and_then(|()| {
                writeln!(file, "pid {} since {since}", std::process::id())
            });
            Ok(BuildLock { _file: file, path })
        }
        Err(TryLockError::WouldBlock) => {
            let mut holder = String::new();
            let _ = file
                .seek(SeekFrom::Start(0))
                .and_then(|_| file.read_to_string(&mut holder));
            let holder = match holder.trim() {
                "" => String::new(),
                h => format!(" ({h})"),
            };
            Err(VwError::FileSystem {
                message: format!(
                    "{dir} is in use by another vw session{holder}; \
                     finish or close that session (a running build, a \
                     REPL) and try again"
                ),
            })
        }
        Err(TryLockError::Error(e)) => Err(VwError::Io(e)),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_second_session_is_refused_until_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Utf8Path::from_path(tmp.path()).unwrap();
        let first = lock_build_dir(ws, None).unwrap();
        let err = lock_build_dir(ws, None).unwrap_err().to_string();
        assert!(err.contains("in use by another vw session"), "{err}");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        drop(first);
        lock_build_dir(ws, None).unwrap();
    }
}
