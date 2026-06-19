// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The structured model of a Vivado command reference ("man page").
//!
//! Vivado ships a plain-text reference page per Tcl command under
//! `doc/eng/man`. Each page follows a regular shape:
//!
//! ```text
//! Description:
//!
//!   <prose, possibly several paragraphs>
//!
//! Arguments:
//!
//!   -fileset <name> - (Optional) <prose>
//!   -norecurse - (Optional) <prose>
//!   <files> - (Required) <prose>
//!
//! Examples:
//!   ...
//!
//! See Also:
//!
//!    *  import_files
//!    *  read_ip
//! ```
//!
//! [`crate::parse`] turns that text into a [`ManPage`]; [`crate::generate`]
//! turns a [`ManPage`] into an htcl wrapper proc.

/// A parsed Vivado command reference page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManPage {
    /// The command name (e.g. `add_files`). Derived from the source
    /// file stem, not the page body — the body never repeats it.
    pub name: String,
    /// The `Description:` section, de-indented, one entry per source
    /// line. Empty lines are preserved as empty strings so paragraph
    /// breaks survive into the emitted doc comment.
    pub description: Vec<String>,
    /// The `Arguments:` section, one entry per documented flag or
    /// positional operand, in declared order.
    pub arguments: Vec<Argument>,
    /// Command names listed under `See Also:`.
    pub see_also: Vec<String>,
}

/// How an argument maps onto the underlying Vivado command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// A `-flag` with no value placeholder: a boolean toggle. Emitted
    /// as `@enum(0, 1) @default(0)` and forwarded as a bare `-flag`
    /// when set.
    Boolean,
    /// A `-flag <value>`: forwarded as `-flag $value` when non-empty.
    Value,
    /// A trailing positional operand (`<objects>`, `<files>`, …):
    /// forwarded by list-expansion (`{*}$operands`) at the end of the
    /// command line.
    Positional,
}

/// One documented argument of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argument {
    pub kind: ArgKind,
    /// The htcl proc-arg identifier the caller uses as `-<ident>`.
    /// Equal to `flag` for flags; derived from the `<placeholder>` for
    /// positionals. May be de-collided with a suffix.
    pub ident: String,
    /// The underlying Vivado flag name without its leading dash
    /// (`fileset`, `norecurse`). `None` for positionals, which have no
    /// flag on the command line.
    pub flag: Option<String>,
    /// Whether the man page marked the argument `(Required)`. Required
    /// arguments are emitted without an `@default`, so htcl forces the
    /// caller to supply them.
    pub required: bool,
    /// Whether this is a generic operand placeholder synthesized by the
    /// generator (the page documented no positional), rather than one
    /// taken from the page text.
    pub synthesized: bool,
    /// The argument's prose description, de-indented, one entry per
    /// source line (empty strings preserve paragraph breaks).
    pub description: Vec<String>,
}

impl Argument {
    /// `true` for flags (`-flag` / `-flag <value>`), `false` for
    /// positionals.
    pub fn is_flag(&self) -> bool {
        matches!(self.kind, ArgKind::Boolean | ArgKind::Value)
    }
}
