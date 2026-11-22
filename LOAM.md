# LOAM

Soil is two layers:

- **Language** — `crates/soil-lang`. Reads `.soil` source, evaluates or compiles
  it, ships the stdlib, and emits WebAssembly.
- **Runtime** — `crates/soil` and the host crates. Loads a Spore, checks its
  manifest, runs the Wasm module, and owns the capabilities.

The language is how you write the program. The runtime is what gives it
permission to do anything. A `.soil` file never opens a socket or reads a
device; a Wasm module can only call imports the host grants. A Spore is the
packaging unit between them: a manifest plus module bytes.

The native runtime is the **LOAM VM**. It uses Wasmtime with Cranelift to compile
modules to machine code at near-native speed, and wgpu for GPU-backed surfaces.
The same module can run in the browser.

```mermaid
flowchart TB
  subgraph Language["Language — crates/soil-lang"]
    S[".soil source"] --> R["reader"]
    R --> E["evaluate / compile"]
    E --> W[".wasm module"]
  end

  subgraph Spore["Spore"]
    W --> M["manifest + module bytes"]
  end

  subgraph Runtime["LOAM VM — crates/soil + hosts"]
    M --> L["loader"]
    L --> G["grant check"]
    G --> V["Wasm engine"]
    V --> C["host capabilities"]
  end
```

The runtime does not parse Soil, and the language does not own a network stack.
A Spore can be written in another language and still run; the host only sees the
manifest, the module, and the imports it is allowed to link.

## Capability contracts

The full package/grant table lives in [CAPABILITIES.md](CAPABILITIES.md). The
canonical WIT is in [`wit/`](wit) and is checked by `cargo test -p soil-abi`.
