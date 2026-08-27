// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Reading a design entity's interface, so a generated harness can be wired
//! to it.
//!
//! Everything the `--dut` forms of `vw bench init` / `vw cosim init` /
//! `vw mist init` write is derived from what is found here: a signal per port
//! with the port's own type, the generic map, the port map, the Rust handle
//! per port. Typing those out by hand against a fifty-port entity is the part
//! of writing a testbench that is pure transcription, and transcription is
//! where the mistakes are.
//!
//! The types are rendered back to VHDL from `vhdl_lang`'s own AST rather than
//! scraped out of the source with a regex, so `std_logic_vector(DATA_W - 1
//! downto 0)` survives with its generic reference intact and a record port
//! stays a record port.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use camino::Utf8Path;
use vhdl_lang::ast::{
    AnyDesignUnit, AnyPrimaryUnit, AnySecondaryUnit, ConcurrentStatement,
    InstantiatedUnit, InterfaceDeclaration, InterfaceList, Mode,
    ModeIndication, TypeDeclaration, TypeDefinition,
};
use vhdl_lang::VHDLParser;
use vw_core::visitor::{walk_design_file, Visitor, VisitorResult};

use crate::{Result, VhdlStandard, VwError};

/// A port's direction.
///
/// `vhdl_lang`'s `Mode` restated so callers — and `vw-cli` — don't have to
/// depend on `vhdl_lang` to ask which way a port points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortMode {
    In,
    Out,
    InOut,
    Buffer,
}

impl PortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PortMode::In => "in",
            PortMode::Out => "out",
            PortMode::InOut => "inout",
            PortMode::Buffer => "buffer",
        }
    }

    /// True when a harness has to drive this port rather than read it.
    pub fn is_driven(self) -> bool {
        matches!(self, PortMode::In | PortMode::InOut)
    }
}

/// One generic or port, as the entity declared it.
#[derive(Clone, Debug)]
pub struct Interface {
    pub name: String,
    /// `None` for a generic.
    pub mode: Option<PortMode>,
    /// The subtype indication rendered back to VHDL — `std_logic_vector(31
    /// downto 0)`, `Pam4Symbol`, `positive`.
    pub subtype: String,
    /// The declared default, if there was one.
    pub default: Option<String>,
}

impl Interface {
    /// The direction, defaulting to `in` the way VHDL itself does.
    pub fn mode(&self) -> PortMode {
        self.mode.unwrap_or(PortMode::In)
    }

    /// True when this port looks like the design's clock.
    ///
    /// By name: a clock's type is `std_logic` or some subtype of it, which is
    /// not a distinction worth resolving the design's types for, and callers
    /// that need the signal to be reachable check that separately.
    pub fn is_clock(&self) -> bool {
        let n = self.name.to_lowercase();
        n == "clk"
            || n == "clock"
            || n.ends_with("_clk")
            || n.ends_with("_clock")
    }

    /// True when this port looks like the design's reset.
    pub fn is_reset(&self) -> bool {
        let n = self.name.to_lowercase();
        matches!(
            n.as_str(),
            "rst" | "rstn" | "rst_n" | "reset" | "resetn" | "reset_n"
        )
    }

    /// True when the reset this port names is asserted low.
    pub fn is_active_low_reset(&self) -> bool {
        let n = self.name.to_lowercase();
        self.is_reset() && (n.ends_with('n') || n.ends_with("_n"))
    }
}

/// An entity's interface, plus enough of its file to write a harness that
/// compiles against it.
#[derive(Clone, Debug)]
pub struct EntityInterface {
    /// The entity name with the declaration's own spelling preserved —
    /// `TxEq`, not `txeq`.
    pub name: String,
    /// Where it was found.
    pub file: PathBuf,
    /// The `library` / `use` clauses its file opens with. A harness that
    /// mentions the entity's types needs the same ones in scope.
    pub context: Vec<String>,
    pub generics: Vec<Interface>,
    pub ports: Vec<Interface>,
}

impl EntityInterface {
    /// The ports a generated harness drives, in declaration order.
    pub fn inputs(&self) -> impl Iterator<Item = &Interface> {
        self.ports.iter().filter(|p| p.mode().is_driven())
    }

    /// The ports a generated harness reads.
    pub fn outputs(&self) -> impl Iterator<Item = &Interface> {
        self.ports.iter().filter(|p| !p.mode().is_driven())
    }

    /// The clock port, if the entity has something that looks like one.
    pub fn clock(&self) -> Option<&Interface> {
        self.ports.iter().find(|p| p.is_clock())
    }

    /// The reset port, if the entity has something that looks like one.
    pub fn reset(&self) -> Option<&Interface> {
        self.ports.iter().find(|p| p.is_reset())
    }
}

// ===========================================================================
// Record types
// ===========================================================================

/// One element of a VHDL record.
#[derive(Clone, Debug)]
pub struct RecordField {
    pub name: String,
    /// The element's subtype, rendered back to VHDL.
    pub subtype: String,
}

/// What the design's own type declarations say, to the extent a driver has to
/// know.
///
/// Two things it cannot work out from a port alone:
///
/// - **What is in a record.** A record port is reached over VHPI one element
///   at a time — the aggregate itself is not a value — so a driver needs the
///   element list to reach any of it.
/// - **What a subtype really is.** `signal x : Pam4Symbol` says nothing about
///   whether that is one bit or twenty, and the answer decides how it is
///   driven.
#[derive(Clone, Debug, Default)]
pub struct DesignTypes {
    /// Record declarations, keyed by lowercased type name.
    pub records: HashMap<String, Vec<RecordField>>,
    /// Subtype declarations, keyed by lowercased name, each holding the type
    /// mark it was declared from.
    pub subtypes: HashMap<String, String>,
    /// Array type names, keyed by lowercased name. Not usable as a whole from
    /// a driver, and tracked so that saying so is possible.
    pub arrays: HashSet<String>,
}

impl DesignTypes {
    /// Follow subtype declarations to the type mark underneath.
    ///
    /// `subtype Nibble is std_logic_vector(3 downto 0)` makes a port declared
    /// `Nibble` a vector, and one declared as a subtype of a subtype is no
    /// different. The hop limit is a guard against a source that declares a
    /// cycle, which will not elaborate but should not hang this either.
    pub fn resolve(&self, subtype: &str) -> String {
        // Takes a whole subtype indication, not just a mark: a caller that
        // had to strip the constraint first would eventually forget to.
        let mut mark = base_type_mark(subtype);
        for _ in 0..8 {
            let Some(next) = self.subtypes.get(&mark) else {
                break;
            };
            let next = base_type_mark(next);
            if next == mark {
                break;
            }
            mark = next;
        }
        mark
    }

    /// The elements of `subtype`, if it resolves to a record.
    pub fn record(&self, subtype: &str) -> Option<&Vec<RecordField>> {
        self.records.get(&self.resolve(subtype))
    }

    /// How a driver can talk to a signal of this subtype.
    pub fn reach(&self, subtype: &str) -> Reach {
        let mark = self.resolve(subtype);
        match mark.as_str() {
            "std_logic" | "std_ulogic" | "bit" | "boolean"
            | "std_logic_vector" | "std_ulogic_vector" | "bit_vector"
            | "unsigned" | "signed" => Reach::Bits,
            "integer" | "natural" | "positive" => Reach::Integer,
            _ if self.records.contains_key(&mark) => Reach::Record,
            _ => Reach::Unknown,
        }
    }
}

/// How a driver can talk to one signal.
///
/// Everything here was settled by running it against nvc rather than read
/// anywhere, because the answers are not what the VHDL types suggest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// A bit string: a logic scalar, a logic vector, or any subtype of one.
    /// Everything written to it is sized from the signal itself, so the
    /// declared width — even one that comes from a generic — never has to be
    /// known ahead of time.
    Bits,
    /// An `integer`/`natural`/`positive`. Writable, but it reads back as an
    /// empty bit string, so a check cannot compare against one.
    Integer,
    /// A record. Not a value in itself — reading the aggregate handle crashes
    /// the GPI layer — but each of its elements is.
    Record,
    /// Something with no meaningful single bit string: an array of
    /// composites, an enumeration, or a type that could not be found.
    Unknown,
}

/// A subtype indication's type mark, lowercased and unqualified.
pub fn base_type_mark(subtype: &str) -> String {
    let mark = subtype.split('(').next().unwrap_or(subtype).trim();
    // A resolution function precedes the mark: `resolved std_logic_vector`.
    let mark = mark.rsplit(char::is_whitespace).next().unwrap_or(mark);
    mark.rsplit('.').next().unwrap_or(mark).to_lowercase()
}

/// Collect the type declarations in the workspace's design sources.
///
/// `defaultlib` only: a design entity's ports are typed by the design's own
/// packages in practice, and parsing every dependency's VHDL to catch the
/// exception would cost far more than it returns. A type that is not found
/// is reported as such rather than guessed at.
pub fn design_types(
    workspace_dir: &Utf8Path,
    vhdl_std: VhdlStandard,
) -> Result<DesignTypes> {
    let config = crate::render_vhdl_ls_config(workspace_dir, None, false)?;
    let files = config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    let parser = VHDLParser::new(vhdl_std.into());
    let mut types = DesignTypes::default();
    for file in &files {
        let absolute = if file.is_relative() {
            workspace_dir.as_std_path().join(file)
        } else {
            file.clone()
        };
        if !absolute.exists() {
            continue;
        }
        // A source that does not parse contributes nothing rather than
        // failing the command: the design is often mid-edit when a testbench
        // for it is wanted.
        let mut diagnostics = Vec::new();
        let Ok((_, design_file)) =
            parser.parse_design_file(&absolute, &mut diagnostics)
        else {
            continue;
        };
        let mut collector = TypeCollector { types: &mut types };
        walk_design_file(&mut collector, &design_file);
    }
    Ok(types)
}

struct TypeCollector<'a> {
    types: &'a mut DesignTypes,
}

impl Visitor for TypeCollector<'_> {
    fn visit_type_declaration(
        &mut self,
        decl: &TypeDeclaration,
        _unit: &AnyDesignUnit,
    ) -> VisitorResult {
        let name = decl.ident.tree.item.name_utf8().to_lowercase();
        match &decl.def {
            TypeDefinition::Record(elements) => {
                let mut fields = Vec::new();
                for element in elements {
                    let subtype = element.subtype.to_string();
                    // `a, b : std_logic;` declares two elements, and a driver
                    // has to reach both.
                    for ident in &element.idents {
                        fields.push(RecordField {
                            name: ident.tree.item.name_utf8(),
                            subtype: subtype.clone(),
                        });
                    }
                }
                self.types.records.insert(name, fields);
            }
            TypeDefinition::Subtype(indication) => {
                self.types.subtypes.insert(name, indication.to_string());
            }
            TypeDefinition::Array(..) => {
                self.types.arrays.insert(name);
            }
            _ => {}
        }
        VisitorResult::Continue
    }
}

// ===========================================================================
// Instances
// ===========================================================================

/// Something instantiated inside a design, and what its ports are.
///
/// Named for what it is used for: a driver that implements this instance in
/// Rust needs the label the simulator knows it by, and the interface it has
/// to present.
#[derive(Clone, Debug)]
pub struct Instance {
    /// The instantiation label, as written.
    pub label: String,
    /// The interface of whatever is instantiated there.
    pub interface: EntityInterface,
}

/// Find the instance labelled `label` in `dut`'s architecture.
///
/// Only labels directly inside the entity's own architecture: an instance
/// further down would need a path rather than a name, and nothing has wanted
/// one yet.
pub fn find_instance(
    workspace_dir: &Utf8Path,
    dut: &EntityInterface,
    label: &str,
    vhdl_std: VhdlStandard,
) -> Result<Instance> {
    let parser = VHDLParser::new(vhdl_std.into());
    let mut diagnostics = Vec::new();
    let (_, design_file) =
        parser.parse_design_file(&dut.file, &mut diagnostics)?;

    // The architecture of the entity in question, which is normally in the
    // same file it is.
    let architecture = design_file
        .design_units
        .iter()
        .find_map(|(_, unit)| match unit {
            AnyDesignUnit::Secondary(AnySecondaryUnit::Architecture(arch))
                if arch
                    .entity_name
                    .item
                    .item
                    .name_utf8()
                    .eq_ignore_ascii_case(&dut.name) =>
            {
                Some(arch)
            }
            _ => None,
        })
        .ok_or_else(|| VwError::Config {
            message: format!(
                "no architecture for '{}' in {} — an instance can only be \
                 found in the architecture that declares it",
                dut.name,
                dut.file.display(),
            ),
        })?;

    let instantiated = architecture
        .statements
        .iter()
        .find_map(|statement| {
            let name = statement.label.tree.as_ref()?;
            if !name.item.name_utf8().eq_ignore_ascii_case(label) {
                return None;
            }
            match &statement.statement.item {
                ConcurrentStatement::Instance(instance) => Some(&instance.unit),
                _ => None,
            }
        })
        .ok_or_else(|| {
            let labels: Vec<String> = architecture
                .statements
                .iter()
                .filter(|s| {
                    matches!(s.statement.item, ConcurrentStatement::Instance(_))
                })
                .filter_map(|s| Some(s.label.tree.as_ref()?.item.name_utf8()))
                .collect();
            VwError::Config {
                message: format!(
                    "no instance labelled '{label}' in {}. It instantiates: {}",
                    dut.name,
                    if labels.is_empty() {
                        "nothing".to_string()
                    } else {
                        labels.join(", ")
                    },
                ),
            }
        })?;

    let (unit_name, is_component) = match instantiated {
        InstantiatedUnit::Entity(name, _) => (&name.item, false),
        InstantiatedUnit::Component(name) => (&name.item, true),
        InstantiatedUnit::Configuration(name) => (&name.item, false),
    };
    let unit_name =
        vw_core::mapping::type_mark_name(unit_name).ok_or_else(|| {
            VwError::Config {
                message: format!("could not read what '{label}' instantiates"),
            }
        })?;

    // An entity first, wherever it lives — an IP wrapper is in the `ip`
    // library, not the design's own. Failing that, a component declaration,
    // which is all there is for a black box with no entity behind it.
    let interface = match find_entity_anywhere(workspace_dir, &unit_name) {
        Ok(file) => read_entity(&file, &unit_name, vhdl_std)?,
        Err(e) if is_component => {
            component_interface(&design_file, &unit_name, &dut.file).ok_or(e)?
        }
        Err(e) => return Err(e),
    };

    Ok(Instance {
        label: label.to_string(),
        interface,
    })
}

/// A component declaration's ports, for a black box with no entity.
fn component_interface(
    design_file: &vhdl_lang::ast::DesignFile,
    name: &str,
    file: &Path,
) -> Option<EntityInterface> {
    use vhdl_lang::ast::Declaration;

    for (_, unit) in &design_file.design_units {
        let AnyDesignUnit::Secondary(AnySecondaryUnit::Architecture(arch)) =
            unit
        else {
            continue;
        };
        for declaration in &arch.decl {
            let Declaration::Component(component) = &declaration.item else {
                continue;
            };
            if !component
                .ident
                .tree
                .item
                .name_utf8()
                .eq_ignore_ascii_case(name)
            {
                continue;
            }
            return Some(EntityInterface {
                name: component.ident.tree.item.name_utf8(),
                file: file.to_path_buf(),
                context: Vec::new(),
                generics: interfaces(component.generic_list.as_ref()),
                ports: interfaces(component.port_list.as_ref()),
            });
        }
    }
    None
}

/// The source declaring `entity_name`, in any library the workspace can see.
///
/// Unlike [`find_design_entity`] this looks beyond `defaultlib`: an IP
/// wrapper vivado generated lives in the `ip` library, and a design that
/// instantiates one is entitled to have it found.
pub fn find_entity_anywhere(
    workspace_dir: &Utf8Path,
    entity_name: &str,
) -> Result<PathBuf> {
    let config = crate::render_vhdl_ls_config(workspace_dir, None, false)?;
    // The workspace's own sources first: a name that appears in both is the
    // design's, not a dependency's.
    let mut libraries: Vec<&String> = config.libraries.keys().collect();
    libraries.sort_by_key(|name| name.as_str() != "defaultlib");

    for library in libraries {
        let files = &config.libraries[library].files;
        for file in files {
            let absolute = if file.is_relative() {
                workspace_dir.as_std_path().join(file)
            } else {
                file.clone()
            };
            let Ok(contents) = std::fs::read_to_string(&absolute) else {
                continue;
            };
            if vw_core::parse_entities(&contents)?
                .iter()
                .any(|e| e.eq_ignore_ascii_case(entity_name))
            {
                return Ok(absolute);
            }
        }
    }

    Err(VwError::Config {
        message: format!(
            "no entity '{entity_name}' in the workspace or any library it \
             uses"
        ),
    })
}

/// Read `entity_name`'s interface out of the workspace's design sources.
pub fn load(
    workspace_dir: &Utf8Path,
    entity_name: &str,
    vhdl_std: VhdlStandard,
) -> Result<EntityInterface> {
    let file = find_design_entity(workspace_dir, entity_name)?;
    read_entity(&file, entity_name, vhdl_std)
}

/// The design source declaring `entity_name`.
///
/// Only `defaultlib` — the workspace's own `hdl/**` — is searched. A
/// testbench for something a dependency ships is a testbench for that
/// dependency, and wiring one here would generate a harness against sources
/// this workspace does not own.
pub fn find_design_entity(
    workspace_dir: &Utf8Path,
    entity_name: &str,
) -> Result<PathBuf> {
    let config = crate::render_vhdl_ls_config(workspace_dir, None, false)?;
    let files = config
        .libraries
        .get("defaultlib")
        .map(|lib| lib.files.clone())
        .unwrap_or_default();

    for file in &files {
        let absolute = if file.is_relative() {
            workspace_dir.as_std_path().join(file)
        } else {
            file.clone()
        };
        let Ok(contents) = std::fs::read_to_string(&absolute) else {
            continue;
        };
        if vw_core::parse_entities(&contents)?
            .iter()
            .any(|e| e.eq_ignore_ascii_case(entity_name))
        {
            return Ok(absolute);
        }
    }

    Err(VwError::Config {
        message: format!(
            "no entity '{entity_name}' in the workspace's design sources \
             (searched {} file{} under hdl/)",
            files.len(),
            if files.len() == 1 { "" } else { "s" },
        ),
    })
}

/// Parse `file` and pull out `entity_name`'s interface.
pub fn read_entity(
    file: &Path,
    entity_name: &str,
    vhdl_std: VhdlStandard,
) -> Result<EntityInterface> {
    let parser = VHDLParser::new(vhdl_std.into());
    // Diagnostics are collected but not reported: a design that does not yet
    // analyze cleanly still has a readable port list, and refusing to
    // scaffold against it would make `--dut` useless exactly when a
    // testbench is most wanted.
    let mut diagnostics = Vec::new();
    let (_, design_file) = parser.parse_design_file(file, &mut diagnostics)?;

    let entity = design_file
        .design_units
        .iter()
        .find_map(|(_, unit)| match unit {
            AnyDesignUnit::Primary(AnyPrimaryUnit::Entity(entity))
                if entity
                    .ident
                    .tree
                    .item
                    .name_utf8()
                    .eq_ignore_ascii_case(entity_name) =>
            {
                Some(entity)
            }
            _ => None,
        })
        .ok_or_else(|| VwError::Config {
            message: format!(
                "'{entity_name}' is not declared in {}",
                file.display()
            ),
        })?;

    Ok(EntityInterface {
        name: entity.ident.tree.item.name_utf8(),
        file: file.to_path_buf(),
        context: context_clauses(file, entity_name),
        generics: interfaces(entity.generic_clause.as_ref()),
        ports: interfaces(entity.port_clause.as_ref()),
    })
}

/// Flatten an interface list into one [`Interface`] per name.
///
/// `a, b : in std_logic` is one declaration of two ports, and a harness has
/// to wire both.
fn interfaces(list: Option<&InterfaceList>) -> Vec<Interface> {
    let Some(list) = list else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in &list.items {
        let InterfaceDeclaration::Object(object) = item else {
            // Interface types, subprograms and packages are not things a
            // generated harness can wire, and an entity that has them is
            // rare enough to hand back to the developer.
            continue;
        };
        let ModeIndication::Simple(simple) = &object.mode else {
            // A mode view — `port (p : view bar)`. The view names a record,
            // so this lands in the same place a record port would.
            continue;
        };
        let mode = simple.mode.as_ref().map(|m| match m.item {
            Mode::In => PortMode::In,
            Mode::Out => PortMode::Out,
            Mode::InOut => PortMode::InOut,
            // `linkage` is vestigial; nothing generated here can use it, and
            // treating it as a buffer at least keeps the port visible.
            Mode::Buffer | Mode::Linkage => PortMode::Buffer,
        });
        let subtype = simple.subtype_indication.to_string();
        let default = simple.expression.as_ref().map(|e| e.to_string());

        for ident in &object.idents {
            out.push(Interface {
                name: ident.tree.item.name_utf8(),
                mode,
                subtype: subtype.clone(),
                default: default.clone(),
            });
        }
    }
    out
}

/// The `library` / `use` lines a file opens with, up to the entity.
///
/// Taken from the text rather than the AST because what a harness wants is
/// the clause as the developer wrote it — `use work.cpm5.all;` — and the
/// parsed form would have to be rendered back into that anyway.
fn context_clauses(file: &Path, entity_name: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };

    // Everything before the entity declaration. A file holding a package and
    // then an entity has two context clauses, and the entity's is the second
    // — but the package's names types the entity may well expose, so taking
    // both is right more often than taking the last.
    let cutoff = regex::RegexBuilder::new(&format!(
        r"\bentity\s+{}\s+is\b",
        regex::escape(entity_name)
    ))
    .case_insensitive(true)
    .build()
    .ok()
    .and_then(|re| re.find(&text).map(|m| m.start()))
    .unwrap_or(text.len());

    let mut seen = HashSet::new();
    let mut clauses = Vec::new();
    for line in text[..cutoff].lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if !(lower.starts_with("library ") || lower.starts_with("use ")) {
            continue;
        }
        if seen.insert(lower) {
            clauses.push(trimmed.to_string());
        }
    }
    clauses
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "\
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

package noc_pkg is
  subtype Nibble is std_logic_vector(3 downto 0);
  type lrq is record
    valid, ready : std_logic;
    data         : Nibble;
  end record;
end package;

library ieee;
use ieee.std_logic_1164.all;
use work.noc_pkg.all;

entity flit_fifo is
  generic (
    DATA_W    : positive := 32;
    FAST_READ : boolean
  );
  port (
    clk         : in  std_logic;
    rst_n       : in  std_logic;
    write_data  : in  std_logic_vector(DATA_W - 1 downto 0);
    read_data   : out std_logic_vector(DATA_W - 1 downto 0);
    tap         : in  Nibble;
    level       : out natural;
    side_bus    : in  lrq;
    full, empty : out std_logic
  );
end entity flit_fifo;
";

    fn parsed() -> (tempfile::TempDir, EntityInterface) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flit_fifo.vhd");
        std::fs::write(&file, SOURCE).unwrap();
        let entity =
            read_entity(&file, "flit_fifo", VhdlStandard::Vhdl2019).unwrap();
        (dir, entity)
    }

    /// The whole interface, in declaration order, with types rendered back to
    /// what a generated harness has to write out again.
    #[test]
    fn ports_come_back_with_their_types_intact() {
        let (_guard, entity) = parsed();

        assert_eq!(entity.name, "flit_fifo");
        let names: Vec<&str> =
            entity.ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "clk",
                "rst_n",
                "write_data",
                "read_data",
                "tap",
                "level",
                "side_bus",
                "full",
                "empty",
            ],
            "a declaration naming two ports is two ports",
        );

        let write_data = &entity.ports[2];
        assert_eq!(write_data.mode(), PortMode::In);
        assert_eq!(
            write_data.subtype, "std_logic_vector(DATA_W - 1 downto 0)",
            "the constraint's generic reference has to survive — the harness \
             declares the same generic and relies on it",
        );
    }

    /// How a driver can talk to each port — the distinction the whole
    /// generator turns on, and one the port alone does not answer.
    #[test]
    fn a_ports_reach_is_resolved_through_the_designs_types() {
        let (_guard, entity) = parsed();
        let types = design_types_of(&entity.file);
        let reach = |name: &str| {
            types.reach(
                &entity
                    .ports
                    .iter()
                    .find(|p| p.name == name)
                    .expect(name)
                    .subtype,
            )
        };

        assert_eq!(reach("clk"), Reach::Bits);
        assert_eq!(reach("write_data"), Reach::Bits);
        assert_eq!(
            reach("tap"),
            Reach::Bits,
            "a subtype of std_logic_vector is still a bit string",
        );
        assert_eq!(reach("level"), Reach::Integer);
        assert_eq!(reach("side_bus"), Reach::Record);
    }

    /// The elements of a record port, which is the only way to reach one.
    #[test]
    fn record_types_come_back_with_their_elements() {
        let (_guard, entity) = parsed();
        let types = design_types_of(&entity.file);

        let fields = types.record("lrq").expect("lrq is a record");
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["valid", "ready", "data"],
            "a declaration naming two elements is two elements",
        );
        assert_eq!(fields[2].subtype, "Nibble");
    }

    /// Parse the one file directly, since `design_types` walks a workspace.
    fn design_types_of(file: &std::path::Path) -> DesignTypes {
        let parser = VHDLParser::new(VhdlStandard::Vhdl2019.into());
        let mut diagnostics = Vec::new();
        let (_, design_file) =
            parser.parse_design_file(file, &mut diagnostics).unwrap();
        let mut types = DesignTypes::default();
        let mut collector = TypeCollector { types: &mut types };
        walk_design_file(&mut collector, &design_file);
        types
    }

    /// Generics come through with their defaults, and the absence of one is
    /// visible rather than invented.
    #[test]
    fn generics_keep_their_defaults() {
        let (_guard, entity) = parsed();
        assert_eq!(entity.generics.len(), 2);
        assert_eq!(entity.generics[0].name, "DATA_W");
        assert_eq!(entity.generics[0].default.as_deref(), Some("32"));
        assert_eq!(entity.generics[1].name, "FAST_READ");
        assert_eq!(entity.generics[1].default, None);
    }

    /// The clock and reset are what decide where the clock process and the
    /// reset sequence go, so they have to be found by name.
    #[test]
    fn the_clock_and_reset_are_recognized() {
        let (_guard, entity) = parsed();
        assert_eq!(entity.clock().map(|p| p.name.as_str()), Some("clk"));

        let reset = entity.reset().expect("reset");
        assert_eq!(reset.name, "rst_n");
        assert!(
            reset.is_active_low_reset(),
            "a bench that gets this backwards never leaves reset",
        );
    }

    /// A generated harness declares signals with the DUT's types, so it needs
    /// the clauses that make those types visible — including the ones in
    /// front of a package earlier in the same file.
    #[test]
    fn context_clauses_are_collected_up_to_the_entity() {
        let (_guard, entity) = parsed();
        assert!(entity
            .context
            .contains(&"use work.noc_pkg.all;".to_string()));
        assert!(entity
            .context
            .contains(&"use ieee.numeric_std.all;".to_string()));
        assert_eq!(
            entity
                .context
                .iter()
                .filter(|c| c.eq_ignore_ascii_case("library ieee;"))
                .count(),
            1,
            "the same clause twice would be repeated in every generated file",
        );
    }

    /// An entity that is not in the file is an error, not an empty interface
    /// that would silently generate a harness wired to nothing.
    #[test]
    fn a_missing_entity_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("flit_fifo.vhd");
        std::fs::write(&file, SOURCE).unwrap();

        assert!(read_entity(&file, "not_here", VhdlStandard::Vhdl2019).is_err());
    }
}
