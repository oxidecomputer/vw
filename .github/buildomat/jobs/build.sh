#!/bin/bash
#:
#: name = "build"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/work/release/*",
#: ]
#:
#: [[publish]]
#: series = "illumos"
#: name = "vw"
#: from_output = "/work/release/vw"

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "build"
cargo build --release
mkdir -p /work/release/
cp target/release/vw /work/release/
