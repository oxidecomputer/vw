// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Parse Xilinx IP-XACT paired-list default values into a nested tree.
//!
//! Many Xilinx IP-XACT `<spirit:value>` defaults are Tcl-list-shaped
//! paired dicts — `KEY VAL KEY VAL …`, where a `VAL` can itself be a
//! braced Tcl list holding another paired dict. The gtwiz-versal
//! `INTF0_TXRX_OPTIONAL_PORTS` param is the canonical example: ~300
//! flat fields at the top level, terminating in
//! `INTF_LR_SETTINGS {LR0_SETTINGS {~80 fields} LR1_SETTINGS { } … }`.
//!
//! That structure IS the field schema. The generator uses it to emit
//! typed constructor procs with named args — no more asking the caller
//! to hand-populate `Properties::from -v {…}` from memory. See
//! `cips_dict::DictSchema::from_paired_default` for the extractor that
//! turns a [`PairedValue`] tree into a `DictSchema`.
//!
//! Semantics match Tcl list tokenization: whitespace separates tokens;
//! `{…}` groups a token, stripping only the outermost braces; nested
//! `{…}` are preserved verbatim inside the outer token. Nothing else is
//! interpreted — `$var` / `[cmd]` substitution isn't touched, since
//! IP-XACT defaults arrive with literal text only.

use std::fmt;

/// A parsed value inside a paired-list dict. A `Scalar` is a single
/// bare token (or a braced token whose interior isn't itself a
/// paired list). A `Nested` value is a braced token whose interior
/// parses as another paired list — recursively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairedValue {
    Scalar(String),
    Nested(Vec<(String, PairedValue)>),
}

impl PairedValue {
    /// True when this value is a `Scalar`. Convenience for callers
    /// that iterate paired lists and treat scalar / nested slots
    /// differently.
    pub fn is_scalar(&self) -> bool {
        matches!(self, Self::Scalar(_))
    }

    /// The underlying string for a `Scalar`, or `None` for `Nested`.
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Self::Scalar(s) => Some(s),
            _ => None,
        }
    }
}

impl fmt::Display for PairedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scalar(s) => write!(f, "{s}"),
            Self::Nested(pairs) => {
                write!(f, "{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{k} {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Parse a Tcl-list-shaped paired-dict default string into pairs.
///
/// Returns an empty list when the input doesn't tokenize as an even-
/// count sequence — that's the "not really a paired dict" signal
/// callers use to fall back to treating the whole default as scalar.
/// Empty input returns empty pairs (a valid empty dict).
pub fn parse_paired_list(input: &str) -> Vec<(String, PairedValue)> {
    let cleaned = fix_wrapped_tokens(input);
    let tokens = tokenize(&cleaned);
    if tokens.is_empty() || !tokens.len().is_multiple_of(2) {
        return Vec::new();
    }
    // Bail on non-ident-shaped keys at even indices. A dict whose
    // "keys" don't look like idents is almost certainly a random
    // scalar default that happened to have an even token count —
    // treating it as pairs would produce garbage schema fields.
    for (i, tok) in tokens.iter().enumerate() {
        if i % 2 == 0 && !is_ident_shaped(tok) {
            return Vec::new();
        }
    }
    let mut pairs = Vec::with_capacity(tokens.len() / 2);
    let mut iter = tokens.into_iter();
    while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
        pairs.push((k, classify_value(&v)));
    }
    pairs
}

/// Classify a token as `Scalar` or `Nested` by attempting to re-parse
/// its content as another paired list. If the re-parse yields at
/// least one valid pair, it's `Nested`; otherwise it's a `Scalar`
/// carrying the original text verbatim.
///
/// An empty string classifies as `Nested([])` — an empty inner dict,
/// which is exactly the shape `LR1_SETTINGS { }` … `LR15_SETTINGS { }`
/// take in the `INTF_LR_SETTINGS` payload. Preserving the nested
/// classification for empties matters so the generator sees a
/// consistent tree shape across every LRn slot even when the anchor
/// only populates LR0.
fn classify_value(text: &str) -> PairedValue {
    if text.is_empty() || text.chars().all(char::is_whitespace) {
        return PairedValue::Nested(Vec::new());
    }
    let inner = parse_paired_list(text);
    if inner.is_empty() {
        PairedValue::Scalar(text.to_string())
    } else {
        PairedValue::Nested(inner)
    }
}

/// True when `s` is shaped like a bare identifier — leading letter or
/// underscore, then letters / digits / underscore / dot. Matches the
/// key form Xilinx uses everywhere in paired-list defaults
/// (`RX_REFCLK_FREQUENCY`, `ch_txdata`, `CONFIG.CPM_PCIE0_MODES`).
fn is_ident_shaped(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Repair Xilinx-style mid-identifier line wraps in IP-XACT
/// `<spirit:value>` payloads. Their XML formatter wraps long lines
/// by inserting `\n` INSIDE a token (`ch_txpr\necursor3` instead of
/// `ch_txprecursor3`) — the standard Tcl-list tokenizer then splits
/// the token in half, blowing the paired-list even-count invariant.
///
/// The repair: when `\n` sits between two identifier characters,
/// drop it — reconstructs the original token. Newlines that are
/// legitimate whitespace between tokens (adjacent to other
/// whitespace or non-identifier chars) survive unchanged.
fn fix_wrapped_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    for i in 0..bytes.len() {
        let c = bytes[i] as char;
        if c == '\n' {
            let prev_ok = i > 0 && is_ident_byte(bytes[i - 1]);
            let next_ok = i + 1 < bytes.len() && is_ident_byte(bytes[i + 1]);
            if prev_ok && next_ok {
                // Wrapped mid-token — drop.
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

/// Tokenize a Tcl list. Whitespace separates tokens; `{` opens a
/// braced token that swallows characters up to the matching `}`,
/// with nested `{…}` preserved verbatim (only the OUTER braces are
/// stripped from the emitted token).
///
/// Unbalanced braces silently truncate — sufficient for well-formed
/// IP-XACT defaults and simpler than a full error type. Callers that
/// need to detect malformed input can compare tokenized length to
/// expected pair count.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&c) = chars.peek() else { break };
        let mut tok = String::new();
        if c == '{' {
            chars.next();
            let mut depth = 1_usize;
            for ch in chars.by_ref() {
                if ch == '{' {
                    depth += 1;
                    tok.push(ch);
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    tok.push(ch);
                } else {
                    tok.push(ch);
                }
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                tok.push(ch);
                chars.next();
            }
        }
        tokens.push(tok);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(s: &str) -> PairedValue {
        PairedValue::Scalar(s.into())
    }

    fn nested(pairs: Vec<(&str, PairedValue)>) -> PairedValue {
        PairedValue::Nested(
            pairs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        )
    }

    #[test]
    fn flat_paired_list() {
        // The `intf::channel_map` shape — 4 pairs, all scalar values.
        let src = "INTF0_RX0 QUAD0_RX0 INTF0_TX0 QUAD0_TX0";
        let pairs = parse_paired_list(src);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "INTF0_RX0");
        assert_eq!(pairs[0].1, scalar("QUAD0_RX0"));
        assert_eq!(pairs[1].0, "INTF0_TX0");
        assert_eq!(pairs[1].1, scalar("QUAD0_TX0"));
    }

    #[test]
    fn one_level_nested_lr_settings() {
        // The `INTF_LR_SETTINGS` payload shape — one nested value.
        let src = "LR0_SETTINGS {RX_REFCLK_FREQUENCY 156.25 TX_REFCLK_FREQUENCY 156.25}";
        let pairs = parse_paired_list(src);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "LR0_SETTINGS");
        assert_eq!(
            pairs[0].1,
            nested(vec![
                ("RX_REFCLK_FREQUENCY", scalar("156.25")),
                ("TX_REFCLK_FREQUENCY", scalar("156.25")),
            ]),
        );
    }

    #[test]
    fn two_level_nesting_matches_txrx_optional_ports_shape() {
        // The `INTF0_TXRX_OPTIONAL_PORTS` anchor shape — flat outer
        // pairs terminating in a nested INTF_LR_SETTINGS block whose
        // inner values are themselves paired dicts.
        let src = "GT_TYPE GTY GT_DIRECTION DUPLEX INTF_LR_SETTINGS \
                   {LR0_SETTINGS {RX_HD_EN 0 TX_HD_EN 0} LR1_SETTINGS { }}";
        let pairs = parse_paired_list(src);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("GT_TYPE".into(), scalar("GTY")));
        assert_eq!(pairs[1], ("GT_DIRECTION".into(), scalar("DUPLEX")));
        let PairedValue::Nested(intf_lr) = &pairs[2].1 else {
            panic!("expected nested INTF_LR_SETTINGS payload");
        };
        assert_eq!(intf_lr.len(), 2);
        assert_eq!(intf_lr[0].0, "LR0_SETTINGS");
        assert_eq!(
            intf_lr[0].1,
            nested(vec![("RX_HD_EN", scalar("0")), ("TX_HD_EN", scalar("0")),]),
        );
        // LR1_SETTINGS { } — empty braces classify as an empty
        // Nested dict, not a Scalar with empty content. Critical:
        // otherwise the schema for LR1 would come out as "one field
        // named ''" instead of "nested slot, presently empty".
        assert_eq!(intf_lr[1].0, "LR1_SETTINGS");
        assert_eq!(intf_lr[1].1, PairedValue::Nested(Vec::new()));
    }

    #[test]
    fn odd_token_count_yields_empty_pairs() {
        // Signal to the caller: "this isn't a paired dict, treat the
        // whole default as scalar." The `intf::gt_settings` param has
        // default `0` (a single token) — must NOT parse as a pair.
        assert!(parse_paired_list("0").is_empty());
        assert!(parse_paired_list("Custom").is_empty());
        assert!(parse_paired_list("A B C").is_empty());
    }

    #[test]
    fn empty_input_yields_empty_pairs() {
        assert!(parse_paired_list("").is_empty());
        assert!(parse_paired_list("   ").is_empty());
    }

    #[test]
    fn non_ident_keys_reject_pair_interpretation() {
        // `NA NA` — the current-generator misfire that would parse as
        // one pair with key=NA (ident-shaped) value=NA. That IS
        // ident-shaped so it DOES parse. Test the reject path with
        // something that shouldn't.
        assert!(parse_paired_list("32.0 GT/s").is_empty(), "leading digit");
        assert!(parse_paired_list("--foo bar").is_empty(), "leading punct");
    }

    #[test]
    fn scalar_value_with_dot() {
        // Frequencies (`156.25`, `10.3125`) tokenize as ident-shaped
        // under our rules (dot allowed). Match Xilinx's usage — a
        // pair like `RX_LINE_RATE 10.3125` should classify the value
        // as scalar even though `10.3125` passes `is_ident_shaped`.
        // classify_value tries to re-parse; a single token doesn't
        // form a pair; falls back to Scalar. Belt-and-suspenders test.
        let pairs = parse_paired_list("RX_LINE_RATE 10.3125");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1, scalar("10.3125"));
    }

    #[test]
    fn deeply_nested_braces_preserved() {
        // Tokenizer strips ONLY the outer braces — inner `{…}` are
        // kept verbatim so downstream re-parses see the same shape.
        let toks = tokenize("A {B {C D} E} F G");
        assert_eq!(toks, vec!["A", "B {C D} E", "F", "G"]);
    }

    #[test]
    fn empty_braces_tokenize_as_empty_string() {
        // `LR1_SETTINGS { }` — the value tokenizes to `""` (or `" "`
        // then trimmed by classify_value). Either way, classifies as
        // an empty nested dict, not a lone Scalar.
        let toks = tokenize("K { }");
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0], "K");
        assert!(toks[1].chars().all(char::is_whitespace) || toks[1].is_empty());
    }
}
