# Writing a Rust WIT plugin for loam

A loam plugin is a Spore that **exports** one or more WIT interfaces. There is
no `plugin.toml` and no second package format. The same `spore.toml`,
`spore.lock`, signature and install roots cover plugins, commands and services.

This guide builds a small function-only plugin. Function-only is the safest
first target: it links directly today. Resource exports are covered separately
at the end.

## 1. Write the WIT contract

Create a crate and a WIT package:

```text
acme-font/
  Cargo.toml
  src/lib.rs
  wit/plugin.wit
```

`wit/plugin.wit`:

```wit
package acme:fonts@0.1.0;

interface fonts {
  record face {
    family: string,
    weight: u16,
  }

  list-faces: func() -> list<face>;
}

world plugin {
  export fonts;
}
```

The exported interface name is the key another Spore imports:

```text
acme:fonts/fonts@0.1.0
```

Exports are declarations, not grants. The provider does not need a `[grants]`
entry for what it provides. It only needs grants for what it imports.

## 2. Write the Rust component

`Cargo.toml`:

```toml
[package]
name = "acme-font"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.57"

[workspace]
```

`src/lib.rs`:

```rust
wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use crate::exports::acme::fonts::fonts::{Face, Guest};

struct Component;

impl Guest for Component {
    fn list_faces() -> Vec<Face> {
        vec![
            Face {
                family: "Berkeley Mono".to_string(),
                weight: 400,
            },
            Face {
                family: "Berkeley Mono".to_string(),
                weight: 700,
            },
        ]
    }
}

export!(Component);
```

`crate-type = ["cdylib"]` is required. `wasm32-wasip2` produces a component,
not a core module. `loam` executes components.

Build it:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

The artifact is:

```text
target/wasm32-wasip2/release/acme_font.wasm
```

Inspect what the component exports:

```sh
wasm-tools component wit \
  target/wasm32-wasip2/release/acme_font.wasm
```

You should see:

```text
export acme:fonts/fonts@0.1.0;
```

## 3. Make the Spore package

Create the package directory:

```text
acme-font-plugin/
  spore.toml
  module.wasm
  signatures/
```

Copy the built component:

```sh
cp target/wasm32-wasip2/release/acme_font.wasm \
  acme-font-plugin/module.wasm
```

`spore.toml`:

```toml
schema = 1

[spore]
id = "acme.fonts"
name = "fonts"
version = "0.1.0"
license = "MIT"
summary = "A tiny font-list plugin"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[module]
path = "module.wasm"

[launch]
default = "canvas"
surfaces = ["canvas"]
```

There is no `[plugin]` table for a wasm-only plugin. `[plugin] sidecar` is only
for a native sidecar behind a wasm adapter.

If the plugin imports a core capability, declare it honestly:

```toml
[grants]
"soil:records" = "read the font catalog from records"
```

The provider's imports are still bounded by those grants. Exporting an
interface does not widen them.

## 4. Seal the package

Generate the lock:

```sh
loam plugin pack acme-font-plugin
```

This writes `spore.lock`. The lock is the package identity; it covers the
manifest and module bytes.

For a local test seal:

```sh
mkdir -p acme-font-plugin/signatures
printf 'local test seal\n' > acme-font-plugin/signatures/local.asc
loam plugin pack acme-font-plugin
loam plugin verify --root acme-font-plugin/..
```

The local seal is enough for local plugin discovery, but it is **not a trusted
publisher signature**. Real publication still requires the publisher signature
path. Do not ship `local.asc`.

Install it into a plugin root:

```sh
mkdir -p ~/.local/share/fungos/spores
cp -R acme-font-plugin ~/.local/share/fungos/spores/fonts
loam plugin list
```

Expected row:

```text
acme:fonts/fonts@0.1.0    acme.fonts    0.1.0    ~/.local/share/fungos/spores/fonts
```

`loam plugin verify` should report no refusals. If it does, the reason is
usually a missing lock, a missing seal, or a module that does not export the
interface named in the manifest.

## 5. Consume it from another component

A consumer imports the interface in its own WIT world:

```wit
package acme:reader@0.1.0;

world reader {
  import acme:fonts/fonts@0.1.0;
}
```

On the Rust side, `wit-bindgen` generates the import module:

```rust
use acme::fonts::fonts::list_faces;

let faces = list_faces();
```

When the consumer runs under loam, the plugin registry:

1. reads the consumer's imported interfaces
2. finds installed providers that export them
3. refuses duplicate providers
4. instantiates each provider once
5. proxies its exports into the consumer's linker

If no provider is installed, the consumer fails at link/instantiation rather
than silently falling back to empty data.

For a command package that imports a provider, the installer can bake the
provider into the command:

```sh
loam install my-command \
  --root ~/.local/share/fungos/commands \
  --provider-root ~/.local/share/fungos/spores \
  --allow-unsigned-dev
```

The installed command gets `runtime.wasm` and `runtime.key`, and `loam run`
loads the composed runtime instead of the original module. `--allow-unsigned-dev`
is only for local work; a normal install verifies the package signature.

## 6. What is supported now

Working today:

```text
function exports
nested component instances
wasm providers indexed from installed Spore roots
provider imports bounded by the provider's own grants
install-time composition for resource imports
```

Limits:

```text
async exports are refused by the runtime linker
resource exports are not proxied by the runtime linker
resource imports should be composed at install time with loam install
native sidecars are a separate trust class
```

For a resource-heavy contract such as `soil:process`, `soil:http` or
`soil:streams`, prefer install-time composition and test the composed runtime.
Do not assume a resource provider can be hot-linked by `loam run` yet.

## 7. Sidecar-backed plugins

If the real implementation cannot be wasm — a native service, a hardware
integration, or an external process — keep it behind a wasm adapter:

```text
consumer imports soil:mcp
        ↓
wasm adapter exports soil:mcp
adapter imports loam:plugin/ipc
        ↓
native sidecar (loam-mcp)
```

The adapter is still a Spore. It still has grants. The sidecar runs with the
user's privileges and must be installed as a trusted native package. The
complete reference is:

- `crates/loam/fixtures/mcp-adapter/`
- `crates/loam/fixtures/mcp-package/spore.toml`
- `scripts/build-mcp-plugin.sh`

## Checklist

```text
[ ] WIT interface under wit/
[ ] world exports the interface
[ ] cdylib crate, wit-bindgen generate!
[ ] cargo build --target wasm32-wasip2 --release
[ ] wasm-tools component wit shows the export
[ ] spore.toml has module path and launch block
[ ] loam plugin pack writes spore.lock
[ ] signatures/ exists for local discovery
[ ] loam plugin verify --root <parent> passes
[ ] consumer imports the exact interface name
[ ] install/test with a provider root
```
