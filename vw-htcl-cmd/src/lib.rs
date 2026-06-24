// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Vivado command reference → htcl wrapper generation.
//!
//! The dual of [`vw_ip`]: where that crate turns an IP-XACT component
//! into a configuration-interface proc, this one turns a Vivado Tcl
//! command's plain-text reference page (under
//! `<Vivado>/doc/eng/man`) into a documented, typed htcl wrapper for
//! that command.
//!
//! Each generated wrapper keeps the command's natural name and shadows
//! the Vivado builtin, forwarding to a `rename`-stashed copy of the
//! original. The payoff is the htcl surface: hover documentation drawn
//! from the man page, `@enum`/`@default` validation on flags, and
//! keyword call sites the analyzer can check — all on the real command
//! names.
//!
//! ```no_run
//! let page = vw_htcl_cmd::load("/opt/Vivado/doc/eng/man/add_files", None)?;
//! let htcl = vw_htcl_cmd::generate(&page, &Default::default());
//! print!("{htcl}");
//! # Ok::<(), vw_htcl_cmd::Error>(())
//! ```

pub mod constraints;
pub mod generate;
pub mod model;
pub mod parse;

pub use constraints::{
    ArgOverride, CommandOverride, ConstraintsError, ConstraintsTable,
};
pub use generate::{generate, GenerateOptions};
pub use model::{ArgKind, Argument, ManPage};
pub use parse::parse_man_page;

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading man page `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot derive a command name from `{0}` (no file stem)")]
    NoName(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Load and parse a man page from disk.
///
/// The command name comes from `name_override` when given, otherwise
/// from the file stem (`.../man/add_files` → `add_files`).
pub fn load(
    path: impl AsRef<Path>,
    name_override: Option<&str>,
) -> Result<ManPage> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    let name = match name_override {
        Some(n) => n.to_string(),
        None => path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::NoName(path.display().to_string()))?
            .to_string(),
    };
    Ok(parse_man_page(&name, &text))
}
