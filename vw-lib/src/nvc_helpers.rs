use crate::{VhdlStandard, VwError};

use tokio::process::Command;

use std::{io::Write, process::{ExitStatus, Output}};

fn get_base_nvc_cmd_args(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
) -> Vec<String> {
    let lib_dir = build_dir.clone() + "/" + lib_name;
    let args = vec![
        format!("--std={std}"),
        format!("--work={lib_dir}"),
        "-M".to_string(),
        "256m".to_string(),
        "-L".to_string(),
        build_dir.clone()
    ];
    args
}

async fn run_cmd_w_output(
    args : &Vec<String>,
    envs : Option<&Vec<(String, String)>>
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

    nvc_cmd.output().await.map_err(|e| 
            VwError::Testbench { 
                message: format!("nvc command failed : {e}")
    })
}

async fn run_cmd(
    args : &Vec<String>,
    envs : Option<&Vec<(String, String)>>
) -> Result<ExitStatus, VwError>{
    let mut nvc_cmd = Command::new("nvc");
    for arg in args {
        nvc_cmd.arg(arg);
    }
    
    if let Some(vars) = envs {
        for (env_var, value) in vars {
            nvc_cmd.env(env_var, value);
        }
    }

    nvc_cmd.status().await.map_err(|e| 
            VwError::Testbench { 
                message: format!("nvc command failed : {e}")
    })
}

pub async fn run_nvc_analysis(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
    referenced_files : &Vec<String>,
    capture_output : bool
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-a".to_string());

    for file in referenced_files {
        args.push(file.clone());
    }

    if capture_output {
        let output = run_cmd_w_output(&args, None).await?;

        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;
            return Err(VwError::NvcAnalysis { 
                library: lib_name.clone(), 
                command: cmd_str
            })
        }
        Ok(Some((output.stdout, output.stderr)))
    }
    else {
        let status = run_cmd(&args, None).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(VwError::NvcAnalysis { 
                library: lib_name.clone(), 
                command: cmd_str 
            })
        }
        Ok(None)
    }

}

pub async fn run_nvc_elab(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
    testbench_name : &String,
    capture_output : bool
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-e".to_string());
    args.push(testbench_name.clone());

    if capture_output {
        let output = run_cmd_w_output(&args, None).await?;
        if !output.status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stdout().write_all(&output.stderr)?;

            return Err(VwError::NvcElab { command: cmd_str });
        }
        Ok(Some((output.stdout, output.stderr)))

    }
    else {
        let status = run_cmd(&args, None).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(
                VwError::NvcElab { command: cmd_str }
            );
        }

        Ok(None)
    }

}

pub async fn run_nvc_sim(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
    testbench_name : &String,
    rust_lib_path : Option<String>,
    runtime_flags : &Vec<String>,
    capture_output : bool
) -> Result<Option<(Vec<u8>, Vec<u8>)>, VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-r".to_string());
    args.push(testbench_name.clone());

    for flag in runtime_flags {
        args.push(flag.clone());
    }

    args.push("--dump-arrays".to_string());
    args.push("--format=fst".to_string());
    args.push(format!("--wave={testbench_name}.fst"));

    let envs = match rust_lib_path {
        Some(path) => {
            args.push(format!("--load={path}"));
            let envs_vec = vec![("GPI_USERS".to_string(), path.clone())];
            Some(envs_vec)
        }
        None => {
            None
        }
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

    }
    else {
        let status = run_cmd(&args, envs.as_ref()).await?;

        if !status.success() {
            let cmd_str = format!("nvc {}", args.join(" "));
            return Err(
                VwError::NvcSimulation { command: cmd_str }
            );
        }
        Ok(None)
    }

}

