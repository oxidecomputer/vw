// Cargo doesn't track files included via `include_str!` for
// rebuild purposes — it only knows about `.rs` source files.
// The Vivado shim is `include_str!`'d into `worker.rs` and
// baked into the binary at compile time; without this build
// script edits to the shim go unnoticed until something else
// triggers a recompile of `vw-vivado`, leaving the deployed
// shim out of sync with the source.
fn main() {
    println!("cargo:rerun-if-changed=shim/vivado-shim.tcl");
}
