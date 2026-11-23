# Image host plan (Loam slice 1)

An Image Spore is a live Soil image. Loam does not mutate it. This slice turns a
Spore's grants and installed providers into an inspectable host plan, and makes
the runtime link only the interfaces those grants cover.

```sh
loam image plan ./my-spore
loam image plan ./my-spore --root ~/.local/share/fungos/spores --json
```

## Input

Loam consumes the normalized image data Soil/Tend produces:

```text
spore id / version
grant set
imported WIT interfaces (when the package has a module)
installed providers
```

A module is not required. If there is no module, an exact grant naming an
installed provider interface is a candidate.

## Plan

```json
{
  "spore_id": "test.image",
  "spore_version": "0.1.0",
  "package": "/…/image",
  "grants": { "soil:echo": "echo" },
  "imports": [
    {
      "interface": "soil:echo/echo@0.1.0",
      "grant": "soil:echo",
      "provider": {
        "id": "test.echo",
        "version": "0.1.0",
        "package": "/…/provider",
        "module": "/…/provider/module.wasm",
        "module_sha256": "…",
        "lock_digest": "…",
        "sidecar": null,
        "sidecar_sha256": null
      }
    }
  ],
  "refused": []
}
```

Refusals are explicit:

```text
no covering [grants] entry
no provider for <interface>
```

## Linking rule

```text
grant exists  -> provider linked
grant missing -> binding absent
```

`PluginRegistry::link_allowed` is the enforcement point. The command runner
passes the entry's `requires`/grants; an installed but ungranted provider never
enters the linker. This is why a standalone command must declare
`requires = ["soil:mcp"]` (or the equivalent Spore grant) before an MCP adapter
can be linked.

The plan is not generated Rust and not a bytecode format. It is the set of
typed WIT imports Loam may satisfy for the image runtime.

## Not in this slice

```text
tend.soil parser (Soil owns it)
image start/eval/status/stop in loamd
snapshot/restore/reload
REPL attach policy
generation/rollback pointers
```

Those need the Soil image runtime command and its snapshot/restore protocol.
Loam should call `image/snapshot -> bytes` and `image/restore(bytes)` rather
than interpret the image itself.
