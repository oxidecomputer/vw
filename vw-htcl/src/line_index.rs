// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Byte-offset ↔ line/column conversion.
//!
//! LSP positions are 0-indexed `(line, character)` where `character`
//! counts UTF-16 code units, not bytes. We honor that here so the
//! editor's cursor lands where the user expects on non-ASCII source.

use crate::span::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of each line. `line_starts[0] == 0`.
    line_starts: Vec<u32>,
    /// Full source text; needed for the UTF-8 → UTF-16 column
    /// conversion.
    text: String,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push((i + 1) as u32);
            }
        }
        Self {
            line_starts,
            text: text.to_string(),
        }
    }

    pub fn position(&self, byte_offset: u32) -> LineCol {
        let offset = byte_offset.min(self.text.len() as u32);
        let line_idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line_idx];
        let line_text = &self.text[line_start as usize..offset as usize];
        let character = line_text.encode_utf16().count() as u32;
        LineCol {
            line: line_idx as u32,
            character,
        }
    }

    pub fn range(&self, span: Span) -> (LineCol, LineCol) {
        (self.position(span.start), self.position(span.end))
    }

    /// Convert a UTF-16 line/character position back to a byte
    /// offset. Inverse of [`position`](Self::position); used to map
    /// LSP positions from clients (which speak UTF-16) into byte
    /// offsets the rest of the analysis uses.
    ///
    /// Clamps gracefully: a line past EOF returns the source length;
    /// a character past EOL returns the offset of the line ending.
    pub fn offset_of(&self, lc: LineCol) -> u32 {
        let Some(&line_start) = self.line_starts.get(lc.line as usize) else {
            return self.text.len() as u32;
        };
        let line_end = self
            .line_starts
            .get(lc.line as usize + 1)
            .copied()
            .map(|n| n.saturating_sub(1))
            .unwrap_or(self.text.len() as u32);
        let line_text = &self.text[line_start as usize..line_end as usize];
        let mut byte_offset = line_start;
        let mut char_count: u32 = 0;
        for ch in line_text.chars() {
            if char_count >= lc.character {
                break;
            }
            char_count += ch.len_utf16() as u32;
            byte_offset += ch.len_utf8() as u32;
        }
        byte_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_positions() {
        let idx = LineIndex::new("abc\nde\nf");
        assert_eq!(
            idx.position(0),
            LineCol {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            idx.position(3),
            LineCol {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            idx.position(4),
            LineCol {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            idx.position(7),
            LineCol {
                line: 2,
                character: 0
            }
        );
    }

    #[test]
    fn utf16_character_count() {
        // `é` is one UTF-16 code unit, two UTF-8 bytes.
        let idx = LineIndex::new("é\nx");
        let pos = idx.position(2); // byte after `é`
        assert_eq!(
            pos,
            LineCol {
                line: 0,
                character: 1
            }
        );
    }

    #[test]
    fn offset_of_round_trips() {
        let idx = LineIndex::new("abc\nde\nf");
        for &b in &[0u32, 1, 3, 4, 6, 7] {
            assert_eq!(idx.offset_of(idx.position(b)), b);
        }
    }

    #[test]
    fn offset_of_clamps_past_line_end() {
        let idx = LineIndex::new("abc\nde");
        // line 0, character 100 → end of line 0 (byte 3)
        assert_eq!(
            idx.offset_of(LineCol {
                line: 0,
                character: 100
            }),
            3
        );
    }
}
