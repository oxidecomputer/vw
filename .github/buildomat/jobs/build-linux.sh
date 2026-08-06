#!/bin/bash
#:
#: name = "build-linux"
#: variety = "basic"
#: target = "ubuntu-24.04"
#: rust_toolchain = "stable"
#: output_rules = [
#:   "/work/release/*",
#: ]
#: access_repos = [
#:   "oxidecomputer/ipe",
#:   "oxidecomputer/anodizer"
#: ]
#:
#: [[publish]]
#: series = "linux"
#: name = "vw"
#: from_output = "/work/release/vw"
#: 
#: [[publish]]
#: series = "linux"
#: name = "vw-agent"
#: from_output = "/work/release/vw-agent"
#: 
#: [[publish]]
#: series = "linux"
#: name = "vw-svc"
#: from_output = "/work/release/vw-svc"
#:
#: [[publish]]
#: series = "linux"
#: name = "vw-analyzer"
#: from_output = "/work/release/vw-analyzer"

set -o errexit
set -o pipefail
set -o xtrace

sudo apt-get install build-essential pkg-config libssl-dev libfontconfig-dev -y

cargo --version
rustc --version

banner "api"
cargo xtask openapi generate

banner "check"
cargo fmt -- --check
cargo clippy --all-targets -- --deny warnings

banner "build"
cargo build --release
mkdir -p /work/release/
cp target/release/vw /work/release/
cp target/release/vw-agent /work/release/
cp target/release/vw-svc /work/release/
cp target/release/vw-analyzer /work/release/
