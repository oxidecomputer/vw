// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Byte-offset spans over source text.

use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    pub fn slice(self, source: &str) -> &str {
        &source[self.range()]
    }

    pub fn merge(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// Translate this span by `delta` bytes. Used to lift spans from a
    /// sub-parse (e.g. a proc body parsed as its own fragment) back
    /// into whole-source coordinates.
    pub const fn shifted(self, delta: u32) -> Span {
        Span::new(self.start + delta, self.end + delta)
    }

    /// True if `offset` lies within this span (start-inclusive,
    /// end-inclusive). End-inclusive is the right call for hover and
    /// "what's at the cursor" queries: a cursor visually positioned
    /// right after a token is still on it.
    pub fn contains(self, offset: u32) -> bool {
        offset >= self.start && offset <= self.end
    }
}

impl From<Range<usize>> for Span {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start as u32,
            end: range.end as u32,
        }
    }
}
