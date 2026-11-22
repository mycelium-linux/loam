# soil-lang-wasm

The Soil Lisp interpreter as a `wasm32-wasip2` command. It embeds the standard
library and `examples/marbles/mb.soil`, then dispatches `(host-call …)` through
the generated typed WIT table in `crates/soil-lang/bindings/soil-dispatch.rs`.

Build:

```sh
cargo build -p soil-lang-wasm --target wasm32-wasip2
```

Package it as a command:

```text
/tmp/commands/mb/command.toml
/tmp/commands/mb/soil-lang-wasm.wasm
```

```toml
schema = 1

[command]
name = "mb"
version = "0.1.0"
summary = "Marbles issue tracker, running Soil Lisp in wasm"
module = "soil-lang-wasm.wasm"
requires = ["soil:records", "soil:time"]
```

Run it through the command runner:

```sh
soil-command --root /tmp/commands \
  --records-root "$HOME/.local/share/fungos/records" \
  --book personal/avery/marbles --collection "" \
  run mb ready --project marbles
```

The empty collection is intentional: `mb` addresses records by entity id only.
