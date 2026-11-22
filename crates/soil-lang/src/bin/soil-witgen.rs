// SPDX-License-Identifier: MIT OR Apache-2.0
use std::path::PathBuf;

use anyhow::{Result, bail};
use soil_lang::dispatchgen::generate as generate_dispatch;
use soil_lang::witgen::generate;

fn main() -> Result<()> {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut output: Option<PathBuf> = None;
    let mut rust = false;
    let mut bindings = "crate::wit".to_string();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                output = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--out needs a path"))?,
                );
            }
            "--rust-dispatch" => rust = true,
            "--bindings" => {
                bindings = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bindings needs a Rust module path"))?;
            }
            "-h" | "--help" => {
                println!(
                    "usage: soil-witgen [--out FILE] <wit-file-or-dir>...\n                            soil-witgen --rust-dispatch [--bindings MODULE] [--out FILE] <wit-file-or-dir>..."
                );
                return Ok(());
            }
            other => inputs.push(PathBuf::from(other)),
        }
    }

    if inputs.is_empty() {
        bail!("missing WIT path");
    }
    if rust {
        let generated = generate_dispatch(&inputs, &bindings)?;
        if let Some(path) = output {
            std::fs::write(&path, &generated.rust)?;
            eprintln!(
                "wrote rust dispatch ({} functions, {} types) to {}",
                generated.functions,
                generated.types,
                path.display()
            );
        } else {
            print!("{}", generated.rust);
        }
        return Ok(());
    }
    if inputs.len() != 1 {
        bail!("S-expression bindings take exactly one WIT path");
    }
    let generated = generate(&inputs[0])?;

    if let Some(path) = output {
        std::fs::write(&path, &generated.sexp)?;
        eprintln!(
            "wrote {} ({} capabilities, {} types) to {}",
            generated.package,
            generated.capabilities,
            generated.types,
            path.display()
        );
    } else {
        print!("{}", generated.sexp);
    }

    Ok(())
}
