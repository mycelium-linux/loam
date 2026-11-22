// SPDX-License-Identifier: MIT OR Apache-2.0
//! The one loam command.
//!
//! `loam` is the native host front door: it runs installed wasm commands
//! through the command registry. The Soil language, REPL, compiler and tools
//! live in the Soil repository, not in this runtime.

use anyhow::{Result, bail};

fn usage() -> &'static str {
    "usage: loam <command>\n\
     commands:\n\
       run [OPTIONS] NAME [ARGS...]   run an installed wasm command\n\
       doctor                         print the linked capability profile\n\
       help                           this message"
}

fn main() {
    if let Err(error) = run() {
        eprintln!("loam: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        bail!("{}", usage());
    };
    match command {
        "run" => {
            let code = soil_command::cli::run_under_loam(args[1..].to_vec())?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        "doctor" => {
            println!("loam {}", env!("CARGO_PKG_VERSION"));
            for capability in soil::WASI_PROFILE {
                println!("  {capability}");
            }
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        other => bail!("unknown loam command {other}\n{}", usage()),
    }
}
