# `soil-image` v1: worktree-scoped images and VFS flush

Status: **v1 stdin server**, 2026-11-22.

`soil-image` hosts more than one live Soil image in one process. The unit is a
VFS worktree, not a host process:

```text
soil-image
  ├─ image(w1)  definitions + globals + provenance
  ├─ image(w2)
  └─ shared prelude / reader / evaluator
```

An image is attached to a VFS workspace root. Source is read and written through
`soil:vfs`, never through an arbitrary host path.

The workspace root is a **thread mount root**, not a Desktop and not a
repository. It is the directory that contains every repository worktree for one
thread's project:

```text
threads/<thread>/<project>/
  soil/
  spores/
  theseus/
```

Subagents of that thread attach to the same mount root. A parallel thread gets a
different mount root. The image does not know or care which repository a form
came from; provenance records the VFS path, and that path is relative to the
thread's mount root.

## Protocol

One JSON request per line, one JSON response per line:

```json
{"op":"attach","name":"w1","workspace":"/vfs/w1","records":"/records","book":"soil"}
{"op":"load","name":"w1","path":"lib/example.soil"}
{"op":"eval","name":"w1","source":"(defun f (x) (+ x 1))"}
{"op":"status","name":"w1"}
{"op":"reload","name":"w1"}
{"op":"flush","name":"w1","dry_run":true}
{"op":"flush","name":"w1"}
{"op":"flush_new","name":"w1","path":"scratch.soil","names":["f"]}
{"op":"list"}
{"op":"detach","name":"w1"}
```

Responses are `{"ok":true,"result":...}` or `{"ok":false,"error":"..."}`.

## Provenance

When a file is loaded from VFS, every `defun`/`defmacro` records:

```text
name, vfs path, span, source hash, original form
```

A REPL redefinition updates the live form but leaves the origin in place. That
makes it dirty. `status` reports:

- `dirty`: definitions whose live form differs from the form loaded from VFS
- `unattached`: definitions created in the image with no VFS origin

The prelude is loaded as baseline and its provenance is cleared, so helpers are
not reported as pending edits.

## Reload

`load` remembers the VFS content hash it read. `reload` checks every loaded
path and reloads only the ones whose content hash changed. The response names
the files it reloaded. This is how a save in Neovim or a `fungos-vfs put`
becomes visible to a running image without restarting it.

`status` includes the loaded paths.

## Flush

`flush_preview` groups dirty definitions by VFS path, rejects the file if its
current content hash differs from the hash the image loaded, and returns the
candidate source. No write happens.

`flush` performs the same check and writes through `soil:vfs-mut`, then reloads
the changed files so origins point at the new source.

`flush_new` appends unattached definitions to a chosen VFS path. This is the
“I did too much in the REPL” path.

## Limits in v1

- top-level forms only
- canonical form printing; comments inside a replaced form are not preserved
- no form-level merge yet: a changed file is a conflict rather than a merge
- no socket server yet; stdin JSON lines only
- one evaluation at a time per process

The next step is a Unix-socket transport and per-session output, without changing
the image/flush semantics.
