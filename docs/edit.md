# loam edit

`loam edit` is a host-owned source overlay workflow. It never mutates a signed
Spore and never adds a WIT capability.

```sh
loam edit open <package-dir> [--build-command PROGRAM] --json
loam edit checkpoint <session> [--message TEXT] --json
loam edit build <session> [--build-command PROGRAM] --json
loam edit save <session> [--message TEXT] --json
loam edit apply <session> --json
loam edit discard <session> [--force] --json
```

Defaults:

```text
edit-root  = $XDG_DATA_HOME/loam/edit
           = ~/.local/share/loam/edit
patch-root = $XDG_DATA_HOME/loam/patches
           = ~/.local/share/loam/patches
```

A session is addressed as:

```text
<spore-id>/<base-digest>
```

For Marbles:

```text
acme:marbles/cbdfd1be88b65c50f4d77c17efb1b24b52d8ebd52c1bf08efcde69a171d474fe
```

The session directory is:

```text
<edit-root>/<spore-id>/<base-digest>/
  session.json
  work/
  checkpoints/0001/
  build/
  patches/<patch-id>/
    patch.soil
    overlay/
    signatures/       optional signing hook
```

`open` requires `spore.soil`, `spore.lock`, and a source directory. A package
whose distribution is `binary` fails with `source-unavailable`. The signed
`module.wasm` is not copied into `work/`.

`save` refuses changes under `wit/` and refuses `module.wasm`; those are forks,
not patches. A failed build is never saveable. A successful build writes logs
and status under `build/`.

`apply` copies saved patch trees to:

```text
<patch-root>/<spore-id>/<base-digest>/<patch-id>/
```

It does not restart a module. This slice always reports `restart_required: false`
because patches cannot replace the module or WIT.
