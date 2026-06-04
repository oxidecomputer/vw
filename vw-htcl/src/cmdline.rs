// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Lightweight analysis of the partially-typed command at the cursor.
//!
//! Completion and signature help need to know, mid-edit, which command
//! the cursor sits in and which word is being typed. The full AST is
//! unreliable here *precisely because* the text is incomplete, so we
//! scan the raw source backward to the nearest command boundary
//! (newline, `;`, or the `[` that opens a command substitution) and
//! tokenize on whitespace. This is a deliberately shallow Tcl reader —
//! good enough to drive IDE affordances, not to execute.

use crate::span::Span;

#[derive(Clone, Debug)]
pub struct CmdLine<'a> {
    /// Whitespace-separated complete words before the cursor. The
    /// first, when present, is the command name.
    pub words: Vec<&'a str>,
    /// The word currently under the cursor: the trailing token when
    /// the prefix doesn't end in whitespace, otherwise empty.
    pub partial: &'a str,
    /// Span of `partial` in the source — the range a completion should
    /// replace. Zero-width (an insertion point) when `partial` is
    /// empty.
    pub partial_span: Span,
}

impl CmdLine<'_> {
    /// The command name (first complete word). `None` while the cursor
    /// is still on the first word — i.e. command-name position.
    pub fn command_name(&self) -> Option<&str> {
        self.words.first().copied()
    }

    /// True when the cursor is in command-name position (no complete
    /// words precede it).
    pub fn in_command_position(&self) -> bool {
        self.words.is_empty()
    }

    /// Flags (`-foo`) already supplied among the complete words after
    /// the command name.
    pub fn used_flags(&self) -> impl Iterator<Item = &str> {
        self.words
            .iter()
            .skip(1)
            .copied()
            .filter(|w| w.starts_with('-'))
    }
}

/// Analyze the command the cursor at `offset` is editing.
pub fn analyze(source: &str, offset: u32) -> CmdLine<'_> {
    let off = (offset as usize).min(source.len());
    let bytes = source.as_bytes();

    // Walk back to the start of the current command. The boundary
    // depends on the cursor's *bracket nesting*: inside a `[ … ]`,
    // newlines are whitespace (matching the parser), only `;` and the
    // opening `[` terminate. Outside brackets, `\n` and `;` both
    // terminate at the cursor's level.
    //
    // We track depth as we walk backward — each `]` going back means
    // we're entering a deeper region, each `[` brings us back out. If
    // we hit an unmatched `[` (the opening bracket of the substitution
    // the cursor sits in), that's the command boundary. Otherwise the
    // closest `\n`/`;` we passed at depth 0 wins. We have to scan past
    // a candidate `\n`/`;` because an enclosing `[` further back would
    // override it.
    let mut depth: i32 = 0;
    let mut nearest_top_sep: Option<usize> = None;
    let mut bracket_open: Option<usize> = None;
    let mut i = off;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b']' => depth += 1,
            b'[' => {
                if depth > 0 {
                    depth -= 1;
                } else {
                    bracket_open = Some(i + 1);
                    break;
                }
            }
            b'\n' | b';' if depth == 0 && nearest_top_sep.is_none() => {
                nearest_top_sep = Some(i + 1);
            }
            _ => {}
        }
    }
    let start = bracket_open.or(nearest_top_sep).unwrap_or(0);
    let prefix = &source[start..off];

    // The partial word is the trailing run of non-whitespace, unless
    // the prefix already ends in whitespace (then we're between words).
    let partial_len: usize = prefix
        .chars()
        .rev()
        .take_while(|c| !c.is_whitespace())
        .map(char::len_utf8)
        .sum();
    let split = prefix.len() - partial_len;
    let head = &prefix[..split];
    let partial = &prefix[split..];

    CmdLine {
        words: head.split_whitespace().collect(),
        partial,
        partial_span: Span::new((start + split) as u32, off as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(src: &str) -> CmdLine<'_> {
        analyze(src, src.len() as u32)
    }

    #[test]
    fn command_position_with_partial() {
        let line = at_end("gr");
        assert!(line.in_command_position());
        assert_eq!(line.partial, "gr");
        assert_eq!(line.partial_span, Span::new(0, 2));
    }

    #[test]
    fn argument_position_after_name() {
        let line = at_end("greet ");
        assert!(!line.in_command_position());
        assert_eq!(line.command_name(), Some("greet"));
        assert_eq!(line.partial, "");
        assert_eq!(line.partial_span, Span::new(6, 6));
    }

    #[test]
    fn partial_flag_after_name() {
        let line = at_end("greet -na");
        assert_eq!(line.command_name(), Some("greet"));
        assert_eq!(line.partial, "-na");
        assert_eq!(line.partial_span.slice("greet -na"), "-na");
    }

    #[test]
    fn used_flags_are_reported() {
        let line = at_end("f -a 1 -b ");
        let used: Vec<&str> = line.used_flags().collect();
        assert_eq!(used, vec!["-a", "-b"]);
    }

    #[test]
    fn resets_at_command_substitution() {
        // Only the text inside the `[...]` counts as the command.
        let src = "puts [greet -na";
        let line = analyze(src, src.len() as u32);
        assert_eq!(line.command_name(), Some("greet"));
        assert_eq!(line.partial, "-na");
    }

    #[test]
    fn resets_at_newline() {
        let src = "set x 1\ngr";
        let line = analyze(src, src.len() as u32);
        assert!(line.in_command_position());
        assert_eq!(line.partial, "gr");
    }

    #[test]
    fn ignores_newlines_inside_brackets() {
        // The cursor sits on `-cell ` in a multi-line `[ … ]`. The
        // analyzer must skip the intervening newlines so it can still
        // see `create_cpm5_cpm_pcie0` as the command name.
        let src = "\
set x [
  create_cpm5_cpm_pcie0
    -cell ";
        let line = analyze(src, src.len() as u32);
        assert_eq!(line.command_name(), Some("create_cpm5_cpm_pcie0"));
        assert_eq!(line.partial, "");
        // The flag in the middle counts as already-used.
        let used: Vec<&str> = line.used_flags().collect();
        assert_eq!(used, vec!["-cell"]);
    }

    #[test]
    fn active_partial_flag_across_lines() {
        // Partial `-max_link_` typed on a fresh line of a multi-line
        // bracket should still be recognized as the partial word, and
        // the command name should still be the bracket's first word.
        let src = "\
set x [
  create_cpm5_cpm_pcie0
    -cell cpm5
    -max_link_";
        let line = analyze(src, src.len() as u32);
        assert_eq!(line.command_name(), Some("create_cpm5_cpm_pcie0"));
        assert_eq!(line.partial, "-max_link_");
    }

    #[test]
    fn skips_balanced_inner_brackets() {
        // Walking back past a complete `[…]` shouldn't fool the
        // analyzer into thinking the cursor is at top level when it's
        // really inside another, *outer* bracket.
        let src = "\
set x [
  [a b]
  outer ";
        let line = analyze(src, src.len() as u32);
        // The cursor's enclosing bracket is the outer one; its first
        // word is the standalone `[a b]` substitution, not a simple
        // identifier — so command_name is None, but partial is empty
        // (we're between words on a continuation line). The point is
        // that the *outer* bracket is what we recognized, not the
        // inner one.
        let used: Vec<&str> = line.used_flags().collect();
        assert!(used.is_empty(), "{used:?}");
        // `outer` is the second word inside the outer bracket; the
        // first word was the `[…]` substitution itself.
        assert!(line.words.contains(&"outer"), "{:?}", line.words);
    }
}
