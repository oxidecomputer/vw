// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Persistent input history with incremental search.
//!
//! Entries are appended to a newline-delimited file (one entry per
//! line, with embedded newlines escaped) under the platform's state
//! dir — typically `~/.local/state/vw/repl-history` on Linux. The
//! file is loaded once at startup; new entries are appended both to
//! memory and to the file as soon as they're recorded so a crashed
//! session doesn't lose history.
//!
//! Ctrl-R triggers an *incremental* search: as the user types, we
//! find the most recent entry whose text contains the query as a
//! substring (case-insensitive). Repeated Ctrl-R steps to the next-
//! older match. Esc cancels; Enter accepts the match into the input
//! buffer.

use std::fs::{create_dir_all, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

const ESCAPED_NEWLINE: &str = "\\n";
const ESCAPED_BACKSLASH: &str = "\\\\";

/// In-memory history, backed by an on-disk file. Indexed
/// most-recent-last; `entries[entries.len() - 1]` is the freshest
/// record, matching how Readline / Reedline order things.
#[derive(Debug)]
pub struct History {
    file_path: PathBuf,
    entries: Vec<String>,
}

impl History {
    /// Load history from the default location. Returns an empty
    /// store (and skips disk writes) when no state dir is available
    /// — the REPL still runs, just without persistence.
    pub fn load_default() -> Self {
        let path = default_history_path();
        match path {
            Some(p) => Self::load_from(p),
            None => Self {
                file_path: PathBuf::new(),
                entries: Vec::new(),
            },
        }
    }

    /// Load history from a specific file. Missing file → empty
    /// history (the file gets created on first append).
    pub fn load_from(file_path: PathBuf) -> Self {
        let entries = read_entries(&file_path).unwrap_or_default();
        Self { file_path, entries }
    }

    #[allow(dead_code)] // public API for the in-progress completion slice
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Append `entry` to the in-memory log and persist it. Empty or
    /// whitespace-only entries are ignored. An entry identical to
    /// the most recent one is also ignored (the common case of
    /// re-running the same command shouldn't bloat the file).
    pub fn append(&mut self, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.entries.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if self.file_path.as_os_str().is_empty() {
            return;
        }
        if let Some(parent) = self.file_path.parent() {
            let _ = create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
        {
            let _ = writeln!(f, "{}", encode_line(entry));
        }
    }

    /// Find the most recent entry whose text contains `query` as a
    /// substring (case-insensitive). Returns the index in
    /// [`Self::entries`] plus the entry itself. `start_before` is an
    /// exclusive upper bound — passing `Some(prev_idx)` resumes the
    /// search at the next-older entry, which is how repeated
    /// `Ctrl-R` steps backward.
    pub fn search_back(
        &self,
        query: &str,
        start_before: Option<usize>,
    ) -> Option<(usize, &str)> {
        if query.is_empty() {
            return None;
        }
        let upper = start_before.unwrap_or(self.entries.len());
        let needle = query.to_lowercase();
        for i in (0..upper).rev() {
            if self.entries[i].to_lowercase().contains(&needle) {
                return Some((i, self.entries[i].as_str()));
            }
        }
        None
    }
}

fn default_history_path() -> Option<PathBuf> {
    let state = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(state.join("vw").join("repl-history"))
}

fn read_entries(path: &PathBuf) -> Option<Vec<String>> {
    let f = std::fs::File::open(path).ok()?;
    let mut entries = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        entries.push(decode_line(&line));
    }
    Some(entries)
}

fn encode_line(s: &str) -> String {
    // Single-line newline-delimited file format: backslashes and
    // embedded newlines get a literal escape so a multi-line htcl
    // buffer round-trips cleanly.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(ESCAPED_BACKSLASH),
            '\n' => out.push_str(ESCAPED_NEWLINE),
            other => out.push(other),
        }
    }
    out
}

fn decode_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn append_persists_and_dedupes_consecutive_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h");
        let mut h = History::load_from(p.clone());
        h.append("foo");
        h.append("foo");
        h.append("bar");
        h.append("");
        h.append("   ");
        assert_eq!(h.entries(), &["foo".to_string(), "bar".to_string()]);
        // File round-trips.
        let mut buf = String::new();
        std::fs::File::open(&p)
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "foo\nbar\n");
    }

    #[test]
    fn multiline_entries_round_trip_with_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h");
        {
            let mut h = History::load_from(p.clone());
            h.append("set x [\n  create_cpm5 -name cpm5\n]");
            h.append("with \\ backslash");
        }
        let h2 = History::load_from(p);
        assert_eq!(
            h2.entries(),
            &[
                "set x [\n  create_cpm5 -name cpm5\n]".to_string(),
                "with \\ backslash".to_string(),
            ]
        );
    }

    #[test]
    fn search_back_finds_most_recent_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load_from(dir.path().join("h"));
        h.append("set x 1");
        h.append("create_project foo");
        h.append("set y 2");
        let (idx, hit) = h.search_back("set", None).unwrap();
        assert_eq!(hit, "set y 2");
        assert_eq!(idx, 2);
        // Step to the next older.
        let (idx2, hit2) = h.search_back("set", Some(idx)).unwrap();
        assert_eq!(hit2, "set x 1");
        assert_eq!(idx2, 0);
        // Nothing older.
        assert!(h.search_back("set", Some(idx2)).is_none());
    }

    #[test]
    fn search_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load_from(dir.path().join("h"));
        h.append("Create_Project foo");
        let (_, hit) = h.search_back("create", None).unwrap();
        assert_eq!(hit, "Create_Project foo");
    }
}
