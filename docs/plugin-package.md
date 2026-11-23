# Plugin package

A plugin is a Spore package. There is no `plugin.toml`, no `.plugin` archive
format, and no separate install root. The package is:

```text
drawbridge-plugin/
  spore.toml
  module.wasm                 # the wasm adapter/provider
  bin/drawbridge-host         # optional native sidecar
  signatures/<key>.asc
  spore.lock
```

`spore.toml` names the module and, for a native sidecar, the sidecar:

```toml
schema = 1

[spore]
id = "dev.fungos.drawbridge"
name = "drawbridge"
version = "0.1.0"
license = "MIT"
summary = "Drawbridge adapter"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[module]
path = "module.wasm"

[launch]
default = "canvas"
surfaces = ["canvas"]

[plugin]
sidecar = "bin/drawbridge-host"

[grants]
"loam:plugin" = "0.1.0"
```

The adapter exports the capability WIT and imports:

```wit
import loam:plugin/ipc@0.1.0;
```

The sidecar is not the plugin. It is an implementation detail behind the wasm
adapter. The adapter is what consumers link; the sidecar is what the adapter
calls.

## Lock and signature

`spore.lock` covers every package file, including a package-relative sidecar.
A bare sidecar name is resolved on `PATH` and cannot be covered by the lock; use
a package-relative `bin/<name>` for reproducible installs.

```sh
loam plugin pack ./drawbridge-plugin
loam plugin verify ./drawbridge-plugin
```

An unsigned package is refused unless the caller explicitly passes
`--allow-unsigned-dev`:

```sh
loam plugin verify ./drawbridge-plugin --allow-unsigned-dev
loam install ./drawbridge-plugin --allow-unsigned-dev
```

The dev marker is recorded by install. It is a local development affordance, not
a publisher trust decision.

## Install

A Spore package installs into the Spore root by default:

```sh
loam install ./drawbridge-plugin
loam status
```

The visible package is a symlink into `.loam/generations/`. The installer
verifies the manifest and lock, copies the package into staging, resolves
providers, composes resource imports, warms commands, then atomically points the
package name at the new generation.

## Verification checks

`loam plugin verify <dir>` and `loam plugin list` prove:

- a current `spore.lock` exists and matches the package
- every declared package file is sealed
- the module exports at least one non-WASI interface
- a sidecar-backed adapter imports `loam:plugin/ipc@0.1.0`
- a package-relative sidecar exists, is executable, and is in the lock
- exported interface names are the WIT names, including version
- two installed packages do not claim the same interface
- unsigned packages are only accepted with `--allow-unsigned-dev`

`loam plugin list --json` emits interface, provider id/version, package/module
paths, sidecar path, `wasm`/`native-sidecar`, lock digest, and signature/dev
state. `loam plugin inspect <id-or-interface>` prints the same view for one
provider.

## Composition

A consumer imports the exact interface a plugin exports. At install time Loam
matches the consumer's import to the provider's export. A wasm-only provider is
composed into the consumer runtime; a native-sidecar adapter is linked at run
time so its `loam:plugin/ipc` import and sidecar association stay visible:

```text
consumer module --import--> plugin interface
                             plugin module --exports--> plugin interface
                             plugin module --imports--> loam:plugin/ipc
                                                       |
                                                       +--> sidecar
```

If no provider is found, the error names the interface and the roots searched.
If two packages export the same interface, the later one is refused and names
the first package. The composition cache key includes the consumer bytes,
provider bytes, interface identity, and Loam version.

## Rollback and status

`loam rollback <id>` swaps back to the previous generation and re-warms it.
`loam status` reports installs from both default roots and flags drift when the
generation or package link is missing.

## Boundary

```text
exported interface   what the plugin provides
grant                what the plugin may import
consumer grant       what the consumer may call
```

Exporting an interface does not grant the plugin any import. A consumer still
needs the interface in its own grants/requires. These are separate reviews.
