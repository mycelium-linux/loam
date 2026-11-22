#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}
FIXTURE="$ROOT/crates/loam-surface/fixtures/rgba"
WASM="$TARGET/surface-fixture/wasm32-wasip2/release/loam_surface_fixture_rgba.wasm"
OUT=$(mktemp "${TMPDIR:-/tmp}/loam-surface.XXXXXX.rgba")
trap 'rm -f "$OUT"' EXIT

FPL_BOB_LOCAL=1 CARGO_TARGET_DIR="$TARGET/surface-fixture" \
    cargo build --manifest-path "$FIXTURE/Cargo.toml" \
    --target wasm32-wasip2 --release

cargo build -p loam-cli
BIN="$TARGET/debug/loam"
"$BIN" surface "$WASM" --size 64x64 --capture "$OUT"

python3 - "$OUT" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
data = path.read_bytes()
width = height = 64
assert len(data) == width * height * 4, (len(data), width * height * 4)
assert data[0:4] == bytes((0, 0, 0, 255)), "corner is not black"
# Quarter 1 starts after the black corner; the first non-corner bar is red.
x, y = width // 3, height // 3
offset = (y * width + x) * 4
assert data[offset : offset + 4] == bytes((0, 255, 0, 255)), data[offset : offset + 4]
print("surface fixture ok: 64x64 RGBA bars")
PY
