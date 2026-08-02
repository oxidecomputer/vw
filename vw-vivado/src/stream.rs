// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Block-level segmentation over vw-vivado's classified stream.
//!
//! The [`crate::worker::PtyClassifier`] already tags every chunk it
//! emits with a [`crate::StreamKind`] — `Info`/`Warning`/`Error` for
//! Vivado's severity-prefixed messages (with attached `at <file>:<line>
//! in ::<proc>` continuation lines already merged into the same chunk),
//! and `Stdout` for everything else. This module groups consecutive
//! `Stdout` chunks into a single [`Block::None`] so downstream renderers
//! can collapse or dim them as one unit instead of drowning the console
//! in Vivado's tables, section headers, banners, and license chatter.
//!
//! ### The design contract
//!
//! - **Diagnostic block = exactly one classified chunk.** Multi-line
//!   diagnostics (severity line + stack frames) are already merged
//!   upstream, so we don't have to reconstruct the "obvious
//!   continuation" boundary here.
//! - **NONE block = consecutive Stdout chunks.** A run of Stdout chunks
//!   feeds into one open NONE block; a Diagnostic chunk flushes it. The
//!   accumulator is stateful for exactly this reason.
//! - **Ordering is preserved.** [`BlockAccumulator::push`] returns
//!   `Vec<Block>` (0-2 elements) rather than a single option, because a
//!   Diagnostic arrival both flushes the pending NONE AND emits the
//!   diagnostic — the caller sees them in the correct order without
//!   having to track state itself.
//!
//! ### Log-level filtering
//!
//! [`LogLevel`] gates which blocks the renderer actually shows. Debug is
//! the escape hatch: it renders raw, no collapse. Info+ suppresses NONE
//! content but keeps it as a rendered "placeholder" (dim in `vw run`,
//! collapsible in the REPL) so users know noise was elided; users pick a
//! level and get exactly the diagnostics at that severity or higher.

use vw_eda::StreamKind;

/// Severity ladder used for log-level filtering. Higher = more severe.
/// Vivado does not emit a `DEBUG:`-tagged message of its own; the
/// [`Severity::Debug`] slot exists so [`LogLevel::Debug`] has a proper
/// floor for filtering — at that level every block renders raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Non-diagnostic noise: Vivado tables, section headers, banners,
    /// license chatter, `VHDL Output written to …` lines. Rendered
    /// dimmed (or collapsed in the REPL) at Info+ so a user can still
    /// see something happened; rendered raw at Debug.
    None,
    /// `INFO: [tag-id]` messages — low-importance advisories.
    Info,
    /// `WARNING: [tag-id]` messages.
    Warning,
    /// `CRITICAL WARNING: [tag-id]` messages. Vivado ranks these
    /// between WARNING and ERROR; semantically they mean "your run
    /// may fail because of this" so downstream rendering treats them
    /// like errors.
    CriticalWarning,
    /// `ERROR: [tag-id]` messages.
    Error,
}

/// The user-facing log level knob. Debug is the escape hatch — at
/// that level even non-diagnostic Stdout renders in full. Info is the
/// default: NONE blocks are elided (dim/collapsed), Info+ diagnostics
/// pass through. Higher levels suppress lower-severity diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevel {
    /// Show everything, including raw non-diagnostic output. The
    /// classifier's block boundaries still apply for rendering
    /// purposes, but nothing is elided.
    Debug,
    /// Show INFO+ diagnostics normally. Elide/collapse NONE blocks.
    #[default]
    Info,
    /// Show WARNING+ diagnostics. Elide/collapse NONE and INFO.
    Warning,
    /// Show CRITICAL WARNING+ diagnostics. Elide/collapse NONE, INFO,
    /// WARNING.
    Critical,
    /// Show only ERROR diagnostics. Elide/collapse everything else.
    Error,
}

impl LogLevel {
    /// Should a diagnostic at the given severity render at this level?
    /// Non-diagnostic ([`Severity::None`]) blocks always return `true`
    /// at Debug (raw stream) and `false` otherwise — the renderer is
    /// still expected to show a collapsed placeholder for them at
    /// Info+, but that's a rendering choice, not a filter decision.
    pub fn allows(self, sev: Severity) -> bool {
        match self {
            LogLevel::Debug => true,
            LogLevel::Info => sev >= Severity::Info,
            LogLevel::Warning => sev >= Severity::Warning,
            LogLevel::Critical => sev >= Severity::CriticalWarning,
            LogLevel::Error => sev >= Severity::Error,
        }
    }

    /// Should a NONE block render as a collapsed placeholder (rather
    /// than being elided entirely OR rendered raw)? At Debug it renders
    /// raw; at all higher levels it renders as a placeholder.
    pub fn collapse_none(self) -> bool {
        !matches!(self, LogLevel::Debug)
    }

    /// Parse the CLI's `--log-level` argument. Accepts lowercase (the
    /// canonical form) plus a few common aliases users type without
    /// checking `--help`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" => Ok(LogLevel::Debug),
            "info" => Ok(LogLevel::Info),
            "warn" | "warning" => Ok(LogLevel::Warning),
            "critical" | "crit" | "critical-warning" => Ok(LogLevel::Critical),
            "error" | "err" => Ok(LogLevel::Error),
            other => Err(format!(
                "unknown log level `{other}`; expected one of \
                 debug|info|warning|critical|error"
            )),
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Critical => "critical",
            LogLevel::Error => "error",
        };
        f.write_str(s)
    }
}

/// One classified block. Ownership: `lines` is the block's content
/// verbatim (trailing `\n` on each line stripped). The renderer
/// re-adds newlines when it writes out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A run of consecutive non-diagnostic Stdout chunks. Render
    /// dimmed (vw run) or collapsed (repl) at Info+; raw at Debug.
    None { lines: Vec<String> },
    /// A single classified diagnostic — one severity-tagged line
    /// plus any attached `at <path>:<line> in ::<proc>` continuation
    /// lines (already merged upstream by [`crate::worker::PtyClassifier`]).
    Diagnostic {
        severity: Severity,
        lines: Vec<String>,
    },
}

impl Block {
    pub fn severity(&self) -> Severity {
        match self {
            Block::None { .. } => Severity::None,
            Block::Diagnostic { severity, .. } => *severity,
        }
    }

    pub fn lines(&self) -> &[String] {
        match self {
            Block::None { lines } | Block::Diagnostic { lines, .. } => lines,
        }
    }

    /// Number of source lines in the block, including any continuation
    /// lines on a diagnostic. Renderers use this to size the collapsed
    /// placeholder ("▶ preview (N lines)").
    pub fn line_count(&self) -> usize {
        self.lines().len()
    }
}

/// Map a [`StreamKind`] to a [`Severity`]. `Stdout` → `None` — the
/// classifier's "we don't know what this is" bucket becomes our
/// non-diagnostic tier.
pub fn severity_of(kind: StreamKind) -> Severity {
    match kind {
        StreamKind::Stdout => Severity::None,
        StreamKind::Info => Severity::Info,
        StreamKind::Warning => Severity::Warning,
        StreamKind::CriticalWarning => Severity::CriticalWarning,
        StreamKind::Error => Severity::Error,
    }
}

/// Inverse of [`severity_of`]: pick the [`StreamKind`] a renderer
/// should use to color a block of the given severity. `Severity::None`
/// maps to `StreamKind::Stdout` — the default, unclassified stream.
pub fn stream_kind_for(severity: Severity) -> StreamKind {
    match severity {
        Severity::None => StreamKind::Stdout,
        Severity::Info => StreamKind::Info,
        Severity::Warning => StreamKind::Warning,
        Severity::CriticalWarning => StreamKind::CriticalWarning,
        Severity::Error => StreamKind::Error,
    }
}

/// Streaming block segmenter. Feed classified chunks in with
/// [`push`](Self::push); collect trailing content with
/// [`flush`](Self::flush) when the stream ends.
///
/// State: the accumulator holds at most one open NONE block. A
/// classified (non-Stdout) chunk flushes it and immediately emits
/// itself as a Diagnostic block, in that order.
#[derive(Debug, Default)]
pub struct BlockAccumulator {
    pending_none: Vec<String>,
}

impl BlockAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one classified chunk. Returns 0-2 blocks in emission
    /// order: a pending-NONE flush (if any) followed by the new
    /// Diagnostic. Stdout chunks return `[]` — their content is
    /// accumulated for the eventual NONE flush.
    ///
    /// The `chunk` is split on `\n`, trailing empty lines are dropped
    /// so a diagnostic that ends `…in ::proc\n` doesn't leave a
    /// spurious empty line in its block. An entirely-empty chunk is
    /// a no-op regardless of kind.
    pub fn push(&mut self, kind: StreamKind, chunk: &str) -> Vec<Block> {
        let lines = split_chunk_lines(chunk);
        if lines.is_empty() {
            return Vec::new();
        }
        let severity = severity_of(kind);
        if severity == Severity::None {
            self.pending_none.extend(lines);
            return Vec::new();
        }
        let mut out = Vec::with_capacity(2);
        if !self.pending_none.is_empty() {
            out.push(Block::None {
                lines: std::mem::take(&mut self.pending_none),
            });
        }
        out.push(Block::Diagnostic { severity, lines });
        out
    }

    /// Emit any pending NONE block. Call once at end-of-stream (or
    /// when a rendering surface needs the trailing noise to settle).
    /// Returns `[]` when nothing is pending.
    pub fn flush(&mut self) -> Vec<Block> {
        if self.pending_none.is_empty() {
            return Vec::new();
        }
        vec![Block::None {
            lines: std::mem::take(&mut self.pending_none),
        }]
    }

    /// Test-only: peek at pending state without emitting.
    #[cfg(test)]
    fn pending_none(&self) -> &[String] {
        &self.pending_none
    }
}

/// Split a chunk into lines the segmenter can accumulate. Drops the
/// single trailing empty element that `split('\n')` produces for a
/// `"a\nb\n"`-shaped chunk. Interior empty lines are preserved — some
/// diagnostic messages include a blank line between the tag and the
/// continuation frames, and renderers want that whitespace back.
fn split_chunk_lines(chunk: &str) -> Vec<String> {
    if chunk.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> =
        chunk.split('\n').map(str::to_string).collect();
    if lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdout_chunks_accumulate_into_one_none_block() {
        let mut acc = BlockAccumulator::new();
        assert_eq!(acc.push(StreamKind::Stdout, "banner line 1\n"), vec![]);
        assert_eq!(acc.push(StreamKind::Stdout, "banner line 2\n"), vec![]);
        assert_eq!(
            acc.push(StreamKind::Stdout, "banner line 3\nline 4\n"),
            vec![]
        );
        // Nothing emitted yet — the None block is still open.
        assert_eq!(acc.pending_none().len(), 4);
        let flushed = acc.flush();
        assert_eq!(
            flushed,
            vec![Block::None {
                lines: vec![
                    "banner line 1".into(),
                    "banner line 2".into(),
                    "banner line 3".into(),
                    "line 4".into(),
                ]
            }]
        );
        assert!(acc.pending_none().is_empty());
    }

    #[test]
    fn diagnostic_chunk_flushes_pending_none_then_emits_itself() {
        let mut acc = BlockAccumulator::new();
        acc.push(StreamKind::Stdout, "table row 1\n");
        acc.push(StreamKind::Stdout, "table row 2\n");
        let out = acc.push(
            StreamKind::Warning,
            "WARNING: [Synth 8-100] something\n  at foo.tcl:12 in ::bar\n",
        );
        assert_eq!(
            out,
            vec![
                Block::None {
                    lines: vec!["table row 1".into(), "table row 2".into()]
                },
                Block::Diagnostic {
                    severity: Severity::Warning,
                    lines: vec![
                        "WARNING: [Synth 8-100] something".into(),
                        "  at foo.tcl:12 in ::bar".into(),
                    ]
                }
            ]
        );
        // Pending is now clean.
        assert!(acc.pending_none().is_empty());
    }

    #[test]
    fn back_to_back_diagnostics_emit_no_none_between_them() {
        let mut acc = BlockAccumulator::new();
        let out1 = acc.push(StreamKind::Info, "INFO: [X 1-1] first\n");
        assert_eq!(
            out1,
            vec![Block::Diagnostic {
                severity: Severity::Info,
                lines: vec!["INFO: [X 1-1] first".into()]
            }]
        );
        let out2 = acc.push(StreamKind::Info, "INFO: [X 1-2] second\n");
        assert_eq!(
            out2,
            vec![Block::Diagnostic {
                severity: Severity::Info,
                lines: vec!["INFO: [X 1-2] second".into()]
            }]
        );
    }

    #[test]
    fn critical_warning_is_its_own_severity() {
        let mut acc = BlockAccumulator::new();
        let out = acc.push(
            StreamKind::CriticalWarning,
            "CRITICAL WARNING: [Vivado 12-4739] set_clock_groups: …\n",
        );
        assert_eq!(
            out,
            vec![Block::Diagnostic {
                severity: Severity::CriticalWarning,
                lines: vec![
                    "CRITICAL WARNING: [Vivado 12-4739] set_clock_groups: …"
                        .into()
                ]
            }]
        );
    }

    #[test]
    fn empty_and_newline_only_chunks_are_ignored() {
        let mut acc = BlockAccumulator::new();
        assert_eq!(acc.push(StreamKind::Stdout, ""), vec![]);
        assert_eq!(acc.push(StreamKind::Stdout, "\n"), vec![]);
        // The lone "\n" IS a blank line and should accumulate — Vivado
        // sometimes emits blank separator lines that a renderer might
        // want to preserve as vertical whitespace. Our
        // `split_chunk_lines` correctly turns "\n" into one empty
        // string line.
        assert_eq!(acc.pending_none(), &["".to_string()]);
    }

    #[test]
    fn trailing_none_after_last_diagnostic_flushes_at_end() {
        let mut acc = BlockAccumulator::new();
        acc.push(StreamKind::Error, "ERROR: [X 1-1] boom\n");
        acc.push(StreamKind::Stdout, "postmortem line 1\n");
        acc.push(StreamKind::Stdout, "postmortem line 2\n");
        let out = acc.flush();
        assert_eq!(
            out,
            vec![Block::None {
                lines: vec![
                    "postmortem line 1".into(),
                    "postmortem line 2".into()
                ]
            }]
        );
    }

    #[test]
    fn log_level_allows_diagnostic_severities() {
        // Debug shows everything.
        assert!(LogLevel::Debug.allows(Severity::None));
        assert!(LogLevel::Debug.allows(Severity::Error));

        // Info hides only NONE.
        assert!(!LogLevel::Info.allows(Severity::None));
        assert!(LogLevel::Info.allows(Severity::Info));
        assert!(LogLevel::Info.allows(Severity::Error));

        // Warning hides NONE + INFO.
        assert!(!LogLevel::Warning.allows(Severity::None));
        assert!(!LogLevel::Warning.allows(Severity::Info));
        assert!(LogLevel::Warning.allows(Severity::Warning));
        assert!(LogLevel::Warning.allows(Severity::CriticalWarning));

        // Critical hides NONE + INFO + WARNING.
        assert!(!LogLevel::Critical.allows(Severity::Warning));
        assert!(LogLevel::Critical.allows(Severity::CriticalWarning));
        assert!(LogLevel::Critical.allows(Severity::Error));

        // Error hides everything but ERROR.
        assert!(!LogLevel::Error.allows(Severity::CriticalWarning));
        assert!(LogLevel::Error.allows(Severity::Error));
    }

    #[test]
    fn log_level_collapse_none_is_off_only_at_debug() {
        assert!(!LogLevel::Debug.collapse_none());
        assert!(LogLevel::Info.collapse_none());
        assert!(LogLevel::Warning.collapse_none());
        assert!(LogLevel::Critical.collapse_none());
        assert!(LogLevel::Error.collapse_none());
    }

    #[test]
    fn log_level_parse_accepts_common_forms() {
        assert_eq!(LogLevel::parse("debug"), Ok(LogLevel::Debug));
        assert_eq!(LogLevel::parse("INFO"), Ok(LogLevel::Info));
        assert_eq!(LogLevel::parse("warn"), Ok(LogLevel::Warning));
        assert_eq!(LogLevel::parse("Warning"), Ok(LogLevel::Warning));
        assert_eq!(LogLevel::parse("critical"), Ok(LogLevel::Critical));
        assert_eq!(LogLevel::parse("crit"), Ok(LogLevel::Critical));
        assert_eq!(LogLevel::parse("critical-warning"), Ok(LogLevel::Critical));
        assert_eq!(LogLevel::parse("err"), Ok(LogLevel::Error));
        assert_eq!(LogLevel::parse("error"), Ok(LogLevel::Error));
        assert!(LogLevel::parse("").is_err());
        assert!(LogLevel::parse("verbose").is_err());
    }

    #[test]
    fn diagnostic_carrying_multiline_message_stays_one_block() {
        // The upstream PTY classifier merges the "at foo.tcl:X in ::proc"
        // continuation lines into the same chunk it hands us. The
        // segmenter should NOT split them across blocks.
        let mut acc = BlockAccumulator::new();
        let chunk = "ERROR: [Synth 8-5826] no such design unit 'foo'\n  \
                     at foo.tcl:1 in ::bar\n  at design.htcl:1\n";
        let out = acc.push(StreamKind::Error, chunk);
        assert_eq!(out.len(), 1);
        let Block::Diagnostic { severity, lines } = &out[0] else {
            panic!("expected Diagnostic");
        };
        assert_eq!(*severity, Severity::Error);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("ERROR:"));
        assert!(lines[1].contains("at foo.tcl"));
        assert!(lines[2].contains("at design.htcl"));
    }
}
