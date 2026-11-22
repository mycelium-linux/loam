#!/usr/bin/env sh
# The language plus its native host bridge and Wasmtime engine.
set -eu
cargo test -p soil-lang --features native "$@"
