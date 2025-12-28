use crate::{VhdlStandard, VwError};

use tokio::process::Command;

use std::process::ExitStatus;

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
    referenced_files : &Vec<String>
) -> Result<(), VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-a".to_string());

    for file in referenced_files {
        args.push(file.clone());
    }

    let status = run_cmd(&args, None).await?;

    if !status.success() {
        let cmd_str = format!("nvc {}", args.join(" "));
        return Err(VwError::NvcAnalysis { 
            library: lib_name.clone(), 
            command: cmd_str 
        })
    }
    Ok(())
}

pub async fn run_nvc_elab(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
    testbench_name : &String
) -> Result<(), VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-e".to_string());
    args.push(testbench_name.clone());

    let status = run_cmd(&args, None).await?;

    if !status.success() {
        let cmd_str = format!("nvc {}", args.join(" "));
        return Err(
            VwError::NvcElab { command: cmd_str }
        );
    }

    Ok(())
}

pub async fn run_nvc_sim(
    std: VhdlStandard, 
    build_dir: &String,
    lib_name: &String,
    testbench_name : &String,
    rust_lib_path : Option<String>,
    runtime_flags : &Vec<String>
) -> Result<(), VwError> {
    let mut args = get_base_nvc_cmd_args(std, build_dir, lib_name);
    args.push("-r".to_string());
    args.push(testbench_name.clone());

    for flag in runtime_flags {
        args.push(flag.clone());
    }

    args.push("--dump-arrays".to_string());
    args.push("--format=fst".to_string());
    args.push(format!("--wave={testbench_name}.fst"));

    if let Some(path) = rust_lib_path {
        let envs = vec![("GPI_USERS".to_string(), path.clone())];
        args.push(format!("--load={path}"));
        run_cmd(&args, Some(&envs)).await?;
    }
    else {
        run_cmd(&args, None).await?;
    }

    Ok(())
}

