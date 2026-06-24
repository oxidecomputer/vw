// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! In-memory session document.
//!
//! Every successful user input gets appended to a growing htcl
//! script that future analyzer queries treat as the prelude.
//! Combined with whatever's in the input buffer right now, this is
//! the document the analyzer sees: variables and procs the user
//! defined earlier are in scope, calls to them validate against
//! their signatures, and completion can offer them.
//!
//! v1 just keeps the concatenated text. Subsequent slices will plug
//! this into [`vw_htcl::parse`] + [`vw_htcl::validate`] for inline
//! diagnostics on the current input, and into [`vw_htcl::complete_at`]
//! for tab completion.

/// A live REPL session: the concatenation of every input the user
/// has successfully evaluated, in order.
#[derive(Clone, Debug, Default)]
pub struct Session {
    text: String,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// The prelude — everything the user has evaluated so far.
    #[allow(dead_code)] // wired up by the in-progress completion slice
    pub fn prelude(&self) -> &str {
        &self.text
    }

    /// The prelude with `pending` appended as if the user had just
    /// submitted it. Used for analyzer queries against the current
    /// input buffer. Returns the combined source plus the byte
    /// offset where `pending` begins (callers translate cursor
    /// positions into this absolute offset).
    #[allow(dead_code)] // wired up by the in-progress completion slice
    pub fn with_pending(&self, pending: &str) -> (String, u32) {
        let mut combined = String::with_capacity(self.text.len() + pending.len() + 1);
        combined.push_str(&self.text);
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            combined.push('\n');
        }
        let pending_start = combined.len() as u32;
        combined.push_str(pending);
        (combined, pending_start)
    }

    /// Commit `entry` to the prelude. A trailing newline is added
    /// so the next appended item starts on a fresh line — matters
    /// for the parser's statement-boundary detection.
    pub fn commit(&mut self, entry: &str) {
        self.text.push_str(entry);
        if !entry.ends_with('\n') {
            self.text.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_starts_after_prelude_with_separator() {
        let mut s = Session::new();
        s.commit("set x 1");
        let (combined, off) = s.with_pending("puts $x");
        assert_eq!(combined, "set x 1\nputs $x");
        assert_eq!(off, 8);
    }

    #[test]
    fn commit_adds_trailing_newline_when_missing() {
        let mut s = Session::new();
        s.commit("a");
        s.commit("b\n");
        assert_eq!(s.prelude(), "a\nb\n");
    }
}
