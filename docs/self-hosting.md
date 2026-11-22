# Self-hosting the numeric-core compiler

Status: **bootstrap stages 0, 1, and the Wasm-hosted compiler**, 2026-11-22.

The Rust compiler remains the reference. The Soil compiler lives in
`examples/compiler/core.soil` and has two backends:

- `compiler/compile-wat` emits WebAssembly text
- `compiler/compile-wasm` emits the binary module, byte-for-byte matching the
  Rust numeric-core compiler on the current corpus

```lisp
(compiler/compile-source "(defun add (a b) (+ a b))")
```

```wat
(module
  (func $add (export "add") (param $a i64) (param $b i64) (result i64)
    local.get $a
    local.get $b
    i64.add
  )
)
```

## Why start with WAT

WAT lets us prove the compiler pipeline without owning:

- Wasm binary section encoding
- LEB128
- opcode tables
- the object representation

It is still a compiler written in Soil, and its output runs under Wasmtime.

## Bootstrap stages

### Stage 0 — WAT compiler in Soil

- read forms with `(read source)`
- lower `defun`, `if`, arithmetic, comparisons, and calls
- emit text
- differential-test against the Rust compiler's shapes and semantics

Current limits:

- no function dependency closure
- no host imports or object model
- no self-compilation yet

### Stage 1 — binary emitter in Soil — done

`compiler/compile-wasm` now emits:

- LEB128 signed/unsigned integers
- type/function/export/code sections
- the same instruction stream as the Rust reference compiler

The differential test `crates/soil-lang/tests/selfhost.rs` compares the Soil
compiler's bytes with `soil_lang::compile` for addition, `if`, `let`, `cond`,
calls, `defconst`, `and`, `or`, and `not`.

### Wasm-hosted compiler — done

`crates/soil-compiler-wasm` embeds the Soil compiler and runs it on the
`wasm32-wasip2` interpreter:

```sh
wasmtime run --dir . soil-compiler-wasm.wasm input.soil > output.wasm
```

The compiler logic is Soil. The interpreter is the Rust-written `soil-lang`
core compiled to Wasm. This is not the fixed point yet, but it proves the
compiler can run in Wasm and emit a runnable module.

### Stage 2 — the object model — started

`examples/compiler/runtime.soil` now implements the first Soil-native object
runtime: linear memory, a mutable heap global, bump allocation, and cons cells.
It is compiled by the Soil compiler and runs under Loam/Wasmtime. See
[Object model](object-model.md).

### Stage 2 — full object model

A compiler that can compile itself needs:

- strings
- lists or arrays
- byte buffers
- symbols
- a way to write bytes

That is the first real expansion of the numeric core. Without it, the compiler
can compile numeric functions but cannot be compiled itself.

### Stage 3 — compile the compiler

Use the Rust reference compiler to compile the Soil compiler to Wasm, then run
that Wasm compiler under the existing `soil-lang-wasm` runner.

### Stage 4 — fixed point

Compile the compiler with itself. Compare:

```text
Rust compiler(compiler)  ==  Soil compiler(compiler)
```

Then bootstrap again from the output. The fixed point is the real self-hosting
milestone.

## Guardrail

The Wasmtime **runtime** stays in Rust. Self-hosting applies to the compiler:
Soil source → Wasm artifact. It does not mean writing a Wasm VM in Soil.
