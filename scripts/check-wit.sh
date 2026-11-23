#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

if ! command -v wasm-tools >/dev/null 2>&1; then
    echo "check-wit: wasm-tools is required" >&2
    exit 1
fi

# Each host world owns its WIT graph. Parse the worlds that use the root
# contracts, not the root directory as one package: the root is a collection.
for world in \
    wit/soil-mcp.wit \
    wit/soil-process.wit \
    wit/soil-inference.wit \
    wit/loam-plugin.wit \
    crates/loam/wit/principal \
    crates/loam/wit/vfs \
    crates/loam/wit/http \
    crates/loam/wit/stream \
    crates/loam-surface/wit \
    crates/loam-ble-host/wit \
    crates/loam-media/wit \
    crates/loam/wit/plugin \
    crates/loam/fixtures/mcp-adapter/wit \
    wit/soil-command
do
    wasm-tools component wit "$world" >/dev/null
    echo "wit ok: $world"
done
