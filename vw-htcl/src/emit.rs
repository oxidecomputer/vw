// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Build and emit htcl source code.
//!
//! Distinct from [`crate::ast`], which is the parser's CST and carries
//! spans, doc comments as raw text, and a structure optimized for
//! analysis. `emit` is the dual: for code generation. No spans,
//! ergonomic constructors, and a [`Display`](std::fmt::Display) impl
//! that produces well-formed, indented htcl text.
//!
//! The model is small on purpose. The [`Word`] variants line up with
//! the parser's [`crate::ast::WordForm`] / [`crate::ast::WordPart`]
//! distinctions (bare / quoted / braced / `$var` / `[cmd]`), and
//! [`Word::lit`] picks the safest word form for a runtime string. The
//! [`ToHtcl`] trait is the interpolation interface used by `vw-quote`'s
//! `quote_htcl!` macro and by hand-written generators.

use std::fmt;

/// A complete htcl document being built.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub items: Vec<Item>,
}

impl Doc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, item: impl Into<Item>) -> &mut Self {
        self.items.push(item.into());
        self
    }

    pub fn cmd(&mut self, cmd: Command) -> &mut Self {
        self.items.push(Item::Command(cmd));
        self
    }

    pub fn comment(&mut self, text: impl Into<String>) -> &mut Self {
        self.items.push(Item::Comment(text.into()));
        self
    }

    pub fn doc(&mut self, text: impl Into<String>) -> &mut Self {
        self.items.push(Item::DocComment(text.into()));
        self
    }

    pub fn blank(&mut self) -> &mut Self {
        self.items.push(Item::Blank);
        self
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Command(Command),
    /// Regular `# ...` comment (one line, no leading `#`).
    Comment(String),
    /// Doc `## ...` comment (one line, no leading `##`). Doc comments
    /// attached to a specific command live on [`Command::doc_comments`].
    DocComment(String),
    /// Emit a blank line.
    Blank,
}

impl From<Command> for Item {
    fn from(c: Command) -> Self {
        Item::Command(c)
    }
}

/// A single htcl command (one logical line, possibly with a body
/// block).
#[derive(Clone, Debug, Default)]
pub struct Command {
    /// `##` doc comments emitted immediately above the command.
    pub doc_comments: Vec<String>,
    /// The command name and its arguments, in order.
    pub words: Vec<Word>,
    /// Optional braced body emitted as `{ … }` after the words, with
    /// its contents indented. Used by `proc`, `if`, `while`, etc.
    pub body: Option<Doc>,
}

impl Command {
    /// `name arg1 arg2 …` with no body. Most generic command shape.
    pub fn call<I, W>(name: impl Into<Word>, args: I) -> Self
    where
        I: IntoIterator<Item = W>,
        W: Into<Word>,
    {
        let mut words = vec![name.into()];
        words.extend(args.into_iter().map(Into::into));
        Self {
            words,
            ..Self::default()
        }
    }

    pub fn with_doc(mut self, doc: impl Into<String>) -> Self {
        self.doc_comments.push(doc.into());
        self
    }

    pub fn with_body(mut self, body: Doc) -> Self {
        self.body = Some(body);
        self
    }
}

/// One word of an htcl command.
///
/// The variants correspond to the parser's word forms. Prefer
/// [`Word::lit`] when you have a runtime string and want the safest
/// form chosen for you; the named constructors are for when you know
/// the form (e.g. you're producing a `$var` reference deliberately).
#[derive(Clone, Debug)]
pub enum Word {
    /// A bare unquoted word. Caller is responsible for ensuring `s`
    /// contains no whitespace or shell-special characters; prefer
    /// [`Word::lit`] when in doubt.
    Bare(String),
    /// A double-quoted word (`"…"`). Tcl substitution applies inside;
    /// the content is escaped during emit so embedded `"` and `\`
    /// are safe.
    Quoted(String),
    /// A braced word (`{…}`). No substitution; embedded `{`/`}` are
    /// the caller's responsibility (typically rare).
    Braced(String),
    /// A `$name` variable reference.
    Var(String),
    /// A `[ cmd ]` command substitution; `s` is the interior text,
    /// emitted verbatim.
    CmdSubst(String),
    /// Pre-formatted text inserted as-is. Caller is responsible for
    /// it being a valid single word. Useful when composing fragments
    /// produced elsewhere.
    Raw(String),
}

impl Word {
    /// Choose the smallest safe word form for `s`: bare when it
    /// contains only word-safe ASCII characters, double-quoted with
    /// escapes otherwise. Empty strings become `""`.
    pub fn lit(s: impl Into<String>) -> Word {
        let s = s.into();
        if needs_quoting(&s) {
            Word::Quoted(s)
        } else {
            Word::Bare(s)
        }
    }

    /// `$name` reference. The name is not validated.
    pub fn var(name: impl Into<String>) -> Word {
        Word::Var(name.into())
    }
}

fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    s.chars().any(|c| {
        c.is_whitespace()
            || matches!(c, ';' | '"' | '\\' | '[' | ']' | '{' | '}' | '$' | '#')
    })
}

impl From<&str> for Word {
    fn from(s: &str) -> Self {
        Word::lit(s)
    }
}

impl From<String> for Word {
    fn from(s: String) -> Self {
        Word::lit(s)
    }
}

// ---------------------------------------------------------------------------
// ToHtcl — the interpolation interface for `quote_htcl!`.
// ---------------------------------------------------------------------------

/// Produce a [`Word`] for interpolation into emitted htcl.
///
/// Implemented for the common Rust value types. Pass any `T: ToHtcl`
/// to `#expr` slots in `quote_htcl!`; the macro calls
/// `(&expr).to_htcl()` to get the inserted word.
pub trait ToHtcl {
    fn to_htcl(&self) -> Word;
}

impl ToHtcl for Word {
    fn to_htcl(&self) -> Word {
        self.clone()
    }
}
impl ToHtcl for str {
    fn to_htcl(&self) -> Word {
        Word::lit(self)
    }
}
impl ToHtcl for String {
    fn to_htcl(&self) -> Word {
        Word::lit(self.clone())
    }
}
impl<T: ToHtcl + ?Sized> ToHtcl for &T {
    fn to_htcl(&self) -> Word {
        (*self).to_htcl()
    }
}
impl ToHtcl for bool {
    fn to_htcl(&self) -> Word {
        Word::Bare(if *self { "1".into() } else { "0".into() })
    }
}

macro_rules! impl_to_htcl_display {
    ($($t:ty),* $(,)?) => {
        $(
            impl ToHtcl for $t {
                fn to_htcl(&self) -> Word {
                    Word::Bare(self.to_string())
                }
            }
        )*
    };
}
impl_to_htcl_display!(i8, i16, i32, i64, i128, isize);
impl_to_htcl_display!(u8, u16, u32, u64, u128, usize);
impl_to_htcl_display!(f32, f64);

// ---------------------------------------------------------------------------
// ToTcl — interpolation interface for `quote_tcl!`.
// ---------------------------------------------------------------------------

/// Produce a [`Word`] for interpolation into emitted *pure Tcl*.
///
/// Distinct from [`ToHtcl`] so that compiler intrinsics (the `repr`
/// codegen module, `kwargs` shim helpers, future ones) which emit
/// Tcl bodies — not htcl — can carry an independent vocabulary if
/// they grow it. For now the surface is intentionally identical:
/// the same Rust value types yield the same [`Word`] under both
/// traits. The split exists so future Tcl-only forms (typed
/// `Tcl_Obj` handle quoting, namespaced-proc-name formatting,
/// etc.) can land on `ToTcl` without changing `ToHtcl`'s contract.
pub trait ToTcl {
    fn to_tcl(&self) -> Word;
}

impl ToTcl for Word {
    fn to_tcl(&self) -> Word {
        self.clone()
    }
}
impl ToTcl for str {
    fn to_tcl(&self) -> Word {
        Word::lit(self)
    }
}
impl ToTcl for String {
    fn to_tcl(&self) -> Word {
        Word::lit(self.clone())
    }
}
impl<T: ToTcl + ?Sized> ToTcl for &T {
    fn to_tcl(&self) -> Word {
        (*self).to_tcl()
    }
}
impl ToTcl for bool {
    fn to_tcl(&self) -> Word {
        Word::Bare(if *self { "1".into() } else { "0".into() })
    }
}

macro_rules! impl_to_tcl_display {
    ($($t:ty),* $(,)?) => {
        $(
            impl ToTcl for $t {
                fn to_tcl(&self) -> Word {
                    Word::Bare(self.to_string())
                }
            }
        )*
    };
}
impl_to_tcl_display!(i8, i16, i32, i64, i128, isize);
impl_to_tcl_display!(u8, u16, u32, u64, u128, usize);
impl_to_tcl_display!(f32, f64);

// ---------------------------------------------------------------------------
// Emit — Display impls produce well-formed htcl text.
// ---------------------------------------------------------------------------

impl fmt::Display for Doc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit_doc(f, self, 0)
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit_item(f, self, 0)
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit_command(f, self, 0)
    }
}

impl fmt::Display for Word {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        emit_word(f, self)
    }
}

const INDENT: &str = "  ";

fn emit_indent(f: &mut fmt::Formatter<'_>, level: usize) -> fmt::Result {
    for _ in 0..level {
        f.write_str(INDENT)?;
    }
    Ok(())
}

fn emit_doc(
    f: &mut fmt::Formatter<'_>,
    doc: &Doc,
    level: usize,
) -> fmt::Result {
    for item in &doc.items {
        emit_item(f, item, level)?;
    }
    Ok(())
}

fn emit_item(
    f: &mut fmt::Formatter<'_>,
    item: &Item,
    level: usize,
) -> fmt::Result {
    match item {
        Item::Command(c) => emit_command(f, c, level),
        Item::Comment(text) => {
            emit_indent(f, level)?;
            writeln!(f, "# {text}")
        }
        Item::DocComment(text) => {
            emit_indent(f, level)?;
            writeln!(f, "## {text}")
        }
        Item::Blank => writeln!(f),
    }
}

fn emit_command(
    f: &mut fmt::Formatter<'_>,
    cmd: &Command,
    level: usize,
) -> fmt::Result {
    for doc in &cmd.doc_comments {
        emit_indent(f, level)?;
        writeln!(f, "## {doc}")?;
    }
    emit_indent(f, level)?;
    let mut first = true;
    for w in &cmd.words {
        if !first {
            f.write_str(" ")?;
        }
        emit_word(f, w)?;
        first = false;
    }
    if let Some(body) = &cmd.body {
        if body.items.is_empty() {
            f.write_str(" {}\n")?;
        } else {
            f.write_str(" {\n")?;
            emit_doc(f, body, level + 1)?;
            emit_indent(f, level)?;
            f.write_str("}\n")?;
        }
    } else {
        f.write_str("\n")?;
    }
    Ok(())
}

fn emit_word(f: &mut fmt::Formatter<'_>, w: &Word) -> fmt::Result {
    match w {
        Word::Bare(s) => f.write_str(s),
        Word::Quoted(s) => {
            f.write_str("\"")?;
            for c in s.chars() {
                match c {
                    '\\' => f.write_str("\\\\")?,
                    '"' => f.write_str("\\\"")?,
                    '$' => f.write_str("\\$")?,
                    '[' => f.write_str("\\[")?,
                    ']' => f.write_str("\\]")?,
                    other => f.write_fmt(format_args!("{other}"))?,
                }
            }
            f.write_str("\"")
        }
        Word::Braced(s) => {
            f.write_str("{")?;
            f.write_str(s)?;
            f.write_str("}")
        }
        Word::Var(name) => {
            f.write_str("$")?;
            f.write_str(name)
        }
        Word::CmdSubst(s) => {
            f.write_str("[")?;
            f.write_str(s)?;
            f.write_str("]")
        }
        Word::Raw(s) => f.write_str(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_lit_picks_bare_when_safe() {
        assert!(
            matches!(Word::lit("hello"), Word::Bare(ref s) if s == "hello")
        );
        assert!(matches!(Word::lit("32.0_GT/s"), Word::Bare(_)));
    }

    #[test]
    fn word_lit_quotes_when_special() {
        let cases = ["with space", "has\"quote", "has$dollar", "has;semi", ""];
        for c in cases {
            assert!(
                matches!(Word::lit(c), Word::Quoted(_)),
                "expected quoted for {c:?}"
            );
        }
    }

    #[test]
    fn emit_command_simple() {
        let cmd = Command::call("puts", ["hi"]);
        assert_eq!(format!("{cmd}"), "puts hi\n");
    }

    #[test]
    fn emit_command_quotes_when_needed() {
        let cmd = Command::call("puts", ["hello world"]);
        assert_eq!(format!("{cmd}"), "puts \"hello world\"\n");
    }

    #[test]
    fn emit_doc_full_proc() {
        // proc greet {name} { puts "hi $name" }
        let inner = Command::call("puts", [Word::Quoted("hi $name".into())]);
        let body = {
            let mut d = Doc::new();
            d.cmd(inner);
            d
        };
        let proc = Command {
            doc_comments: vec!["Say hi.".into()],
            words: vec![
                Word::Bare("proc".into()),
                Word::Bare("greet".into()),
                Word::Braced("name".into()),
            ],
            body: Some(body),
        };
        let mut doc = Doc::new();
        doc.cmd(proc);
        let out = format!("{doc}");
        let expected = "\
## Say hi.
proc greet {name} {
  puts \"hi \\$name\"
}
";
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_body_emits_braces() {
        let cmd = Command {
            words: vec![
                Word::Bare("proc".into()),
                Word::Bare("f".into()),
                Word::Braced("".into()),
            ],
            body: Some(Doc::new()),
            ..Default::default()
        };
        assert_eq!(format!("{cmd}"), "proc f {} {}\n");
    }

    #[test]
    fn to_htcl_basic_types() {
        assert!(matches!("hi".to_htcl(), Word::Bare(ref s) if s == "hi"));
        assert!(matches!(42i64.to_htcl(), Word::Bare(ref s) if s == "42"));
        assert!(matches!(true.to_htcl(), Word::Bare(ref s) if s == "1"));
    }

    #[test]
    fn emitted_output_round_trips_through_parser() {
        // Build a doc, emit it, re-parse, and check we get a structurally
        // similar document — proves the emitter is producing well-formed
        // htcl that the parser accepts.
        use crate::parser::parse;
        let mut body = Doc::new();
        body.cmd(Command::call("puts", [Word::Quoted("hi $name".into())]));
        let proc = Command {
            words: vec![
                Word::Bare("proc".into()),
                Word::Bare("greet".into()),
                Word::Braced("name".into()),
            ],
            body: Some(body),
            ..Default::default()
        };
        let mut doc = Doc::new();
        doc.cmd(proc);
        let text = doc.to_string();
        let parsed = parse(&text);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        // First (and only) statement should be the proc.
        let stmt = &parsed.document.stmts[0];
        let crate::ast::Stmt::Command(cmd) = stmt else {
            panic!("expected command, got {stmt:?}");
        };
        let crate::ast::CommandKind::Proc(p) = &cmd.kind else {
            panic!("expected proc");
        };
        assert_eq!(p.name.as_deref(), Some("greet"));
    }
}
