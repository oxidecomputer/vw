#!/bin/bash
#:
#: name = "build-linux"
#: variety = "basic"
#: target = "ubuntu-22.04"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/work/release/*",
#: ]
#:
#: [[publish]]
#: series = "linux"
#: name = "vw"
#: from_output = "/work/release/vw"

set -o errexit
set -o pipefail
set -o xtrace

sudo apt-get install build-essential pkg-config libssl-dev -y

cargo --version
rustc --version

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "build"
cargo build --release
mkdir -p /work/release/
cp target/release/vw /work/release/
