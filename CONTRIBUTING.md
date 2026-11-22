# Contributing to loam

## What this repository is

`loam` is the native runtime for Soil components.

It owns:

```text
Wasmtime execution
WASI p2 / wasi:http
core host capabilities
command packages
surface presentation
first-party native plugins
```

It does not own the Soil language, compiler, REPL, LSP, formatter, build tool,
UI framework, or product workflows.

## Before opening a PR

```sh
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-architecture.sh
scripts/check-wit.sh
scripts/check-surface-fixture.sh
```

## Rules

- Keep one `loam` binary. Plugin sidecars are explicit architecture exceptions.
- Do not add a product crate.
- Do not add a capability that WASI already provides.
- Do not add a WIT without a host implementation or a signed plugin plan.
- Do not widen the core linker silently.
- Keep docs and the architecture allowlist in the same PR as the change.

## Commit style

Use a scoped subject:

```text
loam-surface: add headless capture
loam-ble: merge api, sim, and host
ci: add WIT and architecture gates
```

## Review checklist

- Does this belong in `loam`, Soil, a Spore, or a plugin?
- Does it add a binary?
- Does it add a WIT?
- Does it add ambient authority?
- Does it make the runtime harder to build or reason about?
