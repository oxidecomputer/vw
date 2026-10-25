// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! One synthesized checkpoint per IP module, built in parallel.
//!
//! Every block design and standalone XCI in the session's project is
//! synthesized out of context in a Vivado of its own, all at once. They can
//! be: once `ip::configure` has run, each module's generated sources are
//! complete, and nothing one IP's synthesis does depends on another's. Each
//! worker opens the persisted project read-only, so the workers share the
//! project and nothing else, and on one host they share one Synthesis
//! license seat.
//!
//! Each checkpoint is cached against a fingerprint of the module's `.bd` /
//! `.xci` (which holds its whole configuration, sub-IPs included), the part,
//! the Vivado install and the recipe below, so a module whose configuration
//! did not change is not synthesized again. A configuration change that
//! moves another module — CIPS' clock frequencies feeding the clock wizards,
//! say — changes that module's `.xci` too, so it is rebuilt as well.
//!
//! What a module's job runs is fixed here rather than written in htcl: the
//! htcl side says which project, and this decides how it is built.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use camino::Utf8Path;
use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use vw_eda::StreamKind;

use crate::job::{tcl_word, Job, Report};
use crate::rpc::RpcContext;
use crate::worker::VivadoConfig;

/// Bumped whenever [`job_tcl`] changes what a checkpoint contains, so
/// checkpoints built the old way are not reused.
const RECIPE: &str = "ip-checkpoint-1";

/// Where a session's IP checkpoints live, under its build directory.
const CHECKPOINT_DIR: &str = "ip-checkpoints";

/// What an IP module is in the project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    BlockDesign,
    Xci,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::BlockDesign => "bd",
            Kind::Xci => "xci",
        }
    }
}

/// An IP module of the project, as the `.xpr` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Module {
    name: String,
    kind: Kind,
    /// Its `.bd` or `.xci`.
    source: PathBuf,
}

/// What the `.xpr` says about the project, as far as building IP goes.
#[derive(Debug, PartialEq, Eq)]
struct Project {
    part: String,
    modules: Vec<Module>,
    /// Files in `constrs_1`.
    constraints: Vec<String>,
}

/// One IP module's synthesized checkpoint and stub.
pub(crate) struct Built {
    pub name: String,
    pub kind: Kind,
    pub dcp: PathBuf,
    pub stub: PathBuf,
    /// Reused rather than synthesized this time.
    pub cached: bool,
}

/// Every IP module of a session's project, synthesized.
pub(crate) struct Synthesized {
    /// The project's part.
    pub part: String,
    /// The Vivado that built them.
    pub vivado: PathBuf,
    pub modules: Vec<Built>,
}

/// The `synth_ip` RPC: synthesize (or reuse) a checkpoint and a VHDL stub
/// for every IP module of the session's project, and answer with where they
/// are: `{<module>: {dcp, stub, kind, cached}}`.
///
/// Optional args: `{"jobs": N}`, the most workers to run at once (absent or
/// 0: one per module).
pub(crate) async fn synth_ip(
    workspace_root: Option<&Path>,
    active_variant: Option<&str>,
    args: Value,
    ctx: RpcContext,
) -> Result<Value, String> {
    let ws = workspace_root
        .and_then(Utf8Path::from_path)
        .ok_or("synth_ip: no workspace root")?;
    let jobs = jobs_arg(&args);
    let built = synthesize(ws, active_variant, jobs, &ctx).await?;
    let mut answer = Map::new();
    for m in built.modules {
        answer.insert(
            m.name,
            json!({
                "dcp": m.dcp.display().to_string(),
                "stub": m.stub.display().to_string(),
                "kind": m.kind.as_str(),
                "cached": m.cached,
            }),
        );
    }
    Ok(Value::Object(answer))
}

/// `{"jobs": N}`: the most workers to run at once, `None` for one per job.
pub(crate) fn jobs_arg(args: &Value) -> Option<usize> {
    args.get("jobs")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .map(|n| n as usize)
}

/// Synthesize (or reuse) every IP module of the session's project, at most
/// `jobs` at once.
pub(crate) async fn synthesize(
    ws: &Utf8Path,
    active_variant: Option<&str>,
    jobs: Option<usize>,
    ctx: &RpcContext,
) -> Result<Synthesized, String> {
    let name = vw_lib::load_workspace_config(ws)
        .map_err(|e| format!("synth_ip: {e}"))?
        .workspace
        .name;
    let project_dir = vw_lib::vw_project_dir(ws, active_variant);
    let xpr = vw_lib::vw_project_xpr(project_dir.as_std_path(), &name);
    let srcs = xpr.with_extension("srcs");
    let text = std::fs::read_to_string(&xpr)
        .map_err(|e| format!("synth_ip: reading {}: {e}", xpr.display()))?;
    let project = parse_xpr(&text, &srcs)?;

    // A read-only project applies whatever constraints are registered in
    // it and can't be told not to, and design constraints have no place in
    // an IP's checkpoint: they would be applied to it and travel with it.
    if !project.constraints.is_empty() {
        return Err(format!(
            "synth_ip: {} has design constraints registered in \
             constrs_1 ({}); remove them from the project before building \
             IP checkpoints",
            xpr.display(),
            project.constraints.join(", "),
        ));
    }

    let vivado = crate::worker::resolve_vivado(&VivadoConfig::default())
        .map_err(|e| format!("synth_ip: {e}"))?;
    let out = vw_lib::build_dir(ws, active_variant).join(CHECKPOINT_DIR);
    std::fs::create_dir_all(out.as_std_path())
        .map_err(|e| format!("synth_ip: creating {out}: {e}"))?;
    let root = project_dir.join(&name);

    let mut built = Vec::new();
    let mut todo = Vec::new();
    for module in project.modules {
        let dcp = out.join(format!("{}.dcp", module.name)).into_std_path_buf();
        let stub = out
            .join(format!("{}_stub.vhdl", module.name))
            .into_std_path_buf();
        let salt = format!(
            "{RECIPE}|{}|{}|{}",
            vivado.display(),
            project.part,
            module.name
        );
        let fingerprint = vw_lib::content_fingerprint(
            &root,
            &salt,
            std::slice::from_ref(&module.source),
        );
        let cached = stub.exists()
            && vw_lib::checkpoint_status(&dcp, fingerprint)
                == vw_lib::CheckpointStatus::Fresh;
        if cached {
            ctx.emit(
                StreamKind::Info,
                format!(
                    "INFO: [vw] IP checkpoint {} is up to date\n",
                    module.name
                ),
            );
        } else {
            // Nothing of an earlier build survives a failed one: a
            // checkpoint without its stub, or either without the mark, is
            // rebuilt.
            for stale in [&dcp, &stub] {
                let _ = std::fs::remove_file(stale);
            }
            let (setup, steps) =
                job_tcl(&module.name, &xpr, &project.part, &dcp, &stub);
            todo.push((
                module.name.clone(),
                dcp.clone(),
                fingerprint,
                Job {
                    label: format!("IP synthesis of {}", module.name),
                    vivado: vivado.clone(),
                    dir: out
                        .join("jobs")
                        .join(&module.name)
                        .into_std_path_buf(),
                    setup,
                    steps,
                    report: Report::Problems,
                },
            ));
        }
        built.push(Built {
            name: module.name,
            kind: module.kind,
            dcp,
            stub,
            cached,
        });
    }
    let done = Synthesized {
        part: project.part,
        vivado,
        modules: built,
    };
    if todo.is_empty() {
        return Ok(done);
    }

    let jobs = jobs.unwrap_or(todo.len());
    ctx.emit(
        StreamKind::Info,
        format!(
            "INFO: [vw] synthesizing {} IP module(s), {} at a time: {}\n",
            todo.len(),
            jobs.min(todo.len()),
            todo.iter()
                .map(|(name, ..)| name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    );

    let permits = Arc::new(Semaphore::new(jobs));
    let mut running = JoinSet::new();
    for (name, dcp, fingerprint, job) in todo {
        let permits = permits.clone();
        let ctx = ctx.clone();
        running.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let result = job.run(&ctx).await.and_then(|seconds| {
                vw_lib::write_checkpoint_fingerprint(&dcp, fingerprint)
                    .map(|()| seconds)
                    .map_err(|e| format!("marking {}: {e}", dcp.display()))
            });
            (name, job.log(), result)
        });
    }
    // The first failure ends the whole call: what was asked for can't be
    // delivered, and the rest may have most of an hour left to run. Dropping
    // the set aborts the other jobs, and with them their Vivados.
    while let Some(joined) = running.join_next().await {
        let (name, log, result) =
            joined.map_err(|e| format!("synth_ip: a job panicked: {e}"))?;
        match result {
            Ok(seconds) => ctx.emit(
                StreamKind::Info,
                format!(
                    "INFO: [vw] IP checkpoint {name} built in {seconds:.0}s\n"
                ),
            ),
            Err(e) => {
                return Err(format!(
                    "synth_ip: synthesizing {name} failed: {e} (log: {})",
                    log.display()
                ));
            }
        }
    }
    Ok(done)
}

/// What a module's worker evaluates, in order: opening the project, then
/// building the module.
fn job_tcl(
    module: &str,
    xpr: &Path,
    part: &str,
    dcp: &Path,
    stub: &Path,
) -> (Vec<String>, Vec<String>) {
    let open = vec![
        // Project-mode synthesis of one module still reads every IP's
        // scoped constraints, and reports each one whose module isn't under
        // the top it is synthesizing. In a job that synthesizes a single IP
        // that is every other IP's, by construction: a module of this IP's
        // own would be found. Expected, so not a critical warning here.
        "set_msg_config -id {Constraints 18-5119} -new_severity INFO\n\
         set_msg_config -id {Designutils 20-1280} -new_severity INFO"
            .to_string(),
        format!("open_project -read_only {}", tcl_word(xpr)),
    ];
    let build = vec![
        format!("synth_design -top {module} -part {part} -mode out_of_context"),
        format!("write_checkpoint -force {}", tcl_word(dcp)),
        // Written last: a stub means the checkpoint before it is whole.
        format!("write_vhdl -force -mode synth_stub {}", tcl_word(stub)),
    ];
    (open, build)
}

/// Read the part, the IP modules and the registered constraints out of a
/// project's `.xpr`. `srcs` is the project's `<name>.srcs` directory, which
/// the `.xpr` calls `$PSRCDIR`.
fn parse_xpr(text: &str, srcs: &Path) -> Result<Project, String> {
    let part = regex::Regex::new(r#"<Option Name="Part" Val="([^"]+)"/>"#)
        .unwrap()
        .captures(text)
        .map(|c| c[1].to_string())
        .ok_or("synth_ip: the .xpr names no part")?;
    let fileset = |name: &str| -> &str {
        let open = format!("<FileSet Name=\"{name}\"");
        text.find(&open)
            .map(|start| {
                let rest = &text[start..];
                &rest[..rest.find("</FileSet>").unwrap_or(rest.len())]
            })
            .unwrap_or("")
    };
    let file = regex::Regex::new(r#"<File Path="([^"]+)""#).unwrap();

    // A module is a `.bd` or `.xci` in a directory of its own name, the way
    // Vivado lays them out. A BD's sub-IPs live inside the BD and are not
    // listed, so every one found here is a top-level module.
    let module = regex::Regex::new(
        r"^\$PSRCDIR/sources_1/(?:bd|ip)/([^/]+)/([^/]+)\.(bd|xci)$",
    )
    .unwrap();
    let mut modules = Vec::new();
    for c in file.captures_iter(fileset("sources_1")) {
        let path = &c[1];
        let Some(m) = module.captures(path) else {
            continue;
        };
        if m[1] != m[2] {
            continue;
        }
        modules.push(Module {
            name: m[1].to_string(),
            kind: if &m[3] == "bd" {
                Kind::BlockDesign
            } else {
                Kind::Xci
            },
            source: srcs.join(path.trim_start_matches("$PSRCDIR/")),
        });
    }
    let constraints = file
        .captures_iter(fileset("constrs_1"))
        .map(|c| c[1].to_string())
        .collect();
    Ok(Project {
        part,
        modules,
        constraints,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    const XPR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Project Version="7">
  <Configuration>
    <Option Name="Part" Val="xcvp1202-vsva2785-3HP-e-S"/>
  </Configuration>
  <FileSets Version="1" Minor="32">
    <FileSet Name="sources_1" Type="DesignSrcs" RelSrcDir="$PSRCDIR/sources_1">
      <Filter Type="Srcs"/>
      <File Path="$PSRCDIR/sources_1/bd/cips/cips.bd">
        <FileInfo/>
      </File>
      <File Path="$PSRCDIR/sources_1/ip/clk_eth/clk_eth.xci">
        <FileInfo/>
      </File>
      <File Path="$PSRCDIR/sources_1/bd/dcmac_bd/hdl/dcmac_bd_wrapper.vhd">
        <FileInfo/>
      </File>
    </FileSet>
    <FileSet Name="constrs_1" Type="Constrs" RelSrcDir="$PSRCDIR/constrs_1">
      <Filter Type="Constrs"/>
    </FileSet>
  </FileSets>
</Project>
"#;

    #[test]
    fn reads_modules_part_and_constraints_from_an_xpr() {
        let srcs = Path::new("/p/x.srcs");
        let project = parse_xpr(XPR, srcs).unwrap();
        assert_eq!(project.part, "xcvp1202-vsva2785-3HP-e-S");
        assert_eq!(
            project.modules,
            vec![
                Module {
                    name: "cips".into(),
                    kind: Kind::BlockDesign,
                    source: srcs.join("sources_1/bd/cips/cips.bd"),
                },
                Module {
                    name: "clk_eth".into(),
                    kind: Kind::Xci,
                    source: srcs.join("sources_1/ip/clk_eth/clk_eth.xci"),
                },
            ]
        );
        assert!(project.constraints.is_empty());
    }

    #[test]
    fn reports_registered_constraints() {
        let xpr = XPR.replace(
            r#"<Filter Type="Constrs"/>"#,
            r#"<Filter Type="Constrs"/>
      <File Path="$PPRDIR/../../../constraints/place/pins.xdc"/>"#,
        );
        let project = parse_xpr(&xpr, Path::new("/p/x.srcs")).unwrap();
        assert_eq!(
            project.constraints,
            vec!["$PPRDIR/../../../constraints/place/pins.xdc".to_string()]
        );
    }

    #[test]
    fn the_job_writes_its_stub_last() {
        let (open, build) = job_tcl(
            "cips",
            Path::new("/p/x.xpr"),
            "xcvp1202-vsva2785-3HP-e-S",
            Path::new("/o/cips.dcp"),
            Path::new("/o/cips_stub.vhdl"),
        );
        assert!(open.last().unwrap().starts_with("open_project -read_only"));
        assert!(build[0].contains("-mode out_of_context"));
        assert!(build.last().unwrap().contains("synth_stub"));
    }
}
