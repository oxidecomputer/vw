// Smoke tests that load the actual Xilinx IP-XACT files from the local
// Vivado install. Skipped automatically when the files aren't present
// so this still passes in CI without a Vivado install.

use std::path::Path;

use vw_ip::{generate, group_parameters, load, GenerateOptions, Summary};

const CIPS: &str =
    "/home/ry/Xilinx/2025.1/data/rsb/iprepos/versal_cips_v3_4/component.xml";
const CPM5: &str =
    "/home/ry/Xilinx/2025.1/data/ip/xilinx/cpm5_v1_0/component.xml";

fn skip_if_missing(p: &str) -> bool {
    if Path::new(p).exists() {
        false
    } else {
        eprintln!("skipping: {p} not present");
        true
    }
}

#[test]
fn loads_cips_component() {
    if skip_if_missing(CIPS) {
        return;
    }
    let component = load(CIPS).expect("load CIPS");
    let summary = Summary::of(&component);
    eprintln!("CIPS summary: {summary:#?}");
    assert!(summary.vlnv.contains("versal_cips"));
}

#[test]
fn loads_cpm5_component() {
    if skip_if_missing(CPM5) {
        return;
    }
    let component = load(CPM5).expect("load CPM5");
    let summary = Summary::of(&component);
    eprintln!("CPM5 summary: {summary:#?}");
    assert!(summary.vlnv.contains("cpm5"));
    // CPM5 should be huge.
    assert!(
        summary.parameter_count > 1000,
        "expected many parameters, got {}",
        summary.parameter_count
    );
}

#[test]
fn generates_cips_wrapper_that_reparses() {
    if skip_if_missing(CIPS) {
        return;
    }
    let component = load(CIPS).expect("load CIPS");
    let out =
        generate(&component, &Default::default(), &GenerateOptions::default());
    eprintln!("--- generated CIPS wrapper (first 60 lines) ---");
    for line in out.lines().take(60) {
        eprintln!("{line}");
    }
    eprintln!("--- ({} lines total) ---", out.lines().count());

    let parsed = vw_htcl::parse(&out);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );

    // Validate the generated wrapper against its own signature using
    // the same validator the LSP runs.
    let diags = vw_htcl::validate(&parsed.document, &out);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == vw_htcl::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "validator errors: {errors:#?}");
}

#[test]
fn generates_cpm5_wrapper_in_split_mode() {
    if skip_if_missing(CPM5) {
        return;
    }
    let component = load(CPM5).expect("load CPM5");
    let out =
        generate(&component, &Default::default(), &GenerateOptions::default());

    // Walk the generated source and measure per-proc arg counts so we
    // can assert nothing is anywhere near the 4200-arg PCIE1 disaster
    // we started with.
    let mut proc_sizes: Vec<(String, usize)> = Vec::new();
    let mut current: Option<(String, usize)> = None;
    let mut in_args = false;
    for line in out.lines() {
        if let Some(name) = line
            .strip_prefix("proc ")
            .and_then(|s| s.split_once(' ').map(|(n, _)| n))
        {
            current = Some((name.to_string(), 0));
            in_args = true;
        } else if line == "} {" {
            if let Some(c) = current.take() {
                proc_sizes.push(c);
            }
            in_args = false;
        } else if in_args
            && line.starts_with("  ")
            && !line.trim_start().starts_with("##")
            && !line.trim().is_empty()
        {
            if let Some(c) = current.as_mut() {
                c.1 += 1;
            }
        }
    }
    proc_sizes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let (max_name, max_size) = proc_sizes[0].clone();
    let total_procs = proc_sizes.len();
    eprintln!(
        "CPM5 wrapper: {} procs, {} lines, biggest is {} ({} args)",
        total_procs,
        out.lines().count(),
        max_name,
        max_size
    );
    for (n, s) in proc_sizes.iter().take(8) {
        eprintln!("  {n:>40} = {s} args");
    }

    // Hierarchical split should leave every proc small enough to
    // navigate in an LSP — no more 4200-arg procs.
    assert!(
        max_size <= 250,
        "biggest proc {max_name} still has {max_size} args; \
         hierarchy isn't splitting deep enough"
    );
    // And the overall proc count should reflect that we *are* splitting.
    assert!(
        total_procs > 50,
        "only {total_procs} procs — hierarchy isn't being built"
    );

    assert!(out.contains("proc create_cpm5 {\n  ## Instance name"));
    assert!(out.contains("proc create_cpm5_cpm_pcie0 "));
    assert!(out.contains("proc create_cpm5_cpm_pcie1 "));

    let parsed = vw_htcl::parse(&out);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let diags = vw_htcl::validate(&parsed.document, &out);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == vw_htcl::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "validator errors: {errors:#?}");
}

#[test]
fn groups_cpm5_parameters_into_handful_of_buckets() {
    if skip_if_missing(CPM5) {
        return;
    }
    let component = load(CPM5).expect("load CPM5");
    let params: Vec<_> = component.component_parameters().collect();
    let groups = group_parameters(params.iter().copied(), 2);
    eprintln!("CPM5 has {} groups at prefix=2:", groups.len());
    for g in groups.iter().take(20) {
        eprintln!("  {:>32} = {} params", g.key, g.parameters.len());
    }
    eprintln!("  ... ({} total)", groups.len());
    // We expect a manageable number of groups (not one giant flat list,
    // not thousands of singletons).
    assert!(groups.len() < 200, "too many groups: {}", groups.len());
    assert!(groups.len() > 2, "too few groups: {}", groups.len());
}
