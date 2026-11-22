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
{"op":"compile","session":"s1","source":"(defun add (a b) (+ a b))"}
{"op":"call-compiled","session":"s1","source":"(defun add (a b) (+ a b))","function":"add","args":[20,22]}
{"op":"compile-function","session":"s1","name":"add"}
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

## Compile

`compile` turns the numeric core into a Wasm artifact and writes it through the
same VFS the image uses:

```json
{"op":"compile","session":"s1","source":"(defun add (a b) (+ a b))"}
{"op":"call-compiled","session":"s1","source":"(defun add (a b) (+ a b))","function":"add","args":[20,22]}
{"op":"compile-function","session":"s1","name":"add"}
```

Response:

```json
{"ok":true,"result":{"session":"s1","path":"compiled/<blake3>.wasm","bytes":123,"functions":["add"],"hash":"..."}}
```

This is intentionally narrower than the interpreter:

- only the numeric core (`i64`, arithmetic, `let`, `if`, `cond`, calls)
- no host capabilities, strings, lists, macros, or dynamic values
- the artifact is content-addressed by its Wasm bytes
- execution of the artifact is a separate step; `compile` does not eval it

That gives the harness a stable build artifact without making the live image
depend on a native Wasm engine.

`call-compiled` compiles (or loads) the artifact, instantiates it in a native
Wasm engine, and calls one exported function:

```json
{"op":"call-compiled","session":"s1","path":"compiled/<hash>.wasm","function":"add","args":[1,2]}
```

Response includes `result`, `compiled: true`, and the module hash. The module is
cached by content hash for later calls. `source` may be supplied instead of
`path`; it is compiled and written first.

This is still the numeric core only. Unsupported forms return an error and the
harness should fall back to interpreted eval.

## Per-function compilation

`compile-function` installs a compiled variant under the live function name:

```json
{"op":"compile-function","session":"s1","name":"add"}
```

After that, ordinary image evaluation dispatches to the compiled function:

```json
{"op":"eval","session":"s1","source":"(add 20 22)"}
```

The interpreter remains the fallback:

- if the function is not in the numeric core, compilation fails and nothing is installed
- if arguments or the return shape cannot be represented, the call falls back
- redefining the function removes its compiled variant
- changing a `defconst` clears all compiled variants, because constants are inlined

`status` reports `compiled` names. This is the first per-function replacement
path; it is not yet a full native compiler.

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
