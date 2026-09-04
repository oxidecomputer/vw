// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw bench init` — a pure VHDL testbench.
//!
//! One file, `bench/<name>_tb.vhd`, holding an entity with no ports and an
//! architecture that drives it. That is the whole shape: `vw bench run`
//! discovers a bench by finding an entity whose name ends in `_tb`, so
//! nothing else has to be registered anywhere.
//!
//! With `--dut` the architecture comes out wired to a design entity — a
//! signal per port carrying that port's own type, the generic map, the port
//! map. Without it, the same architecture arrives with the instantiation left
//! as a comment. Either way the result runs and passes the moment it is
//! written, so a developer finds out the plumbing works before writing the
//! first check rather than after.

use camino::Utf8Path;

use super::entity::{DesignTypes, EntityInterface, Interface};
use super::{write_new_file, Created};
use crate::{Result, VhdlStandard};

/// Write a pure VHDL testbench for `name`.
///
/// `name` is the bench's base name; the entity is always `<name>_tb`, which
/// is what makes it discoverable.
pub fn init(
    workspace_dir: &Utf8Path,
    name: &str,
    dut: Option<&str>,
    vhdl_std: VhdlStandard,
) -> Result<Created> {
    let base = super::base_name(name);
    super::check_vhdl_identifier(&base)?;
    let entity = format!("{base}_tb");

    let dut = dut
        .map(|d| super::entity::load(workspace_dir, d, vhdl_std))
        .transpose()?;
    // A signal declared with the DUT's own subtype still needs a starting
    // value, and what that looks like depends on what the subtype resolves
    // to — `Nibble` takes `(others => '0')`, not `'0'`.
    let types = match &dut {
        Some(_) => super::entity::design_types(workspace_dir, vhdl_std)?,
        None => DesignTypes::default(),
    };

    let path = workspace_dir.join("bench").join(format!("{entity}.vhd"));
    std::fs::create_dir_all(
        path.parent()
            .expect("bench path has a parent")
            .as_std_path(),
    )?;
    write_new_file(&path, &render(&entity, dut.as_ref(), &types))?;

    Ok(Created {
        name: entity,
        files: vec![path],
        registered: false,
        updated: false,
        next_steps: next_steps(&base, dut.as_ref()),
    })
}

fn next_steps(base: &str, dut: Option<&EntityInterface>) -> Vec<String> {
    let mut steps = Vec::new();
    match dut {
        Some(dut) => steps.push(format!(
            "write the checks in the `stimulus` process — {} is already \
             instantiated and its ports are signals of the same name",
            dut.name,
        )),
        None => steps.push(
            "instantiate the design under test where the file says to, or \
             re-run with `--dut <entity>` to have it wired for you"
                .to_string(),
        ),
    }
    steps.push(format!("run it with `vw bench run {base}`"));
    steps
}

/// Where a generated testbench keeps its own bookkeeping signal.
///
/// Named distinctively because every other signal in the architecture is
/// named after one of the DUT's ports, and a design with a port called `done`
/// is not far-fetched.
const DONE: &str = "tb_done";

fn render(
    entity: &str,
    dut: Option<&EntityInterface>,
    types: &DesignTypes,
) -> String {
    let mut out = String::new();

    for clause in super::vhdl_context(dut) {
        out.push_str(&clause);
        out.push('\n');
    }

    out.push_str(&format!(
        "\n\
         entity {entity} is\n\
         end entity;\n\
         \n\
         architecture sim of {entity} is\n\
         \n  \
         constant CLK_PERIOD : time := 10 ns;\n\n",
    ));

    let clock = dut.and_then(EntityInterface::clock);
    let reset = dut.and_then(EntityInterface::reset);
    let clock_name = clock
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "clk".to_string());

    match dut {
        Some(dut) => {
            out.push_str(&generics_block(dut, types));
            out.push_str(&signals_block(dut, types, &clock_name));
        }
        None => {
            let width = clock_name.len().max("rstn".len());
            out.push_str(&format!(
                "  signal {clock_name:width$} : std_logic := '0';\n  \
                 signal {:width$} : std_logic := '0';\n\n",
                "rstn",
            ));
        }
    }

    out.push_str(&format!(
        "  -- Set once the stimulus is finished, which stops the clock and \
         lets the\n  -- simulation run out of events on its own rather than \
         being killed.\n  \
         signal {DONE} : boolean := false;\n\
         \n\
         begin\n\
         \n  \
         {clock_name} <= not {clock_name} after CLK_PERIOD / 2 when not \
         {DONE};\n\n",
    ));

    match dut {
        Some(dut) => out.push_str(&instantiation(dut)),
        None => out.push_str(
            "  -- TODO: instantiate the design under test.\n  \
             --\n  \
             --   dut: entity work.<entity>\n  \
             --     port map (\n  \
             --       clk => clk,\n  \
             --       rstn => rstn\n  \
             --     );\n  \
             --\n  \
             -- `vw bench init <name> --dut <entity>` writes this part for \
             you.\n\n",
        ),
    }

    out.push_str(&stimulus(entity, reset, &clock_name));
    out.push_str("\nend architecture;\n");
    out
}

/// One constant per DUT generic, so the generic map below reads as names
/// rather than magic numbers and a sweep is one edit.
fn generics_block(dut: &EntityInterface, types: &DesignTypes) -> String {
    if dut.generics.is_empty() {
        return String::new();
    }

    let width = dut.generics.iter().map(|g| g.name.len()).max().unwrap_or(0);
    let mut out = format!("  -- {}'s generics.\n", dut.name);
    for generic in &dut.generics {
        match generic
            .default
            .clone()
            .or_else(|| super::placeholder(generic, types))
        {
            Some(value) => out.push_str(&format!(
                "  constant {:width$} : {} := {value};\n",
                generic.name, generic.subtype,
            )),
            None => out.push_str(&format!(
                "  -- TODO: {} has no default; give it a value.\n  \
                 -- constant {:width$} : {} := ;\n",
                generic.name, generic.name, generic.subtype,
            )),
        }
    }
    out.push('\n');
    out
}

/// One signal per DUT port, carrying the port's own type.
fn signals_block(
    dut: &EntityInterface,
    types: &DesignTypes,
    clock_name: &str,
) -> String {
    if dut.ports.is_empty() {
        return String::new();
    }

    let width = dut.ports.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let mut out = format!("  -- {}'s ports.\n", dut.name);
    for port in &dut.ports {
        // Inputs start at a defined value so the design is not driven with
        // metavalues before the stimulus process gets to it; outputs are
        // driven by the DUT and must not be initialized here.
        let init = if port.name == clock_name {
            Some("'0'".to_string())
        } else if port.mode().is_driven() {
            initial_value(port, types)
        } else {
            None
        };
        match init {
            Some(value) => out.push_str(&format!(
                "  signal {:width$} : {} := {value};\n",
                port.name, port.subtype,
            )),
            None => out.push_str(&format!(
                "  signal {:width$} : {};\n",
                port.name, port.subtype,
            )),
        }
    }
    out.push('\n');
    out
}

fn instantiation(dut: &EntityInterface) -> String {
    let mut out = format!("  dut: entity work.{}\n", dut.name);

    if !dut.generics.is_empty() {
        let width =
            dut.generics.iter().map(|g| g.name.len()).max().unwrap_or(0);
        out.push_str("    generic map (\n");
        let mapped: Vec<String> = dut
            .generics
            .iter()
            .map(|g| format!("      {:width$} => {}", g.name, g.name))
            .collect();
        out.push_str(&mapped.join(",\n"));
        out.push_str("\n    )\n");
    }

    if dut.ports.is_empty() {
        out.push_str("    ;\n\n");
        return out;
    }

    let width = dut.ports.iter().map(|p| p.name.len()).max().unwrap_or(0);
    out.push_str("    port map (\n");
    let mapped: Vec<String> = dut
        .ports
        .iter()
        .map(|p| format!("      {:width$} => {}", p.name, p.name))
        .collect();
    out.push_str(&mapped.join(",\n"));
    out.push_str("\n    );\n\n");
    out
}

fn stimulus(
    entity: &str,
    reset: Option<&Interface>,
    clock_name: &str,
) -> String {
    let mut body = String::new();

    if let Some(reset) = reset {
        let (asserted, released) = if reset.is_active_low_reset() {
            ("'0'", "'1'")
        } else {
            ("'1'", "'0'")
        };
        body.push_str(&format!(
            "    {} <= {asserted};\n    \
             wait for CLK_PERIOD * 4;\n    \
             {} <= {released};\n    \
             wait until rising_edge({clock_name});\n\n",
            reset.name, reset.name,
        ));
    } else {
        body.push_str(&format!(
            "    wait for CLK_PERIOD * 4;\n    \
             wait until rising_edge({clock_name});\n\n",
        ));
    }

    format!(
        "  stimulus: process\n  \
         begin\n\
         {body}    \
         -- TODO: drive the design and check what it does. An `assert` that \
         fails\n    \
         -- with severity error or above fails the bench; `report` on its own \
         does\n    \
         -- not.\n\n    \
         report \"{entity} complete\" severity note;\n\n    \
         {DONE} <= true;\n    \
         std.env.finish;\n    \
         wait;\n  \
         end process;\n",
    )
}

/// A safe starting value for a signal of this type, if there is an obvious
/// one.
///
/// Resolved through the design's subtype declarations first: `Nibble` is a
/// vector and takes `(others => '0')`, and nothing about the name says so.
fn initial_value(port: &Interface, types: &DesignTypes) -> Option<String> {
    super::zero_literal(&port.subtype, types)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench_init::entity::{EntityInterface, PortMode};

    fn types() -> DesignTypes {
        let mut types = DesignTypes::default();
        types
            .subtypes
            .insert("nibble".into(), "std_logic_vector(3 downto 0)".into());
        types.records.insert(
            "ctl".into(),
            vec![
                crate::bench_init::entity::RecordField {
                    name: "go".into(),
                    subtype: "std_logic".into(),
                },
                crate::bench_init::entity::RecordField {
                    name: "step".into(),
                    subtype: "Nibble".into(),
                },
            ],
        );
        types
    }

    fn port(name: &str, mode: PortMode, subtype: &str) -> Interface {
        Interface {
            name: name.to_string(),
            mode: Some(mode),
            subtype: subtype.to_string(),
            default: None,
        }
    }

    fn dut() -> EntityInterface {
        EntityInterface {
            name: "counter".to_string(),
            file: std::path::PathBuf::from("hdl/counter.vhd"),
            context: vec![
                "library ieee;".to_string(),
                "use ieee.std_logic_1164.all;".to_string(),
            ],
            generics: vec![Interface {
                name: "WIDTH".to_string(),
                mode: None,
                subtype: "positive".to_string(),
                default: Some("8".to_string()),
            }],
            ports: vec![
                port("clk", PortMode::In, "std_logic"),
                port("rst_n", PortMode::In, "std_logic"),
                port("enable", PortMode::In, "std_logic"),
                port("count", PortMode::Out, "unsigned(WIDTH - 1 downto 0)"),
                port("tap", PortMode::In, "Nibble"),
                port("bus_in", PortMode::In, "ctl"),
            ],
        }
    }

    /// Nothing generated may leave an input floating: a design driven with
    /// `'U'` floods numeric_std with metavalue warnings and the bench's
    /// output becomes unreadable before the first check runs.
    #[test]
    fn every_driven_port_gets_an_initial_value() {
        let text = render("counter_tb", Some(&dut()), &types());
        assert!(text.contains("signal enable : std_logic := '0';"));
        assert!(text.contains("signal rst_n  : std_logic := '0';"));
        // The DUT drives its own outputs — initializing one here would fight it.
        assert!(text.contains("unsigned(WIDTH - 1 downto 0);"));
    }

    /// A subtype resolves to what it really is, and a record gets a named
    /// aggregate — `(others => …)` cannot zero one whose elements differ.
    /// Left uninitialized, either reaches the design as all-'U' and floods
    /// the run with metavalue warnings before a check can fail.
    #[test]
    fn subtype_and_record_inputs_start_at_a_defined_value() {
        let text = render("counter_tb", Some(&dut()), &types());
        assert!(text.contains("signal tap    : Nibble := (others => '0');"));
        assert!(text.contains(
            "signal bus_in : ctl := (go => '0', step => (others => '0'));"
        ));
    }

    /// The generic map has to name every generic, or the instantiation takes
    /// defaults that the constants above it claim to control.
    #[test]
    fn generics_become_constants_and_are_mapped() {
        let text = render("counter_tb", Some(&dut()), &types());
        assert!(text.contains("constant WIDTH : positive := 8;"));
        assert!(text.contains("WIDTH => WIDTH"));
    }

    /// An active-low reset is asserted low and released high. Getting this
    /// backwards produces a bench that never leaves reset and hangs.
    #[test]
    fn reset_polarity_follows_the_port_name() {
        let text = render("counter_tb", Some(&dut()), &types());
        let asserted = text.find("rst_n <= '0';").expect("asserted");
        let released = text.find("rst_n <= '1';").expect("released");
        assert!(asserted < released);
    }

    /// The entity must end in `_tb` and take no ports, which is what makes
    /// `vw bench run` able to find and elaborate it.
    #[test]
    fn the_entity_is_portless_and_discoverable() {
        let text = render("widget_tb", None, &types());
        assert!(text.contains("entity widget_tb is\nend entity;"));
        assert!(text.contains("std.env.finish;"));
    }

    /// Without a DUT the file still compiles and runs — it just has nothing
    /// to drive yet.
    #[test]
    fn a_bench_with_no_dut_still_stands_on_its_own() {
        let text = render("widget_tb", None, &types());
        assert!(text.contains("signal clk  : std_logic := '0';"));
        assert!(text.contains("signal rstn : std_logic := '0';"));
        assert!(text.contains("TODO: instantiate the design under test"));
        assert!(!text.contains("entity work.<entity>\n    port map"));
    }

    /// The DUT's own context clauses come through, because the signals are
    /// declared with the DUT's types.
    #[test]
    fn the_duts_context_clauses_are_carried_over() {
        let mut dut = dut();
        dut.context.push("use work.counter_pkg.all;".to_string());
        let text = render("counter_tb", Some(&dut), &types());
        assert!(text.contains("use work.counter_pkg.all;"));
        // numeric_std was not among them and is added.
        assert!(text.contains("use ieee.numeric_std.all;"));
        // ...but nothing is added twice.
        assert_eq!(text.matches("use ieee.std_logic_1164.all;").count(), 1);
    }
}
