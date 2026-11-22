# loam capability contracts

This directory contains the WIT contracts the native runtime provides or reserves.

## Core

```text
soil:vfs              read-only workspace namespace
soil:vfs-mut          explicit mutation grant
soil:records          durable schema-tagged records
soil:unibus           bounded host event bus
soil:surface          surface handoff
soil:principal        host-attested identity
soil:creds            host-held signing and verification
soil:index            derived VFS index
```

## First-party plugins

```text
soil:ble              Bluetooth LE host
wasi:audio            draft bounded PCM sink
soil:codec            audio/image codec boundary
soil:mcp              generic MCP catalog and tool invocation
```

## Plugin / extra

Contracts a host or plugin may provide, but `loam` does not link by default:

```text
soil:process          opt-in process spawning with opaque pipe handles
```

They stay out of `loam doctor` and the runner's capability profile. Supplying
one is an explicit host/plugin decision, not an ambient grant.

## Runtime support

```text
soil:command          command package surface
soil:spore            package/catalog surface
soil:time             compatibility clock at the plugin edge
fungos:http-context   host-attested request context for wasi:http
loam:plugin           narrow adapter-to-sidecar IPC
```

## WASI

WASI is provided by Wasmtime, not defined here:

```text
wasi:cli
wasi:clocks
wasi:filesystem
wasi:io
wasi:random
wasi:sockets
wasi:http
```

`wasi:filesystem` is preopen-scoped. Workspace access is `soil:vfs`.

## Removed from the runtime

These are Soil code, Spores, optional third-party plugins, or WASI equivalents:

```text
soil:http, soil:net, soil:stream       -> wasi:http, wasi:sockets, wasi:io
soil:time                              -> wasi:clocks at the core edge
soil:canvas, soil:scene, soil:view     -> UI is Soil
soil:terminal*                         -> terminal Spore/plugin
soil:media, soil:notify, soil:agents   -> first-party or third-party plugins later
soil:appdata, soil:storage             -> vfs/records
soil:search, soil:graph, soil:vfs-cid  -> derived over vfs/records
soil:git, soil:worktrees, soil:desktops -> host/plugin concerns
fungos:actions, fungos:webhooks         -> Spore/plugin
```

Nothing here is an RPC protocol. Typed WIT is the contract; any IPC behind a plugin is an implementation detail.
