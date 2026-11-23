#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-"$ROOT/target/loam-mcp-plugin"}
ADAPTER_TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}/loam-mcp-adapter
FIXTURE="$ROOT/crates/loam/fixtures/mcp-adapter"
PACKAGE="$ROOT/crates/loam/fixtures/mcp-package"

FPL_BOB_LOCAL=1 CARGO_TARGET_DIR="$ADAPTER_TARGET" \
    cargo build --manifest-path "$FIXTURE/Cargo.toml" --target wasm32-wasip2 --release
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT/target"} cargo build -p loam --release --bin loam-mcp
cargo build -p loam --features surface --bin loam

rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/signatures"
cp "$PACKAGE/spore.toml" "$OUT/spore.toml"
# A local test seal. Real publishing uses the Spore publisher signature path;
# the registry currently refuses an unsealed tree, not an untrusted one.
printf 'local test seal; replace with a detached publisher signature\n' > "$OUT/signatures/local.asc"
cp "$ADAPTER_TARGET/wasm32-wasip2/release/loam_mcp_adapter.wasm" "$OUT/module.wasm"
cp "${CARGO_TARGET_DIR:-"$ROOT/target"}/release/loam-mcp" "$OUT/bin/loam-mcp"

"${CARGO_TARGET_DIR:-"$ROOT/target"}/debug/loam" plugin pack "$OUT"
echo "mcp plugin package: $OUT"
