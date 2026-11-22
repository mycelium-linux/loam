# Object model for compiled Soil

Status: **first Soil-native slice**, 2026-11-22.

Soil owns the object model. Loam is the Rust/Wasm engine: it instantiates
modules, provides memory limits and host capabilities, and runs the code. Loam
does not own strings, cons cells, or the allocator.

## Decision

Use **linear memory plus tagged i64 values**, written in Soil and compiled by the
Soil compiler.

```text
value = i64
  integer       immediate
  object handle  tagged/linear-memory reference
```

The first slice keeps the encoding simple: values are i64, object handles are
memory addresses, and the runtime knows which positions expect an object. A tag
bit comes before integers and objects are mixed freely.

## First slice — done

`examples/compiler/runtime.soil` is compiled by `examples/compiler/core.soil`
into a Wasm module with:

```text
memory 1
global 0 = mutable i64 heap pointer, initialised to 16
```

It provides:

```lisp
(rt/cons car cdr)
(rt/car cell)
(rt/cdr cell)
```

This is enough to prove:

- the compiler emits a memory section
- the compiler emits a mutable global section
- the compiler emits `i64.load`, `i64.store`, `global.get`, and `global.set`
- a Soil-written bump allocator works
- a Soil-written object runtime runs under Loam/Wasmtime

The runtime is compiled with the existing `soil-lang` binary and run with
Wasmtime; no Rust rebuild is needed for the runtime itself.

## Compiler additions

The Soil compiler now emits:

- memory section
- global section
- `i64.load` / `i64.store`
- `global.get` / `global.set`
- `i32.wrap_i64`
- `i64.extend_i32_u`
- `begin` blocks, which drop non-final expression values
- void expressions such as stores and global sets

The opcode helpers are written with Soil macros inside the compiler source. The
runtime source deliberately avoids macros because the compiler does not expand
macros on the forms it compiles yet.

## Next object work

1. Tag integers and object handles.
2. Add string objects and byte buffers.
3. Add symbol interning.
4. Add list helpers (`length`, `append`, `reverse`, `nth`) in the runtime.
5. Add `read` and `string->bytes` either in the runtime or as explicit Loam
   capabilities.
6. Compile the compiler against the runtime.
7. Replace any remaining bootstrap host imports.
8. Fixed-point check.

## Guardrail

Keep the boundary crisp:

```text
Loam   engine, memory limits, capabilities, WASI
Soil   values, allocator, strings, lists, compiler
```

If an object operation is in Loam, it is a bootstrap exception, not the target
design.
