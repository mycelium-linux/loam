# loam architecture

`loam` is the native runtime for Soil components. It is deliberately small: a WebAssembly component host, a capability linker, a command runner, and a plugin registry.

```text
Soil source / tools / Spores
        │
        │ compile to wasm32-wasip2 components
        ▼
loam
  ├─ Wasmtime component engine
  ├─ WASI p2 + wasi:http
  ├─ core native capabilities
  ├─ signed command registry
  └─ plugin registry
```

## The split

```text
soil   language, tools, UI, workflows, Spores
loam   native host capabilities and component execution
```

`loam` does not contain a parser, evaluator, compiler, REPL, LSP, formatter, build tool, UI framework, or product logic.

## Core capabilities

These are linked by the runtime and are part of its stable native surface:

```text
soil:vfs
soil:vfs-mut
soil:records
soil:unibus
soil:principal
soil:creds
soil:index
```

WASI is provided by Wasmtime:

```text
wasi:cli
wasi:clocks
wasi:filesystem
wasi:io
wasi:random
wasi:sockets
wasi:http
```

`wasi:filesystem` is only reachable through host-granted preopens. The workspace capability is `soil:vfs`, not ambient filesystem access.

## First-party plugins

Bundled with `loam`, loaded through the same plugin path as third-party capabilities:

```text
soil:surface
soil:ble
wasi:audio
soil:codec
soil:inference
```

They are native implementations today. They are still plugins: a build without them still provides the core runtime.

## Plugin forms

### Wasm plugin

A component that exports the capability WIT:

```text
guest imports soil:ble@1.2.0
        ↓
loam finds a plugin exporting soil:ble@1.2.0
        ↓
loam instantiates it and links its exports as guest imports
```

The plugin may import a small stable plugin host surface for logging, IPC, clocks, and randomness. It may import core capabilities only if its signed grant allows them.

### Native sidecar

Hardware capabilities use a native process plus a generated wasm adapter:

```text
native BLE service
        ↕ plugin IPC
wasm adapter exporting soil:ble
        ↕ component linking
guest importing soil:ble
```

The adapter is generated from WIT. The native service can be written in any language that can speak the plugin protocol.

## Plugin manifest

```toml
schema = 1

[plugin]
id = "dev.fungos.ble"
version = "0.1.0"
abi = "loam-plugin/1"
provides = ["soil:ble@1.2.0"]
requires = ["loam:plugin/ipc"]

[launch]
mode = "process"          # or "wasm"
command = "loam-ble-host"
transport = "unix"
```

The signed package binds the plugin id, version, module hash, provided interfaces, and requested grants. The runtime refuses duplicate providers, version mismatches, and undeclared imports.

## Linking rules

For each guest component:

1. Read its imported interfaces.
2. Check them against the signed grant.
3. Resolve each import:
   - core linker
   - installed plugin
   - WASI
4. Refuse unknown imports with a named error.
5. Instantiate plugins in dependency order.
6. Link plugin exports to the guest.
7. Run.

There is no compile-time feature list that decides this. The component and the installed registry do.

## Packaging

Commands and Spores are signed packages:

```text
command.toml
command.grant
command.grant.sig
module.wasm
```

`loam-spore` owns the package identity, lock, manifest, and signature checks. `loam-command` owns the installed command registry and the wasm command runner.

## Non-goals

```text
a UI toolkit
a scene graph
a layout engine
a terminal emulator UI
a product-specific adapter
a second compiler
a second runtime for Soil source
```

If it can be Soil source, a Spore, or a plugin, it does not belong in `loam`.
