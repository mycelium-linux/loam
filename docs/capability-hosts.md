# Capability hosts

A capability reaches a Spore in two halves that ship separately:

- the **contract**, `wit/<package>.wit` — what a Spore imports;
- the **host**, a module in `crates/soil-host/src/` — what a host links.

The contract is a `.wit` file and nothing else. The host implementation owns the
wire to a real deployment (a CLI, a socket, a store) and never appears in the
contract's dependency graph. A host that links the adapter does not link
Mycelium, Unibus or whatever it fronts; a Spore that imports the interface does
not link Soil.

## One host

`soil-host` is the host. One module per contract: `records_host` serves
`soil:records`, `http_host` serves `soil:http`, `vfs_host` serves `soil:vfs`,
`unibus` serves `soil:unibus`, `mycelium` serves `fungos:mycelium/fleet`. The
`wasi` module is the guest profile that links them into one `WasiHost`.

They live in one crate because they are one host. A crate per capability only
bought a `Cargo.toml`, a duplicated error type and a binary per experiment — and
the capability it hosted still could not be reused without dragging its own
build graph along.

## Adding a capability

1. Append the interface to `wit/<package>.wit`. It takes the shape of the stable
   CLI/JSON contract the service already publishes; it does not mirror internal
   types.
2. Add `crates/soil-host/src/<name>.rs` with the bindgen (`path: "../../wit/…"`),
   the adapter type and `add_to_linker`.
3. Add a feature to `crates/soil-host/Cargo.toml` for anything that fronts an
   external deployment, and gate the module:

   ```toml
   mycelium = []
   unibus = ["dep:unibus", "dep:unibus-sdk"]
   ```

4. Wire it into the profile: `wasi/mod.rs` re-exports it, `wasi/service.rs` links
   it behind the flag, and `wasi/host.rs` implements the generated `Host` trait
   by handing the adapter the store state it needs.
5. A product opts in with its own default features (`soil-shell` does), so the
   library never chooses a deployment on its behalf.

A Spore declares what it needs in its manifest (`abi = { mycelium = "0.1.0" }`)
and asks for the grant in `[grants]`. Spores vendor the `.wit`; they never link
the host.

## The flag policy

`soil-host`'s defaults are `model2vec` only. The capabilities that reach an
external deployment — `mycelium`, `unibus` — are features, on by default in the
product that ships them (`soil-shell`), off in a slim build:

```sh
cargo build -p soil-shell --no-default-features
```

A product that turns a capability off must not silently lose the feature it
backs: the host answers `unavailable` (or refuses the import at instantiation),
and the Spore shows that, rather than drawing an empty state that looks like a
healthy but idle system. `soil spore run` refuses a Spore that grants
`soil:unibus` when the build has no bus.

## Today

| module | contract | flag |
| --- | --- | --- |
| `records_host` | `soil:records` | always (likely fundamental) |
| `http_host` | `soil:http` | always |
| `vfs_host` | `soil:vfs`, `soil:vfs-mut`, `soil:index` | always (fundamental) |
| `wasi` | the guest profile + linker | always |
| `mycelium` | `fungos:mycelium/fleet` | `mycelium` (default in `soil-shell`) |
| `unibus` | `soil:unibus` | `unibus` (default in `soil-shell`) |

## Missing adapters

Absent is not empty. `MyceliumFleet::status` returns `unavailable` when the CLI
is missing, non-zero or slower than its timeout; it never returns an empty peer
list, because an empty list is a fleet claim. A component that imports
`fungos:mycelium/fleet` while the interface is unlinked fails at instantiation,
naming the import.

## Decided

- **VFS is fundamental.** A Spore has a home directory; `fungos-vfs` and
  `vfs_host` are always linked and never behind a flag. That is what a Spore
  *is*, not something a product opts into. `model2vec` is not part of that:
  semantic search over the VFS is an index backend a product may leave out.
- **Records is likely the same, not decided.** Until it is, `records_host` stays
  linked unconditionally rather than behind a flag that would probably be
  removed. If records also becomes fundamental, the capability set settles at:
  VFS, records and HTTP always; mycelium and unibus opt-in.

## Open

- **Records.** Fundamental like VFS, or the first contract a product can leave
  out? Everything downstream assumes it is there today; the decision would
  change how `soil:records` consumers are written, not just the graph.
- **The base `unibus` crate arrives through `fungos-records`.** Gating the
  adapter removes the adapter and `unibus-sdk`, not the client crate the records
  store links. With VFS fundamental and records likely so, that is the expected
  graph rather than a leak: a slim build still contains records, and records
  speaks unibus.
- **OS native (mic, audio, surface).** These belong to the slim build, so they
  are not deployment-specific capabilities: they are the base layer every
  profile keeps. `wasi-audio-wasmtime` exists and is linked by no profile yet;
  `soil:mic` is a row in the ABI map with no `.wit` file.
