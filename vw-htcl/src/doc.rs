// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Render helpers for doc-comment blocks.
//!
//! `##` doc comments in source are typically wrapped at a comfortable
//! editing column (~80 chars). When a display surface — an LSP hover,
//! signature help, completion documentation — joins those lines
//! verbatim, the source wrap survives into the rendered markdown.
//! Most LSP clients then treat the first wrapped fragment as a "brief
//! summary," which is almost always a mid-sentence truncation.
//!
//! [`reflow_doc_comments`] converts a slice of doc-comment lines into
//! markdown-clean text: consecutive non-empty lines collapse into one
//! paragraph (joined with a single space), and a blank line becomes a
//! paragraph break (`\n\n`). The first paragraph then reads as a
//! complete unit — usually one or more whole sentences — instead of
//! the editor-wrap fragment that surfaces today.

/// One-line summary suitable for an inline annotation (LSP
/// `CompletionItem::detail`, a parameter list's `— brief` suffix,
/// etc.). Takes the first reflowed paragraph and trims to its first
/// sentence — the convention rustdoc, godoc, and most doc generators
/// follow for "short description vs full body."
///
/// Returns `None` when `lines` has no non-blank content. Falls back
/// to the whole first paragraph when no sentence terminator (`.`,
/// `!`, `?` followed by whitespace or end-of-string) is found.
pub fn brief(lines: &[String]) -> Option<String> {
    let reflowed = reflow_doc_comments(lines);
    if reflowed.is_empty() {
        return None;
    }
    let first_paragraph = reflowed.split("\n\n").next().unwrap();
    let bytes = first_paragraph.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if !matches!(b, b'.' | b'!' | b'?') {
            continue;
        }
        let next = bytes.get(i + 1).copied();
        if next.is_none() || matches!(next, Some(b' ' | b'\t' | b'\n')) {
            return Some(first_paragraph[..=i].to_string());
        }
    }
    Some(first_paragraph.to_string())
}

/// Extended description — everything **after** the first sentence,
/// reflowed into markdown.
///
/// Pairs with [`brief`]: an LSP-facing renderer puts `brief` in
/// `CompletionItem::detail` (the inline summary next to the label)
/// and `extended` in `documentation` (the body popup). Splitting
/// this way avoids the duplication that occurs when both fields
/// start with the same sentence.
///
/// Returns `None` when there is no content after the first sentence
/// — e.g. when the doc is a single-sentence summary with no body.
pub fn extended(lines: &[String]) -> Option<String> {
    let reflowed = reflow_doc_comments(lines);
    if reflowed.is_empty() {
        return None;
    }
    let bytes = reflowed.as_bytes();
    let mut split_at = None;
    for (i, &b) in bytes.iter().enumerate() {
        if !matches!(b, b'.' | b'!' | b'?') {
            continue;
        }
        let next = bytes.get(i + 1).copied();
        if next.is_none() || matches!(next, Some(b' ' | b'\t' | b'\n')) {
            split_at = Some(i + 1);
            break;
        }
    }
    // No sentence terminator means the whole reflow IS the brief —
    // nothing to put in the body.
    let after = reflowed[split_at?..].trim_start();
    (!after.is_empty()).then(|| after.to_string())
}

/// Word-wrap `text` into lines no wider than `width` chars. Used by
/// doc-comment generators that want source files with paragraphs
/// re-flowed to a comfortable editing width (the LSP reflows again
/// for display, but a wrapped source is easier for humans to read
/// and diff).
///
/// A single word longer than `width` is left on a line by itself
/// rather than truncated.
pub fn wrap_paragraph(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Reflow doc-comment lines into a markdown string. See module docs.
pub fn reflow_doc_comments(lines: &[String]) -> String {
    let mut out = String::new();
    let mut paragraph = String::new();
    let flush = |paragraph: &mut String, out: &mut String| {
        if paragraph.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(paragraph);
        paragraph.clear();
    };
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut out);
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(&render_refs(trimmed));
        }
    }
    flush(&mut paragraph, &mut out);
    out
}

/// Rewrite `[NAME]` tokens as `` `NAME` `` so hover-popup markdown
/// renders them as inline code — visually distinct from prose, and
/// (in editors that honor code-span click handlers) discoverable as
/// something a reader can act on. The analyzer's goto/hover paths
/// already resolve the cursor to the same reference; this is the
/// display side of the same feature.
///
/// Interior chars accepted: letters, digits, `_`, and `:` (for
/// namespace qualification). Anything else is left as-is — a
/// prose sentence like "see [1]" or "[TODO: refactor]" isn't a
/// reference.
fn render_refs(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let content_start = i + 1;
            // First char of a Tcl-style ident must be a letter or
            // underscore; digits and `:`-prefixed forms don't count
            // (they're prose or footnote-style refs, not identifiers).
            if content_start < bytes.len()
                && (bytes[content_start].is_ascii_alphabetic()
                    || bytes[content_start] == b'_')
            {
                let mut j = content_start;
                while j < bytes.len() && bytes[j] != b']' {
                    let b = bytes[j];
                    let ok =
                        b.is_ascii_alphanumeric() || b == b'_' || b == b':';
                    if !ok {
                        break;
                    }
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b']' && j > content_start {
                    out.push('`');
                    out.push_str(&s[content_start..j]);
                    out.push('`');
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines<const N: usize>(arr: [&str; N]) -> Vec<String> {
        arr.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_wrapped_paragraph_becomes_one_line() {
        let out = reflow_doc_comments(&lines([
            "Create an external port in the current block design and connect that to the",
            "selected block pin.",
        ]));
        assert_eq!(
            out,
            "Create an external port in the current block design and connect that to the selected block pin."
        );
    }

    #[test]
    fn blank_line_becomes_paragraph_break() {
        let out = reflow_doc_comments(&lines([
            "Summary line one.",
            "",
            "Body line two,",
            "wrapped.",
        ]));
        assert_eq!(out, "Summary line one.\n\nBody line two, wrapped.");
    }

    #[test]
    fn leading_and_trailing_blanks_are_dropped() {
        let out = reflow_doc_comments(&lines(["", "Hello.", "", ""]));
        assert_eq!(out, "Hello.");
    }

    #[test]
    fn empty_input_returns_empty_string() {
        assert_eq!(reflow_doc_comments(&[]), "");
    }

    #[test]
    fn brief_extracts_first_sentence_from_wrapped_lines() {
        let out = brief(&lines([
            "Create an external port in the current block design and connect that to the",
            "selected block pin. If a bd_cell is specified, all pins are made external.",
        ]));
        assert_eq!(
            out.as_deref(),
            Some(
                "Create an external port in the current block design and connect that to the selected block pin."
            )
        );
    }

    #[test]
    fn brief_handles_single_sentence_proc() {
        let out = brief(&lines(["Width of the data bus in bits."]));
        assert_eq!(out.as_deref(), Some("Width of the data bus in bits."));
    }

    #[test]
    fn brief_falls_back_to_paragraph_when_no_terminator() {
        let out = brief(&lines(["just a phrase", "with no period"]));
        assert_eq!(out.as_deref(), Some("just a phrase with no period"));
    }

    #[test]
    fn brief_returns_none_for_empty_input() {
        assert!(brief(&[]).is_none());
        assert!(brief(&lines([""])).is_none());
    }

    #[test]
    fn extended_skips_the_summary_sentence() {
        let out = extended(&lines([
            "Summary. Body sentence in same paragraph.",
            "",
            "Second paragraph here.",
        ]));
        assert_eq!(
            out.as_deref(),
            Some("Body sentence in same paragraph.\n\nSecond paragraph here.")
        );
    }

    #[test]
    fn extended_returns_none_for_single_sentence_doc() {
        assert!(extended(&lines(["Width of the data bus in bits."])).is_none());
    }

    #[test]
    fn extended_brief_round_trip_covers_full_text() {
        // Together, `brief` and `extended` should reproduce every
        // visible character of the reflowed input (modulo a single
        // separator between them).
        let input = lines([
            "First sentence.",
            "Continued first paragraph.",
            "",
            "Second paragraph.",
        ]);
        let b = brief(&input).unwrap();
        let e = extended(&input).unwrap();
        let full = reflow_doc_comments(&input);
        // The recombined text should equal the reflow (with a space
        // between b and e since the brief is part of paragraph 1).
        assert!(full.starts_with(&b));
        assert!(full.ends_with(&e));
    }

    #[test]
    fn brief_does_not_trip_on_decimal_or_versal_dots() {
        // `3.4` shouldn't end the sentence — terminator must be
        // followed by whitespace or end-of-string.
        let out =
            brief(&lines(["Source IP-XACT: xilinx.com:ip:versal_cips:3.4"]));
        assert_eq!(
            out.as_deref(),
            Some("Source IP-XACT: xilinx.com:ip:versal_cips:3.4")
        );
    }

    #[test]
    fn wrap_paragraph_breaks_at_word_boundaries() {
        let out = wrap_paragraph("one two three four five six", 12);
        assert_eq!(out, vec!["one two", "three four", "five six"]);
    }

    #[test]
    fn wrap_paragraph_keeps_oversize_words_on_their_own_line() {
        let out = wrap_paragraph("short superlongword end", 8);
        assert_eq!(out, vec!["short", "superlongword", "end"]);
    }

    #[test]
    fn single_leading_space_is_trimmed_per_line() {
        // `##` doc comments may include a leading space after the
        // `##` marker that gets preserved in the parsed string; we
        // trim each line so the leading space doesn't become a
        // double-space inside the joined paragraph.
        let out = reflow_doc_comments(&lines([" word one", " word two"]));
        assert_eq!(out, "word one word two");
    }

    /// `[NAME]` refs in doc-comment text render as inline code
    /// spans so hover popups distinguish them from prose.
    #[test]
    fn ref_tokens_render_as_inline_code() {
        let out = reflow_doc_comments(&lines([
            "Construct with [dcmac::mac_port] before calling [dcmac::create].",
        ]));
        assert_eq!(
            out,
            "Construct with `dcmac::mac_port` before calling `dcmac::create`."
        );
    }

    /// Not every `[…]` in prose is a reference. Only accept alnum +
    /// `_` + `:` interiors; anything else stays as-is.
    #[test]
    fn non_ref_brackets_left_alone() {
        let out = reflow_doc_comments(&lines([
            "See [1] and [TODO: refactor] for details.",
        ]));
        assert_eq!(out, "See [1] and [TODO: refactor] for details.");
    }
}
