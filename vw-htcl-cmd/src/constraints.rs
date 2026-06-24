// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Per-command signature augmentations layered on top of the
//! man-page-derived wrapper.
//!
//! UG835 gives us each command's flag/positional list and types, but
//! it has no language for the semantic refinements an htcl wrapper
//! benefits from — mutually-exclusive call modes (`set_property`'s
//! `-dict` vs `-name/-value/-objects` pair), inter-argument
//! requirements (`tuser_width @requires has_tuser`), reclassifying
//! a positional into a keyword-form arg with a default. Those live
//! in a TOML file the wrapper-module author hand-maintains alongside
//! the auto-generated `cmd/*.htcl` files.
//!
//! File shape:
//!
//! ```toml
//! [<command>.args.<arg>]
//! default = "..."         # adds/replaces @default(...)
//! enum = ["a", "b"]       # adds/replaces @enum(a, b)
//! clear_enum = true       # drops any @enum the man-page emitted
//! one_of = ["other"]      # adds @one_of(other)
//! requires = ["a", "b"]   # adds @requires(a, b)
//! conflicts = ["a"]       # adds @conflicts(a)
//! ```
//!
//! The generator applies overrides during signature emission. The
//! body emission then follows the post-override arg classification
//! — flipping a flag from `@enum(0, 1)` to `@default("")` makes it
//! a value-taking arg, and the body forwards `-flag $value`
//! instead of `if {$flag} { lappend cmd -flag }`.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConstraintsError {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Per-arg overrides for one command.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ArgOverride {
    /// New `@default(...)` value. Replaces any inherited default.
    #[serde(default)]
    pub default: Option<String>,
    /// New `@enum(...)` choices. Replaces any inherited enum.
    #[serde(default, rename = "enum")]
    pub enum_: Option<Vec<String>>,
    /// Drop any inherited `@enum`. Use when the man-page parsing
    /// modeled an arg as `@enum(0, 1)` (boolean toggle) but it's
    /// actually value-taking.
    #[serde(default)]
    pub clear_enum: bool,
    /// `@one_of(...)` declarations to add. Empty means no addition.
    #[serde(default)]
    pub one_of: Vec<String>,
    /// `@requires(...)` declarations to add.
    #[serde(default)]
    pub requires: Vec<String>,
    /// `@conflicts(...)` declarations to add.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

/// All overrides for one command, indexed by arg ident.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CommandOverride {
    /// Per-arg overrides. The key is the htcl proc-arg identifier
    /// (matches `Argument::ident`).
    #[serde(default)]
    pub args: HashMap<String, ArgOverride>,
}

/// The complete set of overrides loaded from the constraints file.
/// Lookups are by command name (`set_property`, `create_project`,
/// …) — missing entries return `None` and the generator emits the
/// pure man-page-derived wrapper.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ConstraintsTable {
    commands: HashMap<String, CommandOverride>,
}

impl ConstraintsTable {
    /// Empty table — every command falls back to the pure man-page
    /// signature. Used when no `--constraints` was passed.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from a TOML file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConstraintsError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ConstraintsError::Io {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
        toml::from_str(&text).map_err(|e| ConstraintsError::Parse {
            path: path.to_path_buf(),
            source: e,
        })
    }

    /// Per-command overrides, or `None` when nothing is declared
    /// for `command`.
    pub fn for_command(&self, command: &str) -> Option<&CommandOverride> {
        self.commands.get(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_returns_no_overrides() {
        let t = ConstraintsTable::empty();
        assert!(t.for_command("set_property").is_none());
    }

    #[test]
    fn parses_full_arg_override_block() {
        let toml = r#"
            [set_property.args.dict]
            default = ""
            clear_enum = true
            one_of = ["name"]
            requires = ["objects"]

            [set_property.args.name]
            default = ""
            one_of = ["dict"]
            requires = ["value", "objects"]
        "#;
        let t: ConstraintsTable = toml::from_str(toml).unwrap();
        let sp = t.for_command("set_property").unwrap();
        let dict = sp.args.get("dict").unwrap();
        assert_eq!(dict.default.as_deref(), Some(""));
        assert!(dict.clear_enum);
        assert_eq!(dict.one_of, vec!["name".to_string()]);
        assert_eq!(dict.requires, vec!["objects".to_string()]);

        let name = sp.args.get("name").unwrap();
        assert_eq!(name.default.as_deref(), Some(""));
        assert_eq!(name.one_of, vec!["dict".to_string()]);
        assert_eq!(
            name.requires,
            vec!["value".to_string(), "objects".to_string()]
        );
    }

    #[test]
    fn missing_command_returns_none() {
        let toml = r#"
            [set_property.args.dict]
            default = ""
        "#;
        let t: ConstraintsTable = toml::from_str(toml).unwrap();
        assert!(t.for_command("create_project").is_none());
        assert!(t.for_command("set_property").is_some());
    }
}
