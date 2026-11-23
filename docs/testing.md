# Testing

## Fast gate

```sh
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Runtime gate

```sh
cargo test --workspace
scripts/check-architecture.sh
scripts/check-wit.sh
```

The architecture gate enforces the crate and binary allowlist. The WIT gate
parses every host world that consumes the root contracts.

## Surface gate

```sh
scripts/check-surface-fixture.sh
```

This builds the RGBA fixture as `wasm32-wasip2`, runs:

```sh
loam surface fixture.wasm --size 64x64 --capture out.rgba
```

and checks the captured frame byte-for-byte for size, corner and bar order.

## Plugin gate

```sh
scripts/build-mcp-plugin.sh
loam plugin list --root target
loam plugin verify --root target
```

The script builds:

- the `loam-mcp` stdio sidecar
- the `soil:mcp` wasm adapter
- a sealed local test package under `target/loam-mcp-plugin`

The printer at the end names the package directory. Copy it under
`~/.local/share/fungos/spores/` to test it through the default roots.

## Audio

```sh
cargo test -p loam-media
```

The packet/isochrone test runs in CI. The real-device tone test is ignored unless
an audio output device is available.

## What needs hardware

- real BLE radio
- real audio output
- a GPU adapter for a live `wasi:webgpu` surface
- a windowing session for winit

CI still proves the protocol and non-device paths.
