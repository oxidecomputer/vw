// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Parse a Vivado command reference page into a [`ManPage`].
//!
//! Following the convention in [`vw_htcl::parser`], the outer loop is
//! hand-rolled — it owns line grouping and section recovery, which a
//! pure combinator grammar models awkwardly for free-form reference
//! text — while the structural inner pieces (an argument header's
//! `-flag <placeholder> - (marker)` shape, a `See Also` bullet) are
//! parsed with [`winnow`].
//!
//! The grammar the inner parsers recognize, per argument block:
//!
//! ```text
//! flag-header := '-' ident   placeholder?   ' - '   marker?   prose
//! pos-header  := '<' .. '>'  '...'?          ' - '   marker?   prose
//! marker      := '(' ('Optional' | 'Required') ')'
//! ```
//!
//! A `placeholder` (anything between the flag name and the ` - `
//! separator) makes a flag a *value* flag; its absence makes it a
//! *boolean* toggle.

use winnow::ascii::space0;
use winnow::combinator::{opt, preceded};
use winnow::token::take_while;
use winnow::ModalResult;
use winnow::Parser;

use crate::model::{ArgKind, Argument, ManPage};

/// Parse `text` (the contents of one man page) into a [`ManPage`].
/// `name` is the command name (the source file stem).
pub fn parse_man_page(name: &str, text: &str) -> ManPage {
    let normalized = text.replace('\r', "");
    let sections = split_sections(&normalized);

    let description = section_lines(&sections, "Description")
        .map(dedent_block)
        .unwrap_or_default();

    let arguments = section_lines(&sections, "Arguments")
        .map(|lines| parse_arguments(&dedent_block(lines)))
        .unwrap_or_default();

    let see_also = section_lines(&sections, "See Also")
        .or_else(|| section_lines(&sections, "See also"))
        .map(parse_see_also)
        .unwrap_or_default();

    // The `Returns:` section is optional and usually one or two
    // lines. Some pages spell it `Return Value` or `Return value`
    // — accept both.
    let returns = section_lines(&sections, "Returns")
        .or_else(|| section_lines(&sections, "Return Value"))
        .or_else(|| section_lines(&sections, "Return value"))
        .map(dedent_block);

    let mut page = ManPage {
        name: name.to_string(),
        description,
        arguments,
        see_also,
        returns,
    };
    finalize_arguments(&mut page);
    page
}

// ---------------------------------------------------------------------------
// Sectioning (hand-rolled outer loop).
// ---------------------------------------------------------------------------

/// A man page is a flat list of `Header:` sections. Returns each
/// section's title (without the trailing colon) paired with its raw
/// body lines, in document order.
fn split_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(title) = section_header(line) {
            sections.push((title, Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line.to_string());
        }
        // Lines before the first header (a leading blank line, usually)
        // are dropped.
    }
    sections
}

/// Recognize a section header line — a capitalized label at column
/// zero ending in a colon, e.g. `Arguments:` or `See Also:`. Returns
/// the label without the colon.
fn section_header(line: &str) -> Option<String> {
    // Headers sit flush left; body text is indented. Cheap reject
    // first.
    if line.is_empty() || line.starts_with(' ') {
        return None;
    }
    let stripped = line.strip_suffix(':')?;
    if stripped.is_empty()
        || !stripped
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == ' ')
        || !stripped.starts_with(|c: char| c.is_ascii_uppercase())
    {
        return None;
    }
    Some(stripped.to_string())
}

/// The body lines of the first section whose title equals `title`.
fn section_lines<'a>(
    sections: &'a [(String, Vec<String>)],
    title: &str,
) -> Option<&'a [String]> {
    sections
        .iter()
        .find(|(t, _)| t == title)
        .map(|(_, body)| body.as_slice())
}

/// Strip the uniform two-space indent man-page bodies carry, leaving
/// any deeper (bullet / code) indentation intact, and drop leading and
/// trailing blank lines. Interior blank lines are preserved.
fn dedent_block(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .map(|l| l.strip_prefix("  ").unwrap_or(l).trim_end().to_string())
        .collect();
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Arguments.
// ---------------------------------------------------------------------------

/// Group the (already de-indented) argument-section lines into blocks
/// — runs of consecutive non-blank lines — then turn each block into
/// an [`Argument`]. Blocks that aren't an argument header (`Note:`,
/// `Tip:`, free prose) are folded into the preceding argument's
/// description.
fn parse_arguments(lines: &[String]) -> Vec<Argument> {
    let mut args: Vec<Argument> = Vec::new();
    for block in blocks(lines) {
        let first = &block[0];
        match parse_arg_header(first) {
            Some(header) => {
                let mut description = Vec::new();
                let head = header.prose.trim().to_string();
                if !head.is_empty() {
                    description.push(head);
                }
                for line in &block[1..] {
                    description.push(line.trim().to_string());
                }
                args.push(Argument {
                    kind: header.kind,
                    // Provisional: the positional's placeholder name, or
                    // empty for a flag. `finalize_arguments` sanitizes
                    // and de-collides it into the final identifier.
                    ident: header.name_hint.unwrap_or_default(),
                    flag: header.flag,
                    required: header.required,
                    synthesized: false,
                    description,
                });
            }
            None => {
                // A `Note:` / `Tip:` / prose continuation block. Attach
                // it to the most recent argument, separated by a blank
                // line, so the context survives into hover.
                if let Some(prev) = args.last_mut() {
                    prev.description.push(String::new());
                    for line in &block {
                        prev.description.push(line.trim().to_string());
                    }
                }
            }
        }
    }
    args
}

/// Split lines into blocks of consecutive non-empty lines.
fn blocks(lines: &[String]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line.clone());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The structured outcome of parsing an argument header's first line.
struct ArgHeader {
    kind: ArgKind,
    /// Flag name without the dash, or `None` for a positional.
    flag: Option<String>,
    /// A positional's identifier hint, recovered from its `<placeholder>`
    /// (e.g. `<hw_sio_linkgroups>` → `hw_sio_linkgroups`). `None` for a
    /// flag, whose identifier comes from its flag name.
    name_hint: Option<String>,
    required: bool,
    /// The description text that followed the ` - ` separator on the
    /// header line.
    prose: String,
}

/// Parse the first line of an argument block. Returns `None` when the
/// line is not a flag/positional header (so the caller treats the
/// block as a note attached to the previous argument).
fn parse_arg_header(line: &str) -> Option<ArgHeader> {
    let mut input = line;
    if let Ok(flag) = flag_lead.parse_next(&mut input) {
        let (placeholder, prose) = split_separator(input);
        let kind = if placeholder.trim().is_empty() {
            ArgKind::Boolean
        } else {
            ArgKind::Value
        };
        return Some(ArgHeader {
            kind,
            flag: Some(flag.to_string()),
            name_hint: None,
            required: is_required(&prose),
            prose,
        });
    }
    if let Ok(inner) = positional_lead.parse_next(&mut input) {
        let (_ellipsis, prose) = split_separator(input);
        return Some(ArgHeader {
            kind: ArgKind::Positional,
            flag: None,
            name_hint: first_ident_token(inner),
            required: is_required(&prose),
            prose,
        });
    }
    None
}

/// The first `[A-Za-z_][A-Za-z0-9_]*` token in a positional's
/// placeholder text (`hw_sio_linkgroups`, `arg1 arg2 ...` → `arg1`).
/// `None` when the placeholder has no identifier-shaped run (`[0:750]`).
fn first_ident_token(inner: &str) -> Option<String> {
    let mut chars = inner.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_ascii_alphabetic() || c == '_' {
            let mut end = start;
            for (i, c) in inner[start..].char_indices() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    end = start + i + c.len_utf8();
                } else {
                    break;
                }
            }
            return Some(inner[start..end].to_string());
        }
        chars.next();
    }
    None
}

/// `-ident` — consume the dash and flag name, leaving the rest of the
/// line in `input`. Returns the flag name without the dash.
fn flag_lead<'s>(input: &mut &'s str) -> ModalResult<&'s str> {
    preceded('-', ident).parse_next(input)
}

/// `<...>` (with an optional trailing `...`) — consume the angle-bracket
/// placeholder that introduces a positional operand, leaving the rest
/// of the line in `input`. Returns the text inside the brackets.
fn positional_lead<'s>(input: &mut &'s str) -> ModalResult<&'s str> {
    '<'.parse_next(input)?;
    let inner = take_while(0.., |c: char| c != '>').parse_next(input)?;
    '>'.parse_next(input)?;
    let _ = opt("...").parse_next(input)?;
    // Note: do not consume the space after `>` — it is part of the
    // ` - ` separator that `split_separator` looks for.
    Ok(inner)
}

/// An htcl-identifier run: `[A-Za-z0-9_]+`.
fn ident<'s>(input: &mut &'s str) -> ModalResult<&'s str> {
    take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_')
        .parse_next(input)
}

/// Split a header remainder on its first ` - ` separator into the
/// (placeholder, description) halves. With no separator the whole
/// remainder is taken as the description (and the placeholder is
/// empty), which makes a bare `-flag` a boolean toggle.
fn split_separator(rest: &str) -> (String, String) {
    match rest.find(" - ") {
        Some(idx) => (
            rest[..idx].trim().to_string(),
            rest[idx + 3..].trim().to_string(),
        ),
        None => (String::new(), rest.trim().to_string()),
    }
}

/// Whether an argument's description marks it `(Required)`.
fn is_required(prose: &str) -> bool {
    prose
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("(required")
}

// ---------------------------------------------------------------------------
// See Also.
// ---------------------------------------------------------------------------

/// Extract the command names from `See Also` bullet lines
/// (`   *  get_clocks`).
fn parse_see_also(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        if let Ok(name) = see_also_entry.parse_next(&mut line.as_str()) {
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// `   *  <name>` — a See-Also bullet. Returns the command name.
fn see_also_entry<'s>(input: &mut &'s str) -> ModalResult<&'s str> {
    space0.parse_next(input)?;
    '*'.parse_next(input)?;
    space0.parse_next(input)?;
    take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_')
        .parse_next(input)
}

// ---------------------------------------------------------------------------
// Finalization: identifiers, de-collision, synthesized operands.
// ---------------------------------------------------------------------------

/// Assign final htcl identifiers, de-collide duplicates, and synthesize
/// a generic trailing operand when the page documented no positional.
fn finalize_arguments(page: &mut ManPage) {
    let mut used: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut has_positional = false;

    let args = std::mem::take(&mut page.arguments);
    for mut arg in args {
        let base = match arg.kind {
            ArgKind::Positional => {
                has_positional = true;
                if arg.ident.is_empty() {
                    "operands".to_string()
                } else {
                    arg.ident.clone()
                }
            }
            _ => arg.flag.clone().unwrap_or_else(|| "arg".to_string()),
        };
        let base = sanitize_ident(&base);
        // A duplicate flag (the page listing `-foo` twice) is dropped;
        // a positional that collides with a flag is renamed.
        if used.contains(&base) {
            if arg.is_flag() {
                continue;
            }
            arg.ident = unique_ident(&base, &mut used);
        } else {
            used.insert(base.clone());
            arg.ident = base;
        }
        page.arguments.push(arg);
    }

    if !has_positional {
        let ident = unique_ident("operands", &mut used);
        page.arguments.push(Argument {
            kind: ArgKind::Positional,
            ident,
            flag: None,
            required: false,
            synthesized: true,
            description: vec![
                "Positional operands passed through to the underlying \
                 command (object patterns, names, files, …)."
                    .to_string(),
            ],
        });
    }
}

/// First unused identifier in the `base`, `base_2`, `base_3`, … family.
fn unique_ident(
    base: &str,
    used: &mut std::collections::HashSet<String>,
) -> String {
    let base = sanitize_ident(base);
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("exhausted identifier suffixes")
}

/// Coerce an arbitrary string into the htcl proc-arg grammar
/// (`[A-Za-z_][A-Za-z0-9_]*`). Non-conforming characters become
/// underscores; a digit-leading or empty result gets a leading
/// underscore. Never produces the Tcl-reserved varargs name `args`.
fn sanitize_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 1);
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let needs_lead = out
        .as_bytes()
        .first()
        .map(|b| b.is_ascii_digit())
        .unwrap_or(true);
    if needs_lead {
        out.insert(0, '_');
    }
    if out == "args" {
        out = "args_".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(line: &str) -> ArgHeader {
        parse_arg_header(line).expect("expected an argument header")
    }

    #[test]
    fn classifies_value_flag() {
        let h = header("-fileset <name> - (Optional) The fileset.");
        assert_eq!(h.kind, ArgKind::Value);
        assert_eq!(h.flag.as_deref(), Some("fileset"));
        assert!(!h.required);
        assert!(h.prose.starts_with("(Optional)"));
    }

    #[test]
    fn classifies_boolean_flag() {
        let h = header("-norecurse - (Optional) Do not recurse.");
        assert_eq!(h.kind, ArgKind::Boolean);
        assert_eq!(h.flag.as_deref(), Some("norecurse"));
    }

    #[test]
    fn required_value_flag() {
        let h = header("-period <arg> - (Required) The period.");
        assert_eq!(h.kind, ArgKind::Value);
        assert!(h.required);
    }

    #[test]
    fn multiword_placeholder_is_value_flag() {
        let h = header("-waveform <arg1 arg2 ...> - (Optional) Edges.");
        assert_eq!(h.kind, ArgKind::Value);
    }

    #[test]
    fn classifies_positional_and_recovers_name() {
        let h = header("<hw_sio_linkgroups> - (Required) Objects to remove.");
        assert_eq!(h.kind, ArgKind::Positional);
        assert_eq!(h.name_hint.as_deref(), Some("hw_sio_linkgroups"));
        assert!(h.required);
    }

    #[test]
    fn positional_without_marker_is_optional() {
        let h = header("<version> - Version of the library.");
        assert_eq!(h.kind, ArgKind::Positional);
        assert!(!h.required);
    }

    #[test]
    fn non_header_block_is_rejected() {
        assert!(parse_arg_header("Note: this is a note.").is_none());
        assert!(parse_arg_header("Plain prose continuation.").is_none());
    }

    #[test]
    fn first_ident_token_extraction() {
        assert_eq!(first_ident_token("name").as_deref(), Some("name"));
        assert_eq!(first_ident_token("arg1 arg2 ...").as_deref(), Some("arg1"));
        assert_eq!(first_ident_token("[0:750]"), None);
    }

    #[test]
    fn sanitize_ident_never_yields_varargs() {
        assert_eq!(sanitize_ident("args"), "args_");
        assert_eq!(sanitize_ident("64bit"), "_64bit");
        assert_eq!(sanitize_ident("a-b.c"), "a_b_c");
    }

    #[test]
    fn de_collides_positional_against_flag() {
        // A flag `-name` and a positional `<name>` must not collide.
        let page = parse_man_page(
            "demo",
            "\nArguments:\n\n  -name <arg> - (Optional) The flag.\n\n  \
             <name> - (Required) The operand.\n",
        );
        let idents: Vec<&str> =
            page.arguments.iter().map(|a| a.ident.as_str()).collect();
        assert!(idents.contains(&"name"));
        assert!(idents.contains(&"name_2"));
    }

    #[test]
    fn drops_duplicate_flag() {
        let page = parse_man_page(
            "demo",
            "\nArguments:\n\n  -quiet - (Optional) Quietly.\n\n  \
             -quiet - (Optional) Quietly again.\n",
        );
        let quiets =
            page.arguments.iter().filter(|a| a.ident == "quiet").count();
        assert_eq!(quiets, 1);
    }
}
