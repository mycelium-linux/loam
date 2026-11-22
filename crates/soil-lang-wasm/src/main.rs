// SPDX-License-Identifier: MIT OR Apache-2.0
//! Soil Lisp as a `wasm32-wasip2` command.
//!
//! This is the capability-free runner: it embeds the interpreter and prelude,
//! evaluates one source string, and prints the result. Host calls are denied by
//! `FakeHost`; a later runner can link the generated WIT dispatch table.

use std::io::Read;

use soil_lang::{FakeHost, Runtime};

fn source(args: &[String]) -> Result<(String, Vec<String>), String> {
    match args.first().map(String::as_str) {
        Some("--eval" | "-e") => {
            let source = args.get(1).ok_or("--eval needs a source string")?;
            Ok((source.clone(), args[2..].to_vec()))
        }
        Some("-") => {
            let mut source = String::new();
            std::io::stdin()
                .read_to_string(&mut source)
                .map_err(|error| format!("read stdin: {error}"))?;
            Ok((source, args[1..].to_vec()))
        }
        Some(path) => {
            let source = std::fs::read_to_string(path)
                .map_err(|error| format!("read {path}: {error}"))?;
            Ok((source, args[1..].to_vec()))
        }
        None => Err("usage: soil-lang-wasm <file.soil | -e SOURCE | -> [args...]".to_string()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (source, args) = match source(&args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("soil-lang-wasm: {error}");
            std::process::exit(2);
        }
    };

    let mut runtime = Runtime::new(Box::new(FakeHost::new()));
    runtime.set_args(args);
    if let Err(error) = runtime.load(include_str!("../../soil-lang/stdlib/prelude.soil")) {
        eprintln!("soil-lang-wasm: load prelude: {error:#}");
        std::process::exit(1);
    }
    runtime.clear_provenance();
    match runtime.load(&source) {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("soil-lang-wasm: {error:#}");
            std::process::exit(1);
        }
    }
}
