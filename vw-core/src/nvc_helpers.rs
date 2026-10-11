use crate::{VhdlStandard, VwError};

use tokio::process::Command;

use std::{
    io::Write,
    process::{ExitStatus, Output},
};

fn get_base_nvc_cmd_args(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
) -> Vec<String> {
    let lib_dir = build_dir.to_owned() + "/" + lib_name;
    let args = vec![
        format!("--std={std}"),
        format!("--work={lib_dir}"),
        "-M".to_string(),
        "256m".to_string(),
        "-L".to_string(),
        build_dir.to_owned(),
    ];
    args
}

async fn run_cmd_w_output(
    args: &Vec<String>,
    envs: Option<&Vec<(String, String)>>,
) -> Result<Output, VwError> {
    let mut nvc_cmd = Command::new("nvc");
    for arg in args {
        nvc_cmd.arg(arg);
    }

    if let Some(vars) = envs {
        for (env_var, value) in vars {
            nvc_cmd.env(env_var, value);
        }
    }

    nvc_cmd.output().await.map_err(|e| VwError::Testbench {
        message: format!("nvc command failed : {e}"),
    })
}

async fn run_cmd(
    args: &Vec<String>,
    envs: Option<&Vec<(String, String)>>,
) -> Result<ExitStatus, VwError> {
    let mut nvc_cmd = Command::new("nvc");
    for arg in args {
        nvc_cmd.arg(arg);
    }

    if let Some(vars) = envs {
        for (env_var, value) in vars {
            nvc_cmd.env(env_var, value);
        }
    }

    nvc_cmd.status().await.map_err(|e| VwError::Testbench {
        message: format!("nvc command failed : {e}"),
    })
}

pub async fn run_nvc_analysis(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    referenced_files: &Vec<String>,
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-a".to_string());

    for file in referenced_files {
        args.push(file.clone());
    }

    // Always capture, even when the caller wants the output passed through:
    // on a remote instance an inherited stderr goes to the server's log, and
    // a failure that reports only the command line gives the developer
    // nothing to act on. The diagnostics have to travel inside the error.
    let output = run_cmd_w_output(&args, None).await?;

    if !output.status.success() {
        return Err(VwError::NvcAnalysis {
            library: lib_name.to_owned(),
            command: format!("nvc {}", args.join(" ")),
            output: nvc_diagnostics(&output),
        });
    }

    if capture_output {
        Ok(Some((output.stdout, output.stderr)))
    } else {
        // Warnings from a successful analysis still belong on the terminal.
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        Ok(None)
    }
}

/// What nvc said, as text: stderr, where it writes its diagnostics, followed
/// by anything it wrote to stdout.
fn nvc_diagnostics(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stdout);
    }
    text.trim_end().to_owned()
}

pub async fn run_nvc_elab(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    testbench_name: &str,
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    run_nvc_elab_with_generics(
        std,
        build_dir,
        lib_name,
        testbench_name,
        &[],
        capture_output,
    )
    .await
}

/// Elaborate `testbench_name`, overriding generics on the way in.
///
/// A testbench that drives a design entity directly has no wrapper to supply
/// the entity's generics through, so it has to set them at elaboration. Every
/// other caller goes through [`run_nvc_elab`] and passes none.
pub async fn run_nvc_elab_with_generics(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    testbench_name: &str,
    generics: &[(String, String)],
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-e".to_string());
    // Before the unit name: nvc reads `-g` as an option of the elaborate
    // command, and anything after the unit belongs to the next one.
    for (name, value) in generics {
        args.push("-g".to_string());
        args.push(format!("{name}={value}"));
    }
    args.push(testbench_name.to_owned());

    if capture_output {
        let output = run_cmd_w_output(&args, None).await?;
        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stdout().write_all(&output.stderr)?;

            return Err(VwError::NvcElab { command: cmd_str });
        }
        Ok(Some((output.stdout, output.stderr)))
    } else {
        let status = run_cmd(&args, None).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(VwError::NvcElab { command: cmd_str });
        }

        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_nvc_sim(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    testbench_name: &String,
    wave_dir: &str,
    rust_lib_path: Option<String>,
    runtime_flags: &Vec<String>,
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-r".to_string());
    args.push(testbench_name.clone());

    for flag in runtime_flags {
        args.push(flag.clone());
    }

    args.push("--dump-arrays".to_string());
    args.push("--format=fst".to_string());
    args.push(format!("--wave={wave_dir}/{testbench_name}.fst"));

    let envs = match rust_lib_path {
        Some(path) => {
            args.push(format!("--load={path}"));
            let envs_vec = vec![("GPI_USERS".to_string(), path.clone())];
            Some(envs_vec)
        }
        None => None,
    };

    if capture_output {
        let output = run_cmd_w_output(&args, envs.as_ref()).await?;

        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stdout().write_all(&output.stderr)?;

            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(Some((output.stdout, output.stderr)))
    } else {
        let status = run_cmd(&args, envs.as_ref()).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(None)
    }
}

/// Run a design entity under NVC with a Rust cosim driver loaded.
///
/// The entity is the top level — there is no VHDL harness — so the driver is
/// the only thing that touches its ports. Unlike [`run_nvc_cosim`] this still
/// writes an FST, because a digital bench is read by looking at waveforms and
/// there is no Xyce output standing in for them.
#[allow(clippy::too_many_arguments)]
pub async fn run_nvc_driven(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    entity_name: &str,
    driver_lib_path: &str,
    wave_dir: &str,
    runtime_flags: &[String],
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-r".to_string());
    args.push(entity_name.to_string());

    for flag in runtime_flags {
        args.push(flag.clone());
    }

    args.push("--dump-arrays".to_string());
    args.push("--format=fst".to_string());
    args.push(format!("--wave={wave_dir}/{entity_name}.fst"));
    args.push(format!("--load={driver_lib_path}"));

    let envs = vec![
        ("GPI_USERS".to_string(), driver_lib_path.to_string()),
        ("COCOTB_RUST_MODE".to_string(), "1".to_string()),
        ("RUST_COSIM_OUTPUT_DIR".to_string(), wave_dir.to_string()),
    ];

    if capture_output {
        let output = run_cmd_w_output(&args, Some(&envs)).await?;
        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;
            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(Some((output.stdout, output.stderr)))
    } else {
        let status = run_cmd(&args, Some(&envs)).await?;
        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(None)
    }
}

/// Run a mixed-signal co-simulation via NVC with a bridge library loaded.
///
/// Unlike `run_nvc_sim`, this sets `COCOTB_RUST_MODE=1` and does not
/// generate FST waveform output (analog output comes from Xyce `.prn`).
pub async fn run_nvc_cosim(
    std: VhdlStandard,
    build_dir: &str,
    lib_name: &str,
    entity_name: &str,
    bridge_lib_path: &str,
    output_dir: &str,
    capture_output: bool,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-r".to_string());
    args.push(entity_name.to_string());
    args.push(format!("--load={bridge_lib_path}"));

    let envs = vec![
        ("GPI_USERS".to_string(), bridge_lib_path.to_string()),
        ("COCOTB_RUST_MODE".to_string(), "1".to_string()),
        // Tells the bench where to write artifacts (Xyce `.prn`, etc.) via
        // `rust_cosim::output_dir()`. Contract string == rust_cosim OUTPUT_DIR_ENV.
        ("RUST_COSIM_OUTPUT_DIR".to_string(), output_dir.to_string()),
    ];

    if capture_output {
        let output = run_cmd_w_output(&args, Some(&envs)).await?;

        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;

            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(Some((output.stdout, output.stderr)))
    } else {
        let status = run_cmd(&args, Some(&envs)).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(VwError::NvcSimulation { command: cmd_str });
        }
        Ok(None)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A failed analysis has to say why, not only which command failed: on a
    /// remote instance nvc's own stderr never reaches the developer.
    #[tokio::test]
    async fn analysis_error_carries_nvc_diagnostics() {
        if std::process::Command::new("nvc")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("nvc not on PATH; skipping");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bad.vhd");
        std::fs::write(
            &src,
            "entity bad is end entity;\n\
             architecture a of bad is\n\
             begin\n  \
               x <= '1';\n\
             end architecture;\n",
        )
        .unwrap();

        let err = run_nvc_analysis(
            VhdlStandard::Vhdl2019,
            dir.path().to_str().unwrap(),
            "work",
            &vec![src.to_string_lossy().into_owned()],
            false,
        )
        .await
        .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("no visible declaration for X"),
            "nvc's diagnostic is missing from the error:\n{message}"
        );
    }
}
