# soil-lang-wasm

Soil Lisp as a `wasm32-wasip2` command. This is the capability-free runner: it
embeds the interpreter and the standard prelude, evaluates one source string,
and prints the result. Host calls are denied by `FakeHost`.

Build locally (bypass a remote build wrapper when needed):

```sh
cargo build -p soil-lang-wasm --target wasm32-wasip2
```

Run with Wasmtime:

```sh
wasmtime target/wasm32-wasip2/debug/soil-lang-wasm.wasm -e '(map (lambda (x) (+ x 1)) (list 1 2 3))'
wasmtime target/wasm32-wasip2/debug/soil-lang-wasm.wasm path/to/program.soil
printf '(defun sq (x) (* x x))\n(sq 12)\n' \
  | wasmtime target/wasm32-wasip2/debug/soil-lang-wasm.wasm -
```

This runner is deliberately smaller than the native `soil-lang` command:

- no `soil:records`, `soil:time`, `soil:vfs`, or HTTP capabilities
- no image server or VFS flush
- one source string per process

The next step is to link the generated WIT dispatch table from
`crates/soil-lang/bindings/soil-dispatch.rs` so the same runner can call
capabilities under the command runner's grants.
