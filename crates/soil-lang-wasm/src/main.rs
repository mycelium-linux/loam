// SPDX-License-Identifier: MIT OR Apache-2.0
//! Soil Lisp as a WASI p2 command.
//!
//! The interpreter is the wasm-buildable `soil-lang` core. Capability calls go
//! through the generated dispatch table to direct typed WIT imports; there is no
//! second records or time host in this crate.

mod wit {
    wit_bindgen::generate!({
        path: "wit",
        world: "guest",
        generate_all,
    });
}

mod dispatch {
    include!("../../soil-lang/bindings/soil-dispatch.rs");
}

use soil_lang::sexp::Value;
use soil_lang::{Host, Runtime};

struct WasmHost;

impl Host for WasmHost {
    fn granted(&self, interface: &str, function: &str) -> bool {
        dispatch::granted(interface, function)
    }

    fn call(
        &mut self,
        interface: &str,
        function: &str,
        args: &[Value],
    ) -> std::result::Result<Value, String> {
        dispatch::call(interface, function, args)
    }
}

fn main() {
    let mut runtime = Runtime::new(Box::new(WasmHost));
    runtime.set_args(std::env::args().skip(1).collect());

    for (name, source) in [
        (
            "stdlib/prelude.soil",
            include_str!("../../soil-lang/stdlib/prelude.soil"),
        ),
        (
            "stdlib/records.soil",
            include_str!("../../soil-lang/stdlib/records.soil"),
        ),
        (
            "stdlib/cli.soil",
            include_str!("../../soil-lang/stdlib/cli.soil"),
        ),
        (
            "bindings/soil-records.soil",
            include_str!("../../soil-lang/bindings/soil-records.soil"),
        ),
        (
            "bindings/soil-time.soil",
            include_str!("../../soil-lang/bindings/soil-time.soil"),
        ),
        (
            "bindings/wasi-cli.soil",
            include_str!("../../soil-lang/bindings/wasi-cli.soil"),
        ),
        (
            "marbles/mb.soil",
            include_str!("../../../examples/marbles/mb.soil"),
        ),
    ] {
        if let Err(error) = runtime.load(source) {
            eprintln!("soil-lang-wasm: load {name}: {error:#}");
            std::process::exit(1);
        }
    }

    if let Err(error) = runtime.call("main", &[]) {
        eprintln!("soil-lang-wasm: {error:#}");
        std::process::exit(1);
    }
}
