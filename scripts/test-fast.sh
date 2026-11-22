#!/usr/bin/env sh
# The fast, default package set. No Wasmtime host, no UI, no device crates.
set -eu
cargo test -p soil-abi -p soil-lang -p soil-agent-host -p soil-agentsd -p soil-sdk-lua "$@"
