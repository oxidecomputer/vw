// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Modular synthesis: the design synthesized in pieces and linked.
//!
//! In-context synthesis elaborates the user logic and every IP module in
//! one `synth_design`. Modular synthesis builds the same synth checkpoint
//! from three steps instead, each in a Vivado worker of its own:
//!
//! 1. IP. Every IP module out of context, in parallel, each cached on its
//!    configuration ([`crate::synth_ip`]).
//! 2. User logic. The workspace's VHDL alone, every IP a black box bound to
//!    its stub, written as an EDIF. This is the step a synthesizer other
//!    than Vivado's can stand in for.
//! 3. Link. That EDIF linked against the IP checkpoints, with the synth
//!    constraints and the scoped constraints XPM instances carry, written
//!    as the stage's checkpoint.
//!
//! None of it happens in the session's own Vivado, which keeps its project
//! open throughout and opens the result as it would a cached checkpoint.

use std::path::{Path, PathBuf};

use camino::Utf8Path;
use serde_json::{json, Value};
use vw_eda::StreamKind;

use crate::handlers::{
    design_phase_constraints, vhdl_dependency_sources, vhdl_design_sources,
    vhdl_ip_sources, ConstraintPhase,
};
use crate::job::{tcl_list, tcl_word, Job, Report};
use crate::rpc::RpcContext;
use crate::synth_ip::{Built, Kind};

/// The `synth_modular` RPC. Args: `{"top", "checkpoint", "vhdl": "2019" |
/// "2008", "jobs"}`. Writes the synth checkpoint to `checkpoint` and
/// answers `{"checkpoint", "edif"}`.
pub(crate) async fn synth_modular(
    workspace_root: Option<&Path>,
    active_variant: Option<&str>,
    args: Value,
    ctx: RpcContext,
) -> Result<Value, String> {
    let ws = workspace_root
        .and_then(Utf8Path::from_path)
        .ok_or("synth_modular: no workspace root")?;
    let arg = |name: &str| -> Result<String, String> {
        args.get(name)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| format!("synth_modular: missing `{name}`"))
    };
    let top = arg("top")?;
    let checkpoint = PathBuf::from(arg("checkpoint")?);
    let vhdl2019 = args.get("vhdl").and_then(Value::as_str) != Some("2008");

    let ip = crate::synth_ip::synthesize(
        ws,
        active_variant,
        crate::synth_ip::jobs_arg(&args),
        &ctx,
    )
    .await?;

    let dir = checkpoint
        .parent()
        .ok_or("synth_modular: checkpoint has no directory")?
        .join("modular");
    // `link_design -top` finds the netlist by its file name.
    let edif = dir.join(format!("{top}.edf"));
    let sources = Sources::of(ws, active_variant).await?;

    ctx.emit(
        StreamKind::Info,
        format!("INFO: [vw] synthesizing user logic (top={top})\n"),
    );
    let user = Job {
        label: format!("user-logic synthesis of {top}"),
        vivado: ip.vivado.clone(),
        dir: dir.join("user"),
        setup: Vec::new(),
        steps: user_tcl(&top, &ip.part, vhdl2019, &sources, &ip.modules, &edif),
        report: Report::All,
    };
    let seconds = run(&user, &ctx).await?;
    ctx.emit(
        StreamKind::Info,
        format!("INFO: [vw] user logic synthesized in {seconds:.0}s\n"),
    );

    let xpm = xpm_constraints(&ip.vivado)?;
    ctx.emit(
        StreamKind::Info,
        format!(
            "INFO: [vw] linking {top} against {} IP checkpoint(s)\n",
            ip.modules.len()
        ),
    );
    let link = Job {
        label: format!("link of {top}"),
        vivado: ip.vivado.clone(),
        dir: dir.join("link"),
        setup: Vec::new(),
        steps: link_tcl(
            &top,
            &ip.part,
            &edif,
            &ip.modules,
            &sources.synth_constraints,
            &xpm,
            &checkpoint,
        ),
        report: Report::All,
    };
    let seconds = run(&link, &ctx).await?;
    ctx.emit(
        StreamKind::Info,
        format!("INFO: [vw] linked in {seconds:.0}s\n"),
    );

    Ok(json!({
        "checkpoint": checkpoint.display().to_string(),
        "edif": edif.display().to_string(),
    }))
}

async fn run(job: &Job, ctx: &RpcContext) -> Result<f64, String> {
    job.run(ctx).await.map_err(|e| {
        format!(
            "synth_modular: {} failed: {e} (log: {})",
            job.label,
            job.log().display()
        )
    })
}

/// What the workspace gives synthesis: the same answers `vw::synth`'s own
/// RPCs give it, so both flows read the same files.
struct Sources {
    /// Dependency VHDL, by library.
    dependencies: Vec<(String, Vec<PathBuf>)>,
    /// `vw::make_wrapper` outputs, library `ip`.
    ip_wrappers: Vec<PathBuf>,
    /// The workspace's own VHDL, library `work`.
    design: Vec<PathBuf>,
    synth_constraints: Vec<PathBuf>,
}

impl Sources {
    async fn of(
        ws: &Utf8Path,
        active_variant: Option<&str>,
    ) -> Result<Self, String> {
        let root = Some(ws.as_std_path());
        let variant = active_variant.map(String::from);
        let paths = |v: Value| -> Vec<PathBuf> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        };
        let dependencies =
            match vhdl_dependency_sources(root, false, true).await? {
                Value::Object(by_lib) => by_lib
                    .into_iter()
                    .map(|(lib, files)| (lib, paths(files)))
                    .collect(),
                _ => Vec::new(),
            };
        Ok(Self {
            dependencies,
            ip_wrappers: paths(vhdl_ip_sources(root, variant.clone())?),
            design: paths(vhdl_design_sources(root, variant.clone())?),
            synth_constraints: paths(design_phase_constraints(
                root,
                ConstraintPhase::Synth,
                variant,
            )?),
        })
    }
}

/// Synthesize the user logic, every IP a black box, to `edif`.
fn user_tcl(
    top: &str,
    part: &str,
    vhdl2019: bool,
    sources: &Sources,
    ip: &[Built],
    edif: &Path,
) -> Vec<String> {
    let std = if vhdl2019 { " -vhdl2019" } else { "" };
    let mut tcl = vec![
        format!(
            "create_project -in_memory -part {part} vw_user\n\
             set_property TARGET_LANGUAGE VHDL [current_project]"
        ),
        // The black boxes are the point, and link — which fails on any it
        // can't fill — is where they're resolved. Not a critical warning
        // here.
        "set_msg_config -id {Project 1-486} -new_severity INFO".to_string(),
    ];
    for (lib, files) in &sources.dependencies {
        tcl.push(format!(
            "read_vhdl{std} -library {lib} [list {}]",
            tcl_list(files)
        ));
    }
    if !sources.ip_wrappers.is_empty() {
        tcl.push(format!(
            "read_vhdl{std} -library ip [list {}]",
            tcl_list(&sources.ip_wrappers)
        ));
    }
    // Every IP module binds to its stub, which carries `syn_black_box` and
    // the module's interface attributes (link checks those against the
    // checkpoint). Each goes in the one library its instances are bound
    // through: a BD's in `ip` with the wrapper that instantiates it, an
    // XCI's in `xil_defaultlib`, where the design instantiates it by
    // entity. A stub in both libraries makes its black box `ip_<name>`,
    // which no checkpoint matches.
    for m in ip {
        let lib = match m.kind {
            Kind::BlockDesign => "ip",
            Kind::Xci => "xil_defaultlib",
        };
        tcl.push(format!(
            "read_vhdl{std} -library {lib} {}",
            tcl_word(&m.stub)
        ));
    }
    if !sources.design.is_empty() {
        tcl.push(format!(
            "read_vhdl{std} [list {}]",
            tcl_list(&sources.design)
        ));
    }
    // No synth constraints here: they name clocks the IP creates, which
    // this synthesis can't see. Link applies them.
    tcl.push(format!("synth_design -top {top} -part {part}"));
    tcl.push(format!(
        "file mkdir {}\nwrite_edif -force {}",
        tcl_word(edif.parent().unwrap_or(Path::new("."))),
        tcl_word(edif)
    ));
    tcl
}

/// Link `edif` against the IP checkpoints, constrain it, and write the
/// stage's checkpoint.
fn link_tcl(
    top: &str,
    part: &str,
    edif: &Path,
    ip: &[Built],
    synth_constraints: &[PathBuf],
    xpm: &[(String, PathBuf)],
    checkpoint: &Path,
) -> Vec<String> {
    let mut tcl = vec![format!(
        "create_project -in_memory -part {part} vw_link\n\
         set_property TARGET_LANGUAGE VHDL [current_project]\n\
         read_edif {}",
        tcl_word(edif)
    )];
    let dcps: Vec<&Path> = ip.iter().map(|m| m.dcp.as_path()).collect();
    if !dcps.is_empty() {
        tcl.push(format!("add_files [list {}]", tcl_list(&dcps)));
    }
    if !synth_constraints.is_empty() {
        tcl.push(format!("read_xdc [list {}]", tcl_list(synth_constraints)));
    }
    // Link fills each black box from the checkpoint whose top module has
    // its name.
    tcl.push(format!("link_design -top {top} -part {part}"));
    tcl.push(
        "set bbs [get_cells -quiet -hierarchical -filter {IS_BLACKBOX == TRUE}]\n\
         if {[llength $bbs] > 0} {\n  \
           error \"black boxes no IP checkpoint filled: $bbs\"\n\
         }"
        .to_string(),
    );
    tcl.push(xpm_tcl(ip, xpm));
    tcl.push(format!(
        "file mkdir {}\nwrite_checkpoint -force {}",
        tcl_word(checkpoint.parent().unwrap_or(Path::new("."))),
        tcl_word(checkpoint)
    ));
    tcl
}

/// Apply XPM's scoped constraints to the user logic's XPM instances.
///
/// Synthesis applies a script per XPM instance — the CDC primitives'
/// `set_max_delay -datapath_only` and `set_bus_skew`, the async FIFOs'
/// LUTRAM read paths — and they live on in a synth checkpoint. An EDIF
/// carries none of them and `link_design` doesn't recreate them, so they're
/// applied here, per instance, the way synthesis would have. Instances
/// inside an IP are skipped: the IP's checkpoint already carries theirs.
/// Synthesis uniquifies XPM modules (`xpm_cdc_gray__7`) but keeps the
/// original name in `ORIG_REF_NAME`, which the EDIF carries.
fn xpm_tcl(ip: &[Built], xpm: &[(String, PathBuf)]) -> String {
    let modules = ip.iter().map(|m| m.name.as_str()).collect::<Vec<_>>();
    let scripts = xpm
        .iter()
        .map(|(module, script)| format!("  {module} {}", tcl_word(script)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "set ::vw_ip_cells {{}}\n\
         foreach m {{{modules}}} {{\n  \
           foreach c [get_cells -quiet -hierarchical -filter \"REF_NAME == $m\"] {{\n    \
             lappend ::vw_ip_cells \"$c/\"\n  \
           }}\n\
         }}\n\
         set applied 0\n\
         foreach {{module script}} {{\n{scripts}\n}} {{\n  \
           set cells {{}}\n  \
           foreach c [get_cells -quiet -hierarchical -filter \"ORIG_REF_NAME == $module || REF_NAME == $module\"] {{\n    \
             set inside 0\n    \
             foreach p $::vw_ip_cells {{\n      \
               if {{[string first $p $c] == 0}} {{ set inside 1; break }}\n    \
             }}\n    \
             if {{!$inside}} {{ lappend cells $c }}\n  \
           }}\n  \
           if {{[llength $cells] > 0}} {{\n    \
             read_xdc -unmanaged -cells $cells $script\n    \
             incr applied [llength $cells]\n  \
           }}\n\
         }}\n\
         puts \"INFO: \\[vw\\] applied XPM scoped constraints to $applied instance(s)\"",
        modules = modules.join(" "),
    )
}

/// Every XPM module with scoped constraints, and its script, from the
/// IP-XACT metadata of each library under the Vivado install's
/// `data/ip/xpm`.
fn xpm_constraints(vivado: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    // `<install>/bin/vivado`.
    let xpm = vivado
        .parent()
        .and_then(Path::parent)
        .map(|install| install.join("data/ip/xpm"))
        .ok_or_else(|| {
            format!("no Vivado install around {}", vivado.display())
        })?;
    let mut found = Vec::new();
    let mut libs = std::fs::read_dir(&xpm)
        .map_err(|e| format!("reading {}: {e}", xpm.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    libs.sort();
    for lib in libs {
        let Some(name) = lib.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(lib.join(format!("{name}.xml")))
        else {
            continue;
        };
        for (module, script) in parse_xpm_xml(&text) {
            found.push((module, lib.join(script)));
        }
    }
    Ok(found)
}

/// `(module, script)` for each `<module>` that names an `xdc`-typed Tcl
/// file, the script relative to the library's directory.
fn parse_xpm_xml(text: &str) -> Vec<(String, String)> {
    let module =
        regex::Regex::new(r#"(?s)<module name="([^"]+)">(.*?)</module>"#)
            .unwrap();
    let script =
        regex::Regex::new(r#"<file path="([^"]+\.tcl)" file_type="xdc">"#)
            .unwrap();
    module
        .captures_iter(text)
        .flat_map(|m| {
            let name = m[1].to_string();
            script
                .captures_iter(&m[2])
                .map(|s| (name.clone(), s[1].to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn finds_scoped_constraint_scripts_in_xpm_metadata() {
        let xml = r#"
      <module name="xpm_cdc_single">
        <file path="tcl/xpm_cdc_single.tcl" file_type="xdc">
          <processing_order>late</processing_order>
        </file>
      </module>
      <module name="xpm_cdc_pulse">
      </module>
      <module name="xpm_cdc_gray">
        <file path="tcl/xpm_cdc_gray.tcl" file_type="xdc">
        </file>
      </module>"#;
        assert_eq!(
            parse_xpm_xml(xml),
            vec![
                ("xpm_cdc_single".into(), "tcl/xpm_cdc_single.tcl".into()),
                ("xpm_cdc_gray".into(), "tcl/xpm_cdc_gray.tcl".into()),
            ]
        );
    }

    fn ip() -> Vec<Built> {
        vec![
            Built {
                name: "cips".into(),
                kind: Kind::BlockDesign,
                dcp: "/o/cips.dcp".into(),
                stub: "/o/cips_stub.vhdl".into(),
                cached: true,
            },
            Built {
                name: "clk".into(),
                kind: Kind::Xci,
                dcp: "/o/clk.dcp".into(),
                stub: "/o/clk_stub.vhdl".into(),
                cached: false,
            },
        ]
    }

    #[test]
    fn binds_each_stub_through_its_library() {
        let sources = Sources {
            dependencies: vec![("quartz".into(), vec!["/d/a.vhd".into()])],
            ip_wrappers: vec!["/t/ip/cips/wrapper.vhd".into()],
            design: vec!["/w/hdl/top.vhd".into()],
            synth_constraints: Vec::new(),
        };
        let tcl = user_tcl(
            "top",
            "xcvp1202-vsva2785-3HP-e-S",
            true,
            &sources,
            &ip(),
            Path::new("/s/modular/top.edf"),
        )
        .join("\n");
        assert!(
            tcl.contains("read_vhdl -vhdl2019 -library ip {/o/cips_stub.vhdl}"),
            "{tcl}"
        );
        assert!(
            tcl.contains("read_vhdl -vhdl2019 -library xil_defaultlib {/o/clk_stub.vhdl}"),
            "{tcl}"
        );
        assert!(
            tcl.contains(
                "read_vhdl -vhdl2019 -library quartz [list {/d/a.vhd}]"
            ),
            "{tcl}"
        );
        assert!(
            tcl.contains(
                "synth_design -top top -part xcvp1202-vsva2785-3HP-e-S"
            ),
            "{tcl}"
        );
        assert!(
            tcl.ends_with("write_edif -force {/s/modular/top.edf}"),
            "{tcl}"
        );
        assert!(!tcl.contains("-no_iobuf"), "{tcl}");
    }

    #[test]
    fn link_fails_on_unfilled_black_boxes_and_writes_the_checkpoint_last() {
        let tcl = link_tcl(
            "top",
            "p",
            Path::new("/s/modular/top.edf"),
            &ip(),
            &["/w/constraints/synth/t.xdc".into()],
            &[(
                "xpm_cdc_gray".into(),
                "/x/xpm_cdc/tcl/xpm_cdc_gray.tcl".into(),
            )],
            Path::new("/s/top.dcp"),
        );
        let link = tcl
            .iter()
            .position(|t| t.starts_with("link_design"))
            .unwrap();
        assert!(tcl[link + 1].contains("IS_BLACKBOX"));
        assert!(tcl
            .iter()
            .any(|t| t
                .contains("xpm_cdc_gray {/x/xpm_cdc/tcl/xpm_cdc_gray.tcl}")));
        assert!(tcl
            .last()
            .unwrap()
            .ends_with("write_checkpoint -force {/s/top.dcp}"));
    }
}
