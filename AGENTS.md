# loam agent instructions

Read `docs/architecture.md` before changing runtime code.

## Boundaries

- `loam` is the native runtime. `soil` is the language, tools, and Spores.
- One binary: `loam`.
- Core: `soil:vfs`, `soil:vfs-mut`, `soil:records`, `soil:unibus`,
  `soil:surface`, `soil:principal`, `soil:creds`, `soil:index`.
- First-party plugins: `loam-surface`, `loam-ble`, `loam-media`, `loam-mcp`.
- `soil:inference` is a wasm plugin contract, not a core host implementation.
- Everything else belongs in Soil, a Spore, or an external plugin.

## Do not add

- a parser, compiler, REPL, LSP, formatter, or build tool
- UI, layout, scene, canvas, terminal, media browser, or product crates
- agent host, CRM, Marbles, provider integrations, or task workflows
- a second binary without updating the architecture allowlist
- a new `soil-*` Rust package

## Rules

- No new WIT without updating `docs/architecture.md`, `wit/README.md`, and the
  architecture allowlist.
- No dependency without a capability reason.
- Do not reintroduce deleted crates from `archive/pre-loam` without a port plan.
- A plugin is a Spore that exports WIT; there is no `plugin.toml`.
- Native hardware goes behind a first-party plugin or sidecar, not the kernel.
- Keep the plugin boundary real: linking is not granting. Exports are
  declarations, not grants.

## Verify

```sh
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/check-architecture.sh
scripts/check-wit.sh
scripts/check-surface-fixture.sh
scripts/build-mcp-plugin.sh
```
