# soil-lang

Soil's Lisp: the reader, interpreter, typed core, WIT bindings, and Wasm
backends. This is the language side of Soil; the host and capabilities live in
`crates/soil` and `wit/`.

## What is here

- `sexp.rs` — S-expression reader (`'x`, `nil`, comments). `parse_document` retains top-level spans, comments, and stable form ids; nested trivia is the next slice.
- `eval.rs` — closed interpreter with `host-call`.
- `native_host.rs` — native REPL host; delegates `soil:vfs` to Soil's
  existing `VfsHost`, `soil:records` to `RecordsBookHost`, and `soil:time` to
  `soil::time::SystemTimeHost`. It is a value-boundary bridge, not a second
  capability host.
- `core.rs` / `wasm.rs` — numeric core and Wasm backend.
- `lisp.rs` — McCarthy-style Lisp core (symbols, cons, `quote`, `atom`, `eq`,
  `car`, `cdr`, `cons`, `cond`) compiled to Wasm.
- `witgen.rs` — WIT -> `.soil` binding stubs.
- `bindings/` — generated Soil capability bindings.
- `stdlib/prelude.soil` — list, JSON, records, and time helpers.
- `stdlib/ui.soil` — UI forms, HTML escaping, `render-html`, and `render-page`.
- `stdlib/highlight.soil` — a Soil tokenizer that emits highlighted form nodes.
- `boot/boot.soil` — the first evaluator written in Soil: environments,
  binding, special forms, application, and a primitive table. This is the seed
  for Phase 7 self-hosting, not the finished bootstrap.

## Run Instinct

```sh
TMP=$(mktemp -d)
cargo run -p soil-lang --bin soil-lang -- \
  --prelude crates/soil-lang/stdlib/prelude.soil \
  --records-root "$TMP" --book soil \
  crates/soil-lang/bindings/soil-records.soil \
  crates/soil-lang/bindings/soil-time.soil \
  crates/soil-lang/examples/seed_instinct.soil \
  crates/soil-lang/examples/instinct.soil
```

```text
((:devices (("az09:1" (:in-use 0.85)) ...)) (:person (:present 0.9)))
```

## Compile

WIT bindings:

```sh
cargo run -p soil-lang --bin soil-witgen -- \
  wit/soil-records.wit --out crates/soil-lang/bindings/soil-records.soil
```

Wasm (numeric core or McCarthy Lisp core):

```sh
cargo run -p soil-lang --bin soil-c -- \
  crates/soil-lang/examples/numeric.soil /tmp/numeric.wasm
# jmc.lisp needs the six accessors from stdlib/prelude.soil (see
# tests/jmc_wasm.rs). Concatenate them before compiling with --lisp.
```

## Tests

```sh
cargo test -p soil-lang
```

`tests/document.rs` pins top-level spans/comments/ids and canonical round-trip.
`tests/soil_lisp.rs` is one Rust harness for tests written in Soil:
`tests/soil/*.soil` files define `test/...` functions. This keeps most language
and WIT behavior tests in Lisp and avoids one Rust test binary per area.
`tests/soil/boot.soil` covers the bootstrap evaluator.
`tests/soil/prelude.soil` covers list/JSON helpers.
`tests/soil/wit_call.soil` calls `soil:time` and `soil:records` from Lisp.
`tests/soil/ui.soil` covers HTML escaping, form rendering, and highlighting.
`tests/jmc.rs` runs McCarthy's `eval.` in the interpreter.
`tests/jmc_wasm.rs` compiles the same `eval.` and runs it on wasmtime.
`tests/fixtures/jmc.lisp` is Paul Graham's CL translation of McCarthy's 1960
Lisp and retains its original provenance; it is not covered by this crate's
SPDX header.
