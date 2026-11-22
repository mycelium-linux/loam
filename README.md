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
| Audio | `loam-audio` | native |
| Codec | `loam-codec` | native |
| Inference | `loam-inference`, `loam-inference-host` | native |
| Text scrubbing | `loam-scrub` | internal to inference |

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
loam doctor                    # print the linked capability profile
loam help
```

Command packages live in the usual roots:

```text
~/.local/share/fungos/commands/
~/.local/share/fungos/spores/
```

`loam run` is the virtual PATH. It does not compile or interpret Soil source.

## Plugins

`loam` is a linker. Core capabilities are linked natively. Everything else is discovered at runtime.

Two plugin forms:

1. **Wasm plugin** — a component exporting the capability WIT. `loam` links its exports directly.
2. **Native sidecar** — a native process plus a generated wasm adapter that exports the same WIT and forwards over `loam:plugin/ipc`.

No `loam` rebuild is needed to add a capability. The plugin package is signed and declares the WIT interfaces it provides and the grants it requires.

See [`docs/architecture.md`](docs/architecture.md).

## Repository layout

```text
crates/loam              runtime host and linker
crates/loam-cli          the one `loam` binary
crates/loam-command      command registry and wasm command runner
crates/loam-spore        package identity, locks, signatures
crates/loam-surface      winit + WebGPU surface host
crates/loam-ble-*        first-party BLE plugin
crates/loam-audio        first-party audio plugin
crates/loam-codec        first-party codec plugin
crates/loam-inference*   first-party inference plugin
crates/loam-scrub        internal inference helper
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
cargo build -p loam-cli
scripts/check-architecture.sh
scripts/check-wit.sh
scripts/check-surface-fixture.sh
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
