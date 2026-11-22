# loam

**loam is the native runtime for Soil components.**

It is the host side of the Soil system:

```text
soil   the language, tools and Spores
loam   the native runtime that runs them
```

A Soil program compiles to a WebAssembly component. `loam` executes that component, provides the host capabilities it was granted, and enforces the signed grant boundary.

There is one command:

```sh
loam run NAME [ARGS...]
loam surface FILE [--size WxH]
```

`loam` is not the compiler, the REPL, the formatter, the LSP, the package authoring tool, or the UI framework. Those live in the Soil repository.

## What belongs here

First-party runtime capabilities:

| Capability | WIT | Status |
|---|---|---|
| Workspace namespace | `soil:vfs` | native |
| Workspace mutation | `soil:vfs-mut` | native |
| Durable records | `soil:records` | native |
| Host event bus | `soil:unibus` | native |
| Surface handoff | `soil:surface` | native |
| Identity | `soil:principal` | native |
| Host-held credentials | `soil:creds` | native |
| Derived VFS index | `soil:index` | native |

First-party native plugins:

| Capability | Crates | Status |
|---|---|---|
| Surface | `loam-surface` | native |
| BLE | `loam-ble-api`, `loam-ble-host`, `loam-ble-sim` | native |
| Audio + codec | `loam-media` | native |

`soil:process` is a plugin/extra contract in `wit/`. `soil:http` and
`soil:streams` already live with their opt-in host implementations under
`crates/loam/wit/`. None of them are linked by the default runner or advertised
in `loam doctor`.

The runtime also provides WASI p2 and `wasi:http` through Wasmtime:

```text
wasi:cli
wasi:clocks
wasi:filesystem
wasi:io
wasi:random
wasi:sockets
wasi:http
```

## What does not belong here

```text
layout, UI, scene graphs, canvas ownership
terminal UI, media browser, notification policy
Marbles, CRM, projects, tasks, agents
LSP, formatter, linter, docs renderer, build tool
the Soil reader, evaluator, compiler or REPL
product workflows and provider integrations
```

Those are Soil code, Spores, or plugins.

## Commands

```sh
loam run NAME [ARGS...]        # run an installed wasm command
loam surface FILE [--size WxH] # open a native winit + WebGPU surface
loam edit <op> ... [--json]    # edit a personal source overlay
loam doctor                    # print the linked capability profile
loam help
```

Command packages live in the usual roots:

```text
~/.local/share/fungos/commands/
~/.local/share/fungos/spores/
```

`loam run` is the virtual PATH. It does not compile or interpret Soil source.

## Editing

A signed Spore is immutable. `loam edit` copies its `source/` into a personal
session, checkpoints that work tree, saves full-file overlays, and applies them
under the personal patch root:

```sh
loam edit open spores/marbles --json
loam edit read acme:marbles/<base-digest> mb.soil --json
loam edit list acme:marbles/<base-digest> --json
loam edit checkpoint acme:marbles/<base-digest> --message "first change" --json
loam edit save acme:marbles/<base-digest> --message "overlay" --json
loam edit apply acme:marbles/<base-digest> --json
```

Every command writes JSON to stdout. `build` is unsupported until a builder is
configured; source-only overlays can still be saved. The signed package is never
written to.

## Plugins

`loam` is a linker. Core capabilities are linked natively. Everything else is discovered at runtime.

**A plugin is a Spore that exports WIT.** There is no second package format:

```text
Spore = wasm component + spore.toml + spore.lock + signatures
grants constrain imports
exports are what the Spore provides
```

A Spore that exports `run` is a command. One that exports `wasi:http/incoming-handler` is a service. One that exports `soil:mcp` is a capability plugin.

```sh
loam plugin list       # installed exports
loam plugin verify     # refused packages, duplicate providers, missing seals
loam plugin pack DIR   # write spore.lock for a local package
```

Wasm plugins are linked directly. A native sidecar is a different trust class
behind the same interface: the package ships a wasm adapter and a native
executable, and the adapter reaches it through `loam:plugin/ipc`.

```text
guest imports soil:mcp
        ↓
adapter Spore exports soil:mcp
adapter imports loam:plugin/ipc
        ↓
native sidecar (loam-mcp)
```

Exports are declarations, not grants. A provider's own imports are still
bounded by its `[grants]`, and a consumer still needs the capability in its own
`[grants]`.

See [`docs/architecture.md`](docs/architecture.md).

## Repository layout

```text
crates/loam              runtime, host, command runner, plugin linker, Spore package
crates/loam-surface      winit + WebGPU surface host
crates/loam-ble-*        first-party BLE plugin
crates/loam-media        first-party audio + codec plugin
wit/                     canonical capability contracts
```

## Install

```sh
# release tarball, shell installer or Homebrew once published
loam doctor
```

See [`docs/install.md`](docs/install.md).

## Build

```sh
cargo check --workspace
cargo test --workspace
cargo build -p loam --features surface --bin loam
scripts/check-architecture.sh
scripts/check-wit.sh
scripts/check-surface-fixture.sh
scripts/build-mcp-plugin.sh   # build the first real plugin package
```

The runnable binary is `target/debug/loam`.

Project rules are in [`AGENTS.md`](AGENTS.md) and
[`CONTRIBUTING.md`](CONTRIBUTING.md). Agent discovery starts at
[`llms.txt`](llms.txt).

## Principles

- **No ambient authority.** A component reaches exactly what its signed grant names.
- **One binary.** `loam` is the native front door; plugins are packages, not host forks.
- **Wasmtime is the engine.** WASI is provided, not reimplemented.
- **Soil is the userland.** Runtime code stays at the host boundary.
- **Delete before abstracting.** A capability that can be Soil source should not become a Rust crate.
