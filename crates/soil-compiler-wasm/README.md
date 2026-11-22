# soil-compiler-wasm

The Soil numeric-core compiler, running in the `wasm32-wasip2` interpreter.

This crate embeds:

- `crates/soil-lang/stdlib/prelude.soil`
- `examples/compiler/core.soil`

It reads one Soil source file (or stdin with `-`), calls
`compiler/compile-wasm`, and writes the produced Wasm module to stdout.

```sh
cargo build -p soil-compiler-wasm --target wasm32-wasip2
wasmtime run --dir . target/wasm32-wasip2/debug/soil-compiler-wasm.wasm \
  input.soil > output.wasm
```

This is not the fixed-point self-host yet: the *compiler source* is Soil, but it
is executed by the Rust-written interpreter compiled to Wasm. The next step is
to compile the compiler to Wasm directly, which needs the object model.
