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

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/mycelium-linux/loam/releases/latest/download/loam-installer.sh | sh
loam doctor
```

The installer puts `loam`, `loamd`, `loam-mcp`, `loam-ble-host`, the WIT
contracts, and the man pages under `~/.local`. Use `--prefix DIR` or
`LOAM_VERSION=v0.1.0` for a different location or release.

Release tarballs are published for macOS arm64/x86_64 and Linux
arm64/x86_64. See [`docs/install.md`](docs/install.md).

## First run

```sh
loam status
loam doctor
man loam
man loamd
```

`loam status` reads the default command and Spore roots. `loam doctor` prints
the capabilities linked into this build. `loam help` lists the subcommands.

## Soil 101

A Soil form is data, and a Spore is a package around a component. The smallest
useful program is a function:

```soil
(defun greet (name)
  (str "Hello, " name "!"))
```

A package that should appear at `hello.myc` declares a web surface and an
anycast claim:

```toml
[launch]
default = "web"
surfaces = ["web"]

[[surfaces]]
name = "hello"
kind = "web"
entry = "web/index.html"

[anycast]
name = "hello"
kind = "web"
alpns = ["soil/http/1"]
ttl_secs = 240
```

The complete runnable example is
[`examples/hello-soil`](examples/hello-soil). It is a static web Spore today;
the manifest shape is the same one a compiled Soil component uses when the
compiler can lower a real Spore.

```sh
cp -R examples/hello-soil ~/.mycelium/spores/
loamd serve --root ~/.mycelium/spores --web-listen 127.0.0.1:8799
# then, through the Mycelium frontdoor:
curl -k https://hello.myc/
```

See [`docs/soil-101.md`](docs/soil-101.md) for the longer explanation and the
404/TLS checklist.

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
loam install PATH [--root DIR] # verify, compose, warm, install atomically
loam status [--root DIR]... [--json]
loam rollback ID [--root DIR]
loam surface FILE [--size WxH] # open a native winit + WebGPU surface
loam edit <op> ... [--json]    # edit a personal source overlay
loam doctor                    # print the linked capability profile
loam help
```

`loam install` is local-only and source-preserving. It keeps generations and a
previous digest for rollback:

```sh
loam install spores/marbles
loam status --json
loam rollback acme:marbles
```

See [`docs/installer.md`](docs/installer.md).

`loamd` is the per-user supervisor for command instances:

```sh
loamd
loamd start mb --id mb-1 -- ready --json
loamd list
loamd logs mb-1
loamd stop mb-1
```

It listens on `$XDG_RUNTIME_DIR/loam/loamd.sock` and accepts only peers with the
daemon's Unix uid. Host defaults can live in `~/.config/loam/loam.conf.soil`;
logs and instances publish their durable state under the per-user state
directory. See [`docs/loamd.md`](docs/loamd.md).

Command packages live in the usual roots:

```text
~/.local/share/fungos/commands/
~/.local/share/fungos/spores/
```

`loam run` is the virtual PATH. It does not compile or interpret Soil source.

Manual pages are installed with the release:

```sh
man loam
man loamd
```

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

For a worked Rust example, see [`docs/rust-plugin.md`](docs/rust-plugin.md).

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

See [`docs/install.md`](docs/install.md) for the shell installer, tarball
checksums, platform matrix, and source build.

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
