// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Soil numeric-core compiler, hosted by the Wasm interpreter.
//!
//! This does not compile the compiler itself to Wasm yet. It runs the compiler
//! source in `examples/compiler/core.soil` on the interpreter shipped as a
//! `wasm32-wasip2` command, then writes the produced Wasm module to stdout.

use std::io::{Read, Write};

use soil_lang::sexp::Value;
use soil_lang::{FakeHost, Runtime};

fn bytes(value: Value) -> Result<Vec<u8>, String> {
    let Value::List(values) = value else {
        return Err(format!("compiler did not return bytes: {value:?}"));
    };
    values
        .into_iter()
        .map(|value| match value {
            Value::Int(byte) if (0..=255).contains(&byte) => Ok(byte as u8),
            other => Err(format!("not a byte: {other:?}")),
        })
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: soil-compiler-wasm <file.soil | ->");
        std::process::exit(2);
    };
    let source = if path == "-" {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .map(|_| source)
            .map_err(|error| format!("read stdin: {error}"))
    } else {
        std::fs::read_to_string(&path).map_err(|error| format!("read {path}: {error}"))
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => {
            eprintln!("soil-compiler-wasm: {error}");
            std::process::exit(2);
        }
    };

    let mut runtime = Runtime::new(Box::new(FakeHost::new()));
    for (name, source) in [
        (
            "stdlib/prelude.soil",
            include_str!("../../soil-lang/stdlib/prelude.soil"),
        ),
        (
            "examples/compiler/core.soil",
            include_str!("../../../examples/compiler/core.soil"),
        ),
    ] {
        if let Err(error) = runtime.load(source) {
            eprintln!("soil-compiler-wasm: load {name}: {error:#}");
            std::process::exit(1);
        }
    }
    runtime.clear_provenance();
    let value = match runtime.call("compiler/compile-wasm", &[Value::String(source)]) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("soil-compiler-wasm: {error:#}");
            std::process::exit(1);
        }
    };
    let bytes = match bytes(value) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("soil-compiler-wasm: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = std::io::stdout().write_all(&bytes) {
        eprintln!("soil-compiler-wasm: write stdout: {error}");
        std::process::exit(1);
    }
}
