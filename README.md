# Soil

**A Lisp-like programming language for performant applications and automation.**

[![ci](https://github.com/ajmwagar/soil/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ajmwagar/soil/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![website](https://img.shields.io/badge/website-soil--lang.org-c8f169.svg)](https://soil-lang.org)
[![stars](https://img.shields.io/github/stars/ajmwagar/soil?style=flat&logo=github&label=stars)](https://github.com/ajmwagar/soil/stargazers)

```lisp
(defun view (state)
  (el "main" nil
    (list
      (el "h1" nil (list (txt "Hello from Soil")))
      (el "button" (list (attr :data-op "inc"))
        (list (txt "+"))))))
```

Soil compiles to WebAssembly and runs natively. Programs are called **Spores**:
a signed manifest plus a Wasm module.

[Website](https://soil-lang.org) ·
[API docs](https://soil-lang.org/docs/) ·
[Examples](https://soil-lang.org/#examples) ·
[Architecture](LOAM.md) ·
[Source](https://github.com/ajmwagar/soil)

> **Status:** early. The interpreter, stdlib, native CLI, and Wasm path run today.

## Language

The reader, interpreter, typed core, stdlib, and Wasm backends live in
[`crates/soil-lang`](crates/soil-lang/README.md). [LOAM.md](LOAM.md) shows where
the language ends and the runtime begins.

## Capabilities

A Spore gets exactly the imports its manifest grants. The full contract list
lives in [LOAM.md](LOAM.md); the canonical WIT is in [`wit/`](wit).

## One runner

`soil` is one host and one binary. A Spore can run as a command, an HTTP
service, a window, or a browser bridge. The manifest grants capabilities; the
host links them. An ungranted import fails.

```sh
soil spore run ./spore --mode command -- ingest --event '<json>'
soil spore run ./spore --mode http
soil spore run ./spore --mode ui
soil spore run ./spore --mode web
```

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Repository

Canonical source: <https://github.com/ajmwagar/soil>. The old
`mycelium-linux/soil` URL redirects after the transfer.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=ajmwagar/soil&type=Date)](https://star-history.com/#ajmwagar/soil&Date)

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
