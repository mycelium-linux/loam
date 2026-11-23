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
```

They are native implementations today. They are still plugins: a build without them still provides the core runtime.

Optional plugin contracts are not linked by the runtime:

```text
soil:process          wit/soil-process.wit
soil:http             crates/loam/wit/http
soil:streams          crates/loam/wit/stream
```

`soil:process` is the opt-in process/pipe contract, `soil:http` is bounded
host-mediated HTTP, and `soil:streams` is the host-accepted byte-stream queue.
The HTTP and stream contracts stay beside their host implementations rather than
being duplicated at the root. A host that wants one links it explicitly; the
default `loam run` profile does not expose them.

## Plugin forms

A plugin is a role, not a package type. It is a Spore that exports one or more
capability interfaces. The same manifest, lock and signature cover it.

### Wasm plugin

A component that exports the capability WIT:

```text
guest imports soil:mcp/mcp@0.1.0
        ↓
loam indexes a Spore exporting soil:mcp/mcp@0.1.0
        ↓
loam instantiates the provider and links its exports
```

The provider may import a small stable host surface for IPC. It may import core
capabilities only if its own signed grant allows them.

### Native sidecar

A native implementation stays behind a wasm adapter:

```text
native MCP service
        ↕ loam:plugin/ipc (line-delimited JSON)
wasm adapter exporting soil:mcp
        ↕ component linking
guest importing soil:mcp
```

The adapter is still a Spore and still has grants. The sidecar is not a Spore:
it runs with the user's privileges and must be installed as a trusted native
package. The package format is shared; the execution model is not.

## Plugin manifest

There is no `plugin.toml`. A plugin Spore uses the normal `spore.toml`:

```toml
schema = 1

[spore]
id = "dev.fungos.mcp"
name = "mcp"
version = "0.1.0"
license = "MIT"
summary = "MCP capability adapter"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[module]
path = "module.wasm"

[launch]
default = "canvas"
surfaces = ["canvas"]

[plugin]
sidecar = "bin/loam-mcp"    # optional; relative to the package

[grants]
"loam:plugin" = "0.1.0"
```

Exports are read from the module, not copied into the manifest. `spore.lock`
covers the module and sidecar bytes. The runtime refuses duplicate providers,
unsealed packages, and undeclared imports.

## Linking rules

For each guest component:

1. Read its imported interfaces.
2. Check them against the signed grant.
3. Resolve each import:
   - core linker
   - WASI
   - an installed Spore export
4. Refuse duplicate providers and unknown imports.
5. Instantiate providers in dependency order.
6. Link provider exports to the guest.
7. Run.

There is no compile-time feature list that decides this. The component and the
installed registry do. **Exports are declarations, not grants.**

## Packaging

Commands and Spores are signed packages:

```text
command.toml
command.grant
command.grant.sig
module.wasm
```

`loam::spore` owns package identity, lock, manifest, and signature checks. `loam::command` owns the installed command registry and the wasm command runner.

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
