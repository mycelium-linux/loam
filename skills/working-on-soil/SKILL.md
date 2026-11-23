---
name: working-on-soil
description: Work safely in the Soil repositories and in Soil Lisp programs. Use when reading, editing, testing, or running `.soil` files, generated WIT bindings, capability-driven examples, live images, Tend projects, or Spore manifests; covers form shapes, image/REPL use, tests in `.soil`, capability grants, bootstrap limits, and safe edit/rollback discipline.
---

# Working on Soil

Soil is a homoiconic Lisp substrate. The source, IR, documentation and test
forms are all S-expressions. Treat the form as the unit of work, not the line.

## Repositories

- `ajmwagar/soil` is the Soil source: `core/`, `std/`, `tools/`, `compiler/`,
  `examples/`, `spores/`, `wit/`, `scripts/` and `tend.soil`.
- `mycelium-linux/loam` is the host: the WIT linker, command runner, image
  lifecycle, surfaces, records/VFS/inference/HTTP capabilities and `loamd`.
- `ajmwagar/soil-rust` is the Rust bootstrap runner. It is not the place for
  new Soil features. Use it only when bootstrapping an interpreter or compiler
  that cannot yet run from a checked-in Soil artifact.
- Do not add a third home for Soil source, manifests or runners. If a change is
  a language/stdlib change, it belongs in `ajmwagar/soil`; if it is a host or
  lifecycle change, it belongs in Loam.

## Boundaries

- `core/prelude.soil` is the language baseline. Read it before inventing list,
  string or JSON helpers.
- `core/` and `std/` are Soil source. Host capabilities live behind WIT and are
  loaded from `std/bindings/` or generated at build time.
- `compiler/` is the Soil-written bootstrap compiler. It emits the numeric,
  object and component-probe subset. Its flat-i64 `component-wat` path is not a
  full Spore compiler.
- `tools/tend/` is the project build tool. `tend build` currently lowers source
  through `compiler/component-wat` and writes `build/module.wat` +
  `build/world.txt`; the host wrapper does `component embed` / `component new`.
- Live images are interpreted. `core/image.soil` and `tools/image/main.soil`
  describe the JSON-lines image protocol; Loam's `loamd image` owns lifecycle.
- A Spore is a package/live image. A surface, command, daemon and window are
  entry points/roles, not separate package kinds.
- `soil-wasm.wasm` is the general bootstrap runner used by Tend/LSP/tools.
  `soil-lang-wasm` is a typed command runner (records/time, embedded sources),
  not a host and not a Tend dependency.

## Form shapes

- A JSON object decodes to a list of `(:key value)` pairs. Arrays decode to
  plain lists. A keyword is `:name`; `(field record :key)` reads a field.
- `json-encode` treats a non-empty list of `(:key value)` pairs as an object;
  any other list is an array. Use keywords for object keys.
- `json-decode` accepts either a string or a list of byte integers. HTTP
  response bodies arrive as a list of bytes, so `(json-decode body)` works
  directly.
- `assoc` only replaces an existing key; it does not append a missing one. To
  add a field, `(cons (list :key value) record)`.
- `field` on `nil` or a non-record returns `nil`; use `nil?` before numeric or
  string operations.
- `length` is for lists and strings. Check `nil?` before counting an optional
  list.
- `atom` is true for non-lists; `(not (atom x))` is the list test.
- Use `defun`, `let`, `cond`, `if`, `begin`/`do`, and recursion. Lambdas and
  closures work in the interpreter, not in the current compiler.
- Numeric `=` compares numbers; list/string `=` compares values.
- Capability declarations use forms such as `define-package`, `define-record`,
  `define-enum`, `define-capability` and `define-alias`. Generated binding files
  are generated: regenerate them; do not hand-edit them.

## Manifests and entry points

- `tend.soil` is the project manifest. Read `(members ...)` and the `image`
  block before building.
- `(image (entry ...) (prelude ...) (load ...))` is an image spec: prelude first,
  declared loads in manifest order, then the entry. Do not treat it as a
  single-file compiler input.
- `spore.toml` is the installed package manifest. `spore.soil` is the source
  manifest a package carries while edited.
- A package may declare several surfaces/entry points. A web surface has a page
  entry; a Canvas surface has a UI entry; a command is a `wasi:cli` component.
- `[launch] default` names the surface a bare launch opens. `open = "window"` or
  `open = "browser"` is a presentation preference for a web surface.
- Grants are requests in the manifest and consent in the host. A manifest cannot
  grant itself authority.

## Workflow

1. Read the relevant example, prelude helpers, WIT contract and manifest.
2. Put domain logic in a plain `.soil` library. Keep host calls at the edge.
3. Write pure tests in `tests/`. A test file defines `(defun test/... () ...)`
   functions; returning `nil` or `#f` fails.
4. Run focused tests from the Soil repo:

   ```sh
   SOIL_WASM=/path/to/soil-wasm.wasm scripts/run-core-tests.sh
   ```

   For a single test, invoke the installed `soil-wasm`/Wasmtime path the script
   uses; do not invent a second test harness.

5. Parse/load a file without calling `main` through the installed `soil` runner
   or the checked-out runner, with the prelude and bindings the program needs.
6. For a live image, use attach/load/eval/status/flush/snapshot; edits apply on
   the next reload/eval. A compiled Spore needs rebuild + restart.

## Capabilities and secrets

- A guest names a capability; it never opens a socket, reads a token, or
  chooses a peer. The host decides origins, credentials, quotas and routing.
- `soil:http` requests use `:url`, `:method`, `:headers`, `:body`,
  `:credential`, `:max-bytes`, `:timeout-ms`; responses carry `:status`,
  `:final-url`, `:headers`, `:body`.
- Credentials are handles. Never put a token, bridge key, peer address or
  device id in a committed `.soil` file.
- `soil:records`, `soil:vfs`, `soil:inference`, `soil:ble`, `soil:unibus` and
  the other interfaces are host policy. A program names the operation, not the
  transport.
- A live image is stateful by definition. A stateless entry point must not own
  windows, mutate VFS, or depend on process-local state.

## Bootstrap compiler limits

The Soil-written compiler does not yet lower arbitrary `host-call` forms into
typed WIT imports. The current `tend build` can emit a core module and refuse
`host-call` structurally; that is honest, not a complete Spore build.

Do not claim a Spore builds until the output has passed:

```sh
wasm-tools validate module.wasm
```

and Loam can instantiate it with the grants it declares. Keep the bootstrap
emitter behind `--target core` if a build path is not a real Spore path.

## Safe edit protocol

Before applying a structural or text edit:

1. Read the current form and keep a snapshot of the file.
2. Apply the change to a temporary copy, not the working file.
3. Parse/load the temporary copy without calling `main`. A syntax or
   unbound-form error stops here.
4. Run the focused `.soil` tests.
5. Show the semantic diff and `git diff`.
6. Apply atomically (write temp + rename) under a file-mutation lock.
7. Re-parse and rerun the focused test. On failure, restore the snapshot and
   keep the failure output.

Do not evaluate mutating forms as a “lint” step. Evaluation can turn on lights,
publish events, write records or spend tokens. Use a dry-run/plan function, a
fake host, or a disposable VFS/records root. Syntax checking and loading are
always safe; side-effecting evaluation is a separate, explicit action.

Use git as the durable checkpoint:

- work on a feature branch, never directly on `main`;
- checkpoint only the files touched by the change;
- use a git stash/ref checkpoint per agent turn if the harness supports it;
- before stopping, commit intended files, push the branch, and open a PR; do
  not leave a dirty worktree.

## Common mistakes

- Assuming `assoc` adds a key.
- Calling `length` on `nil`.
- Treating a decoded JSON object as an array.
- Hardcoding a URL, token, peer or device id in Soil.
- Evaluating a side-effecting form to test syntax.
- Editing generated bindings by hand instead of regenerating them.
- Pointing `tend build` at the bootstrap emitter and calling the result a Spore.
- Treating `(image ...)` as a single source file instead of prelude + loads +
  entry.
- Adding a Rust test when the assertion belongs in `tests/*.soil`.
