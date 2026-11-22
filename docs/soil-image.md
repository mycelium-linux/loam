# `soil-image`: session-scoped images and VFS flush

Status: **v2 Unix socket + stdin**, 2026-11-22.

`soil-image` hosts more than one live Soil image in one process. The unit is a
session, not a host process:

```text
soil-image
  ├─ session(s1)  definitions + globals + provenance
  ├─ session(s2)
  └─ shared prelude / reader / evaluator
```

An image is attached to a VFS workspace root. Source is read and written through
`soil:vfs`, never through an arbitrary host path.

The workspace root is a **session mount root**, not a Desktop and not a
repository. It is the directory that contains every repository worktree for one
session's project:

```text
sessions/<session>/<project>/
  soil/
  spores/
  theseus/
```

Subagents of that session attach to the same mount root. A parallel session gets a
different mount root. The image does not know or care which repository a form
came from; provenance records the VFS path, and that path is relative to the
session's mount root.

## Protocol

One JSON request per line, one JSON response per line:

```json
{"op":"attach","session":"s1","desktop":"fpl","project":"suite","workspace":"/vfs","records":"/records","book":"soil"}
{"op":"load","session":"s1","path":"lib/example.soil"}
{"op":"eval","session":"s1","source":"(defun f (x) (+ x 1))"}
{"op":"status","session":"s1"}
{"op":"reload","session":"s1"}
{"op":"flush","session":"s1","dry_run":true}
{"op":"flush","session":"s1"}
{"op":"flush_new","session":"s1","path":"scratch.soil","names":["f"]}
{"op":"list"}
{"op":"detach","session":"s1"}
```

Responses are `{"ok":true,"result":...}` or `{"ok":false,"error":"..."}`.

`name` is still accepted as a deprecated alias for `session` so the v1 stdin
protocol keeps working.

## Transport

Stdin mode is the fallback:

```sh
soil-image
```

Socket mode is the long-lived form:

```sh
soil-image --socket ~/.local/share/soil/image.sock
```

The socket is local-only, created with mode `0600`, and accepts the same JSON
lines. Several clients may attach to the same session. `attach` is idempotent
for a live session: the second client joins the existing image instead of
replacing its definitions. Requests are serialized by the image thread, so one
session never evaluates two forms at once.

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

## Limits

- top-level forms only
- canonical form printing; comments inside a replaced form are not preserved
- no form-level merge yet: a changed file is a conflict rather than a merge
- one evaluation at a time across the process, not per session
- no streaming eval events yet; a response is one JSON line
- no session lease yet: one daemon owns its sessions

The next step is per-session evaluation/streaming and a lease, without changing
the image/flush semantics.
