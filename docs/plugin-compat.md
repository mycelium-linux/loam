# Plugin compatibility

The package is a Spore; this table tracks the contract versions that matter when
a plugin, its adapter, and its sidecar are upgraded independently.

| Surface | Current version | Source of truth |
|---|---|---|
| Loam package schema | `1` | `spore.toml` `schema = 1` |
| Spore lock | `spore-lock 1` | `spore.lock` header |
| Plugin IPC WIT | `loam:plugin/ipc@0.1.0` | `crates/loam/wit/plugin/plugin.wit` |
| Plugin IPC framing | newline-delimited JSON | `docs/plugin-ipc.md` |
| IPC canonical field | `payload` | `docs/plugin-ipc.md` |
| IPC request alias | `params` | compatibility only |
| IPC response alias | `result` | compatibility only |
| WIT target | `wasm32-wasip2` component | `spore.toml` `[engine]` |
| Sidecar protocol | `loam:plugin/ipc@0.1.0` | adapter import |
| Sidecar version | package-defined | `spore.toml` `[spore] version` |
| Adapter version | package-defined | `spore.toml` `[spore] version` |

Rules:

- A provider's exported interface name and version must match the consumer's
  import exactly.
- A native-sidecar adapter must import `loam:plugin/ipc@0.1.0`.
- A sidecar that only understands `params`/`result` remains compatible with
  `payload`/`result` for this protocol version.
- Changing the IPC WIT version requires a new package version and a new
  compatibility row; do not silently change the meaning of `0.1.0`.

Example upgrade matrix:

| WIT | Adapter | Sidecar | Result |
|---|---|---|---|
| `0.1.0` | 1.2.0 | 1.2.0 | compatible |
| `0.1.0` | 1.2.0 | 1.0.0 | compatible if it reads `payload` or `params` |
| `0.2.0` | 1.2.0 | 1.2.0 | refused: adapter imports an older IPC version |
