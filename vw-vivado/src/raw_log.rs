// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Helpers for the raw Vivado byte-log — the ground-truth file every
//! `vw run` / `vw repl` session writes so users have an unfiltered
//! record of what Vivado emitted, independent of the block classifier
//! and log-level filters.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Compute the raw-log path for a workspace: creates
/// `<workspace>/target/logs/` if it doesn't exist and returns the
/// timestamped file path inside it. The filename encodes the
/// wall-clock time at the call site as `vivado-<YYYYMMDD>-<HHMMSS>.log`
/// so runs sort in start order.
///
/// The file is created here, empty, to claim the name: two sessions
/// starting in the same second — concurrent builds of two variants on
/// one cloud instance, which share `target/logs` — would otherwise both
/// write `vivado-<same second>.log`. The second gets
/// `vivado-<YYYYMMDD>-<HHMMSS>-2.log`, and so on.
///
/// Returns `Err` only when the parent directory couldn't be created —
/// the caller can decide whether to abort the session or continue
/// without a raw log (typically the latter, since the log is a
/// diagnostic aid, not a build artifact).
pub fn raw_log_path_for_workspace(
    workspace: &Path,
) -> std::io::Result<PathBuf> {
    let dir = workspace.join("target").join("logs");
    std::fs::create_dir_all(&dir)?;
    let slug = timestamp_slug();
    for n in 1.. {
        let name = match n {
            1 => format!("vivado-{slug}.log"),
            n => format!("vivado-{slug}-{n}.log"),
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("an unbounded counter always finds a free name")
}

/// Format the current wall-clock time as `YYYYMMDD-HHMMSS`. Manually
/// computed from `UNIX_EPOCH` rather than pulling in `chrono` — the
/// output is one filename component, and vw-vivado has no other
/// reason to depend on a date/time crate.
///
/// Uses UTC because local-time conversion requires reading `/etc/
/// localtime`, which fails on some minimal containers, and the log
/// filename doesn't need human-friendly local-time semantics — it
/// only has to be monotonic within a session.
fn timestamp_slug() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = split_unix_epoch(now);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Break a Unix timestamp (seconds since 1970-01-01 UTC) into
/// `(year, month, day, hour, minute, second)`. Deliberately
/// self-contained — the raw-log module has one caller and doesn't
/// warrant a chrono dependency for what amounts to a filename slug.
///
/// The algorithm is Zeller's congruence-style month/day extraction
/// via the "civil from days" formula popularized by Howard Hinnant
/// (public domain, `date` C++ library). Correct across the Gregorian
/// range vw ever runs in.
fn split_unix_epoch(secs: u64) -> (u64, u32, u32, u32, u32, u32) {
    let second = (secs % 60) as u32;
    let mins = secs / 60;
    let minute = (mins % 60) as u32;
    let hours = mins / 60;
    let hour = (hours % 24) as u32;
    let mut days = (hours / 24) as i64;
    // Shift epoch so day 0 is 0000-03-01 (Hinnant's convention: months
    // run March→February to make leap-day handling uniform).
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u64; // day of era
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era
    let year_shifted = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (year_shifted + if month <= 2 { 1 } else { 0 }) as u64;
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_slug_shape() {
        let s = timestamp_slug();
        // Format is YYYYMMDD-HHMMSS = 15 chars, digit + hyphen split.
        assert_eq!(s.len(), 15, "unexpected slug shape: {s}");
        assert_eq!(s.as_bytes()[8], b'-');
        assert!(s[..8].bytes().all(|b| b.is_ascii_digit()));
        assert!(s[9..].bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn split_unix_epoch_at_zero_is_1970_01_01() {
        assert_eq!(split_unix_epoch(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn split_unix_epoch_known_timestamps() {
        // 2026-06-25 12:34:56 UTC = 1782390896 (verified via
        // `date -u -d '2026-06-25 12:34:56' +%s`).
        assert_eq!(split_unix_epoch(1782390896), (2026, 6, 25, 12, 34, 56));
        // Leap-day boundary: 2024-02-29 00:00:00 UTC = 1709164800
        assert_eq!(split_unix_epoch(1709164800), (2024, 2, 29, 0, 0, 0));
        // Y2038 boundary + 1 second: 2038-01-19 03:14:08 UTC
        assert_eq!(split_unix_epoch(2147483648), (2038, 1, 19, 3, 14, 8));
    }

    /// Concurrent sessions sharing `target/logs` (two variants built at
    /// once on one cloud instance) start within the same second.
    #[test]
    fn sessions_started_together_get_their_own_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..5)
            .map(|_| raw_log_path_for_workspace(tmp.path()).unwrap())
            .collect();
        let unique: std::collections::HashSet<&PathBuf> =
            paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "{paths:#?}");
        assert!(paths.iter().all(|p| p.is_file()));
    }

    #[test]
    fn raw_log_path_creates_target_logs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = raw_log_path_for_workspace(tmp.path()).unwrap();
        assert!(path.starts_with(tmp.path().join("target").join("logs")));
        // Directory was created as a side effect.
        assert!(tmp.path().join("target").join("logs").is_dir());
        // File name matches vivado-<slug>.log.
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("vivado-"), "unexpected name: {name}");
        assert!(name.ends_with(".log"), "unexpected name: {name}");
    }
}
