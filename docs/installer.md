# Local install

`loam install` is the local, atomic package installer. It does not use a store,
does not expose `loam:ctl`, and does not run a daemon.

```sh
loam install PATH --root DIR [--allow-unsigned-dev]
loam status --root DIR [--json]
loam rollback ID --root DIR
```

`PATH` is a command package or a Spore package directory. The source tree is
never modified.

## Layout

```text
DIR/
  .loam/
    state.json
    staging/<txn>/
    generations/<package>/<generation>/
    compositions/<key>.wasm
  .warm/<command>.json
  <package> -> .loam/generations/<package>/<generation>
```

The visible package path is a symlink. A new generation is fully copied,
composed and warmed before the symlink or `state.json` changes.

## Sequence

```text
verify manifest + lock
verify signatures unless --allow-unsigned-dev
copy into staging
resolve providers from plugin roots
compose resource imports through compose_cached
write runtime.wasm + runtime.key
warm the staged module
write the generation directory
atomically replace the package symlink
write state.json
```

The previous generation is retained for `loam rollback`.

## Verification

A Spore directory must have a current `spore.lock`. A stale lock is refused, not
rewritten. Unsigned packages require `--allow-unsigned-dev`, which is recorded
as `dev: true` in the install state.

## Providers

`--provider-root DIR` may be repeated. If omitted, the normal plugin roots are
scanned.

A resource import with no matching wasm provider is refused and the import is
named. Native sidecar providers cannot be composed into `runtime.wasm` in this
slice.

## Rollback

```sh
loam rollback acme:marbles --root ~/.local/share/fungos/commands
```

Rollback re-warms the previous generation, swaps the package symlink, and keeps
the generation it replaced as the new previous entry. It does not re-run
composition.

## Not in this slice

```text
remote store / update catalog
custom keyring policy
signing command packages
loam:ctl
streaming install progress
```
