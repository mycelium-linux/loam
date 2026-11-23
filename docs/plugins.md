# Plugins 101

See also [`plugin-package.md`](plugin-package.md), [`plugin-ipc.md`](plugin-ipc.md), [`plugin-compat.md`](plugin-compat.md), and [`loamd.md`](loamd.md).

```text
A plugin is a Spore that exports non-WASI WIT.
There is no plugin.toml and no second package format.
A native sidecar is declared with [plugin] sidecar.
```

Loam already has one package format: the Spore. A plugin is the same
`spore.toml` + `module.wasm` + `spore.lock` + `signatures/` package whose module
exports one or more interfaces instead of only consuming them. The registry
indexes those exports; the linker satisfies a consumer's imports with them.

```text
producer Spore                     consumer Spore
  module.wasm                        module.wasm
  exports acme:echo/echo@0.1.0  <--- imports acme:echo/echo@0.1.0
```

## Three separate things

```text
exported interface   what the plugin provides
grant                what the plugin may import
consumer grant       what the consumer may call
```

An export is a declaration, not a grant. A plugin that exports an interface does
not thereby gain any capability. Its own imports are still checked against its
own `[grants]`. A consumer still has to declare or be granted the interface it
calls.

## Complete minimal example

This is a function-only plugin with a native sidecar. The adapter is wasm; the
sidecar is a process behind it.

### Adapter WIT

```wit
package acme:echo@0.1.0;

interface echo {
  answer: func() -> string;
}

world plugin {
  import loam:plugin/ipc@0.1.0;
  export echo;
}
```

The vendored `loam:plugin` WIT is in `crates/loam/wit/plugin/plugin.wit` (or
`crates/loam/fixtures/mcp-adapter/wit/deps/loam-plugin.wit` in the fixture).

### Adapter Cargo.toml

```toml
[package]
name = "acme-echo-adapter"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"
wit-bindgen = "0.57"

[workspace]
```

### Adapter src/lib.rs

```rust
wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use crate::exports::acme::echo::echo::Guest;
use loam::plugin::ipc;

struct Component;

impl Guest for Component {
    fn answer() -> String {
        let reply = ipc::request("answer", b"null").expect("sidecar");
        String::from_utf8(reply).expect("utf-8")
    }
}

export!(Component);
```

### Package

```text
acme-echo/
  spore.toml
  module.wasm
  bin/acme-echo-host
  signatures/local.asc
  spore.lock
```

`spore.toml`:

```toml
schema = 1

[spore]
id = "acme.echo"
name = "echo"
version = "0.1.0"
license = "MIT"
summary = "Echo adapter"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[module]
path = "module.wasm"

[launch]
default = "canvas"
surfaces = ["canvas"]

[plugin]
sidecar = "bin/acme-echo-host"

[grants]
"loam:plugin" = "0.1.0"
```

The sidecar reads one JSON line and writes one JSON line:

```sh
#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"ok":true,"payload":"hello"}'
done
```

Build, seal, and install:

```sh
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/acme_echo_adapter.wasm acme-echo/module.wasm
cp ./acme-echo-host acme-echo/bin/acme-echo-host
chmod 755 acme-echo/bin/acme-echo-host
mkdir -p acme-echo/signatures
printf 'local test seal\n' > acme-echo/signatures/local.asc
loam plugin pack ./acme-echo
loam plugin verify ./acme-echo
loam install ./acme-echo
loam plugin list --json
```

Expected list entry:

```json
{
  "interface": "acme:echo/echo@0.1.0",
  "provider_id": "acme.echo",
  "provider_version": "0.1.0",
  "package": "/home/me/.local/share/fungos/spores/acme-echo",
  "module": "/home/me/.local/share/fungos/spores/acme-echo/module.wasm",
  "sidecar": "/home/me/.local/share/fungos/spores/acme-echo/bin/acme-echo-host",
  "kind": "native-sidecar",
  "lock_digest": "…",
  "signed": true,
  "dev": false
}
```

### Consumer

The consumer's WIT imports the same interface:

```wit
package acme:consumer@0.1.0;

world consumer {
  import acme:echo/echo@0.1.0;
}
```

After `loam install ./acme-consumer`, the installer resolves the provider from
the Spore root. A wasm-only provider is composed into the consumer runtime; a
native-sidecar adapter stays hot-linked at runtime so the consumer keeps the
import and Loam can find the adapter's sidecar. `loam run acme-consumer` then
calls:

```text
consumer -> acme:echo/echo@0.1.0 -> adapter -> loam:plugin/ipc -> sidecar
```

## Discovery and install roots

- Default plugin install root: the Spore root, `.../fungos/spores`
- Default command install root: `.../fungos/commands`
- `loam install ./pkg` infers the root from the manifest
- `--root DIR` overrides it
- `--provider-root DIR` overrides provider discovery
- `loam status` reads both default roots and reports drift
- `loam plugin list|verify|inspect` use the same roots

## Sidecar lifecycle

Loam owns the process, not the adapter:

```text
Loam/loamd owns the command process
  └── command runner owns one sidecar for this run
        └── adapter talks only through loam:plugin/ipc
```

The sidecar's working directory is the plugin package. Its stdin/stdout are
pipes; there is no socket. It inherits the Loam process environment, but the
adapter cannot read that environment. The child is killed and reaped when the
run ends. A missing sidecar is refused at verification; a died or EOF sidecar
maps to `unavailable`.

## Rollback and drift

`loam rollback <id>` re-warms and swaps back to the previous generation.
`loam status` checks that the active generation, package link, and composed
runtime still exist; missing pieces appear as drift.

## Supported today, and not yet

```text
supported     function exports, nested instances, wasm providers,
              install-time composition, sidecar-backed adapters
not yet       async exports, resource exports hot-linked by loam run,
              sidecar pooling, remote plugin store
```

Resource imports should be composed at install time. Function-only is the first
milestone on purpose.
