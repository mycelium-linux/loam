# Soil compiler bootstrap

- `core.soil` — the Soil-written numeric-core compiler (WAT and binary backends)
- `runtime.soil` — the first Soil-written object runtime: memory, heap global,
  cons cells, car/cdr

## Smoke test without Rust rebuilds

Use an existing `soil-lang` binary and `wasmtime`:

```sh
SOIL_LANG=${SOIL_LANG:-target/debug/soil-lang}

$SOIL_LANG --prelude crates/soil-lang/stdlib/prelude.soil \
  --call main examples/compiler/core.soil /tmp/compile-runtime.soil \
  > /tmp/runtime-bytes.txt
```

`/tmp/compile-runtime.soil` is a small Soil program whose `main` calls
`(compiler/compile-wasm "<runtime + test source>")`. The result is a Lisp list
of bytes; convert it with a few lines of Python, then run it:

```sh
python3 - <<'PY'
from pathlib import Path
text=Path('/tmp/runtime-bytes.txt').read_text().strip()[1:-1]
Path('/tmp/runtime.wasm').write_bytes(bytes(int(x) for x in text.split()))
PY

wasmtime run --invoke rt/test-cons /tmp/runtime.wasm
# 1
wasmtime run --invoke rt/test-cdr /tmp/runtime.wasm
# 2
```

This path exercises the Soil compiler and runtime without rebuilding Rust.
