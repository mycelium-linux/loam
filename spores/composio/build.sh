#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
FPL_BOB_LOCAL=1 CARGO_TARGET_DIR="$ROOT/target/loam-mcp-adapter" \
  cargo build --manifest-path crates/loam/fixtures/mcp-adapter/Cargo.toml \
  --target wasm32-wasip2 --release
cargo build -p loam --release --bin loam-mcp
cp target/loam-mcp-adapter/wasm32-wasip2/release/loam_mcp_adapter.wasm spores/composio/module.wasm
cp target/release/loam-mcp spores/composio/bin/loam-mcp
chmod 755 spores/composio/bin/loam-mcp
printf 'local test seal; replace with a detached publisher signature\n' > spores/composio/signatures/local.asc
