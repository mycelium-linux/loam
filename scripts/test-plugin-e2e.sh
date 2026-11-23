#!/usr/bin/env bash
# End-to-end function plugin test: adapter -> loam:plugin/ipc -> sidecar.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

ADAPTER_TARGET="$WORK/adapter-target"
CLIENT_TARGET="$WORK/client-target"

FPL_BOB_LOCAL=1 CARGO_TARGET_DIR="$ADAPTER_TARGET" \
  cargo build --manifest-path "$ROOT/crates/loam/fixtures/mcp-adapter/Cargo.toml" \
  --target wasm32-wasip2 --release
FPL_BOB_LOCAL=1 CARGO_TARGET_DIR="$CLIENT_TARGET" \
  cargo build --manifest-path "$ROOT/crates/loam/fixtures/mcp-client/Cargo.toml" \
  --target wasm32-wasip2 --release
CARGO_TARGET_DIR="$TARGET" cargo build -p loam --features surface --bin loam --bin loam-mcp

LOAM="$TARGET/debug/loam"
SPORES="$WORK/spores"
COMMANDS="$WORK/commands"
PROVIDER="$WORK/mcp-provider"
CONSUMER="$WORK/mcp-consumer"

mkdir -p "$SPORES" "$COMMANDS" "$PROVIDER/bin" "$PROVIDER/signatures" "$CONSUMER"
cp "$ROOT/crates/loam/fixtures/mcp-package/spore.toml" "$PROVIDER/spore.toml"
cp "$ADAPTER_TARGET/wasm32-wasip2/release/loam_mcp_adapter.wasm" "$PROVIDER/module.wasm"
cp "$TARGET/debug/loam-mcp" "$PROVIDER/bin/loam-mcp"
chmod 755 "$PROVIDER/bin/loam-mcp"
printf 'local test seal\n' > "$PROVIDER/signatures/local.asc"
"$LOAM" plugin pack "$PROVIDER" >/dev/null
"$LOAM" plugin verify "$PROVIDER" >/dev/null
"$LOAM" install "$PROVIDER" --root "$SPORES" --allow-unsigned-dev >/dev/null
"$LOAM" plugin list --root "$SPORES" --json | grep -q '"kind": "native-sidecar"'

cp "$CLIENT_TARGET/wasm32-wasip2/release/loam_mcp_client.wasm" "$CONSUMER/module.wasm"
cat > "$CONSUMER/command.toml" <<'TOML'
schema = 1
[command]
name = "mcp-client"
version = "0.1.0"
summary = "exercise the mcp plugin end to end"
module = "module.wasm"
TOML
"$LOAM" install "$CONSUMER" --root "$COMMANDS" --provider-root "$SPORES" --allow-unsigned-dev >/dev/null
OUT=$("$LOAM" run --root "$COMMANDS" --root "$SPORES" mcp-client)
printf '%s\n' "$OUT"
printf '%s\n' "$OUT" | grep -q 'tool: echo'
printf '%s\n' "$OUT" | grep -q 'tool: write-note'
echo "plugin e2e ok"
