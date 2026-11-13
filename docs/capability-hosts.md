# Capability hosts

A fungOS capability reaches a Spore in two halves that ship separately:

- the **contract**, `wit/<package>.wit` — what a Spore imports;
- the **host adapter**, `crates/<package>-host` — what a host links.

The contract is a `.wit` file and nothing else. The adapter owns the wire to a
real deployment (a CLI, a socket, a store) and never appears in the contract's
dependency graph. A host that links the adapter does not link Mycelium, Unibus or
whatever it fronts; a Spore that imports the interface does not link Soil.

`fungos:mycelium/fleet` is the first one: `wit/fungos-mycelium.wit` plus
`crates/soil-mycelium-host`, which calls `mycelium fleet status --json` and
`mycelium topology --json` and returns a bounded projection — peers and four
topology counts, never the topology document.

## Why not a path dependency on the service

Because a product that ships without the service must still build. A slim app is
the nice canvas, a Spore, and the OS pieces it needs (mic, audio, surface);
linking `mycelium`, `unibus` and a mesh runtime into it to keep one optional
panel working is the sprawl this pattern exists to avoid. The adapter is the
join point: one crate per capability, opt-in per host.

## The shape

1. Append the interface to `wit/<package>.wit`. It takes the shape of the stable
   CLI/JSON contract the service already publishes; it does not mirror internal
   types.
2. Add `crates/<package>-host` with `Cargo.toml`, `src/lib.rs` (bindgen, the
   adapter type, `add_to_linker`) and `wit/` symlinked to the repository `wit/`,
   so the crate and the contract cannot drift.
3. Add a feature to every host profile that may link it:

   ```toml
   [features]
   default = ["mycelium"]
   mycelium = ["dep:soil-mycelium-host"]
   ```

   and gate the `add_to_linker` call and the store field behind it.
4. A Spore declares what it needs in its manifest (`abi = { mycelium = "0.1.0" }`)
   and asks for the grant in `[grants]`. Spores vendor the `.wit`; they never
   link the adapter.

## The flag policy

Capabilities are **on by default** and off with `--no-default-features`. The
flag lives on the host profile (`soil-wasi-host`, `soil-canvas-host`, …) and is
forwarded by the product crate that embeds it, so a slim build is one choice at
the top, not a matrix at the bottom:

```sh
cargo check -p soil-shell --no-default-features
```

A product that turns a capability off must not silently lose the feature it
backs: the host answers `unavailable` (or refuses the import at instantiation),
and the Spore shows that, rather than drawing an empty state that looks like a
healthy but idle system.

## Today

| host profile | crate | capabilities |
| --- | --- | --- |
| WASI HTTP (`soil spore run`) | `soil-wasi-host` | `mycelium`, `unibus` (both default on) |
| Canvas modules | `soil-canvas-host` | none yet |
| Shell | `soil-shell` | forwards `mycelium` to `soil-wasi-host` |

The Canvas profile is where a slim app would read a fleet — its modules can
present scenes but cannot yet import `fungos:mycelium`. Doing that is the same
four steps on `soil-canvas-host`; it waits for a Spore that needs it.

## Missing adapters

Absent is not empty. `MyceliumFleet::status` returns `unavailable` when the CLI
is missing, non-zero or slower than its timeout; it never returns an empty peer
list, because an empty list is a fleet claim. A component that imports
`fungos:mycelium/fleet` while the interface is unlinked fails at instantiation,
naming the import.

## Open

- **VFS and records.** `soil:records` and `fungos-vfs` are linked unconditionally
  today. They may be fundamental (a Spore is a thing with a home directory and a
  store) or the same opt-in shape as the rest. The flag mechanism does not care;
  the decision is about what a Spore *is*, so make it deliberately.
- **OS native (mic, audio, surface).** These belong to the slim build, so they
  are not fungOS capabilities in the same sense: they are the base layer every
  profile keeps. If an OS backend is optional per platform, it still wants the
  same feature-per-crate shape, just always-on for the shipped sizes.
- **Unibus lives in-tree.** `soil:unibus` is behind the `unibus` flag (on by
  default): the adapter and `unibus-sdk` drop out of a slim build. The base
  `unibus` crate still arrives through `fungos-records`, which is the records
  question below, not a wiring gap. The adapter is still
  [`crates/soil-wasi-host/src/unibus.rs`](../crates/soil-wasi-host/src/unibus.rs)
  rather than a `soil-unibus-host` crate, because it binds `soil:unibus` against
  `WasiHost` directly. Extracting it is the next full instance of this shape, and
  it waits for a second host profile that needs the bus.
