// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Signature help for htcl proc calls.
//!
//! When the cursor is inside a call to a known `proc`, report that
//! proc's signature and which parameter is "active" so the editor can
//! highlight it. The active parameter is the one named by the most
//! recent `-flag` typed on the line; before any flag is typed there is
//! no active parameter (the whole signature is shown).
//!
//! Pure analysis, like [`crate::complete`]: the LSP backend turns the
//! returned [`SignatureHelp`] into `lsp_types::SignatureHelp`.

use crate::ast::{CommandKind, Document, ProcSignature, Stmt};
use crate::cmdline::{self, CmdLine};

#[derive(Clone, Debug)]
pub struct SignatureHelp<'a> {
    pub proc_name: String,
    pub signature: &'a ProcSignature,
    /// Proc-level doc comments (`##` above the declaration).
    pub doc_comments: &'a [String],
    /// Index into `signature.args` of the parameter under the cursor,
    /// if one is determinable.
    pub active_parameter: Option<u32>,
}

/// Signature help for the call the cursor at `offset` is inside, or
/// `None` if the cursor isn't in a known proc call.
pub fn signature_help_at<'a>(
    document: &'a Document,
    source: &str,
    offset: u32,
) -> Option<SignatureHelp<'a>> {
    let line = cmdline::analyze(source, offset);
    // `command_name` is `None` while the cursor is still on the first
    // word, which is exactly when there's no call to describe yet.
    let name = line.command_name()?;
    let (signature, doc_comments) = find_proc(document, name)?;
    Some(SignatureHelp {
        proc_name: name.to_string(),
        signature,
        doc_comments,
        active_parameter: active_parameter(signature, &line),
    })
}

fn find_proc<'a>(
    document: &'a Document,
    name: &str,
) -> Option<(&'a ProcSignature, &'a [String])> {
    for stmt in &document.stmts {
        let Stmt::Command(cmd) = stmt else { continue };
        let CommandKind::Proc(proc) = &cmd.kind else {
            continue;
        };
        if proc.name.as_deref() == Some(name) {
            return Some((proc.signature.as_ref()?, &cmd.doc_comments));
        }
    }
    None
}

/// The active parameter is the arg named by the most recent `-flag`
/// token. Complete flags must name an arg exactly; a flag still being
/// typed (the partial word) matches by prefix so the highlight tracks
/// as the user types.
fn active_parameter(sig: &ProcSignature, line: &CmdLine<'_>) -> Option<u32> {
    let mut active = None;
    for word in line.words.iter().skip(1) {
        if let Some(flag) = word.strip_prefix('-') {
            if let Some(i) = sig.args.iter().position(|a| a.name == flag) {
                active = Some(i as u32);
            }
        }
    }
    if let Some(flag) = line.partial.strip_prefix('-') {
        if !flag.is_empty() {
            if let Some(i) =
                sig.args.iter().position(|a| a.name.starts_with(flag))
            {
                return Some(i as u32);
            }
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn cursor(src_with_marker: &str) -> (String, u32) {
        let offset = src_with_marker.find('|').expect("no cursor marker");
        (src_with_marker.replacen('|', "", 1), offset as u32)
    }

    fn help(src_with_marker: &str) -> Option<(String, Option<u32>)> {
        let (src, off) = cursor(src_with_marker);
        let parsed = parse(&src);
        signature_help_at(&parsed.document, &src, off)
            .map(|h| (h.proc_name, h.active_parameter))
    }

    #[test]
    fn shows_signature_after_name() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg |\n";
        let (name, active) = help(src).unwrap();
        assert_eq!(name, "cfg");
        assert_eq!(active, None);
    }

    #[test]
    fn active_parameter_follows_last_flag() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -depth |\n";
        let (_, active) = help(src).unwrap();
        assert_eq!(active, Some(1));
    }

    #[test]
    fn active_parameter_tracks_partial_flag() {
        let src = "\
proc cfg {\n  width\n  depth\n} { }\n\
cfg -wid|\n";
        let (_, active) = help(src).unwrap();
        assert_eq!(active, Some(0));
    }

    #[test]
    fn none_while_typing_proc_name() {
        let src = "\
proc cfg {\n  width\n} { }\n\
cf|\n";
        assert!(help(src).is_none());
    }

    #[test]
    fn none_for_unknown_command() {
        let src = "puts |\n";
        assert!(help(src).is_none());
    }

    #[test]
    fn works_inside_proc_body() {
        let src = "\
proc helper {\n  size\n} { }\n\
proc outer {} {\n  helper -size |\n}\n";
        let (name, active) = help(src).unwrap();
        assert_eq!(name, "helper");
        assert_eq!(active, Some(0));
    }
}
