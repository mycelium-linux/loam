#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

cargo metadata --format-version 1 --no-deps > /tmp/loam-metadata.json

python3 - <<'PY'
import json
import sys

allowed = {
    "loam",
    "loam-surface",
    "loam-media",
    "loam-ble-api",
    "loam-ble-host",
    "loam-ble-sim",
    "soil-codec",
    "soil-inference",
    "soil-inference-host",
    "soil-scrub",
}
allowed_bins = {"loam", "loam-ble-host", "loam-mcp", "loamd"}
forbidden = (
    "soil-lang", "soil-abi", "soil-ui", "soil-layout", "soil-scene",
    "soil-canvas", "soil-terminal", "soil-marbles", "soil-crm",
    "soil-agent", "soil-media", "soil-notify", "soil-store",
    "soil-install", "soil-shell", "tend-core", "viewer-spore",
)

metadata = json.load(open("/tmp/loam-metadata.json"))
packages = {package["name"]: package for package in metadata["packages"]}
errors = []

for name in sorted(packages):
    if name not in allowed:
        errors.append(f"unexpected workspace package: {name}")

for name in sorted(packages):
    for part in forbidden:
        if part in name:
            errors.append(f"forbidden package name: {name}")

for name, package in sorted(packages.items()):
    for target in package["targets"]:
        if "bin" in target["kind"] and target["name"] not in allowed_bins:
            errors.append(f"unexpected binary: {target['name']} ({name})")

if errors:
    print("architecture check failed:", file=sys.stderr)
    for error in errors:
        print(f"  - {error}", file=sys.stderr)
    sys.exit(1)

print(f"architecture ok: {len(packages)} packages, {len(allowed_bins)} binaries")
PY
