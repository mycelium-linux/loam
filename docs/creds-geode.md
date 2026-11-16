# Geode credentials in Soil: two adapters, one custody story

There is nothing left to invent here — the bridge exists, twice, because there are
two directions a credential can travel. This note records the map so the next
person does not rebuild it (it has been "proposed" twice already).

## Direction 1 — `soil:creds` (guest-facing, signing)

`crates/soil/src/creds.rs`

- `trait CredentialProvider { resolve, sign, verify }` — no read, list or export
  operation; that is the point of the contract.
- `StaticCredentials` — in-process table (tests, local operator).
- `GeodePgpCredentials` — per-entry `{ reference: "geode://…", purpose,
  fingerprint, public_key }`; invokes `geode-pgp`, which leases the key from
  `geoded` and signs/verifies in an ephemeral GnuPG home. Only the signature
  comes back.
- `GeodeCredentials` — leases over the owner-only `geoded` socket; mints an
  opaque handle per resolve, keeps the material host-side as
  `Material::HmacSha256`, and reports purpose + expiry on the handle. The
  reference prefix is host configuration, so a guest cannot address another
  realm by spelling its reference; the purpose is host-chosen, so a credential
  minted to verify a webhook cannot sign one.

## Direction 2 — `http_host` (host-side, outbound requests)

`crates/soil/src/http_host.rs`

- its own `trait CredentialProvider { resolve(handle) -> (header, value),
  resolve_path(handle) }` — the guest supplies a handle, the host injects the
  header or the path segment.
- `NoCredentials`, `BearerCredential` (one env-supplied token for handle
  `default`, never in argv or `ps`).
- `GeodeCredentials` — leases a reference for the duration of one fetch and
  exposes it as a header or a path segment. The secret never returns to the
  Spore.

## The overlap to clean up (not a missing feature)

- Two traits, two structs both named `GeodeCredentials`, two unix-socket clients
  for the same `{"op":"lease"}` protocol.
- Cleanest fix: one geode client (the `creds.rs` one) grows
  `resolve_header(handle) -> (name, value)` / `resolve_path(handle)`, and
  `http_host`'s trait becomes a thin view over it. Failing that, rename them
  (`GeodeHandles`, `GeodeHttp`) so the collision stops being a trap.
- `Material` is `HmacSha256 | Ed25519Signing | Ed25519Verifying`, so a
  bearer-style secret is not expressible **through `soil:creds`**. That is
  deliberate: a guest that needs to present a token does it through `soil:http`
  with a handle (Direction 2), and never sees the bytes. Nothing is missing for
  bob — its R2/sccache tokens are Direction 2 credentials.

## Wiring points

- `host.rs`: `HostSetup::credential_provider(Arc<dyn creds::CredentialProvider>)`
  feeds `soil:creds`.
- `http_host.rs`: `HttpAccess` holds the Direction-2 provider; the CLI
  (`BearerCredential`, geode) chooses it.
- Binaries construct these: `crates/soil/src/bin/soil-http.rs`,
  `soil-wasi-http.rs`, `soil-webgpu.rs`, and `canvas-winit`. That is the place to
  check when a credential works in a test and not in a window.
