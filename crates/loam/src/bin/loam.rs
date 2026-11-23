// SPDX-License-Identifier: MIT OR Apache-2.0
//! The one loam command.
//!
//! `loam` is the native host front door: it runs installed wasm commands
//! through the command registry. The Soil language, REPL, compiler and tools
//! live in the Soil repository, not in this runtime.

use anyhow::{Context, Result, bail};

fn usage() -> &'static str {
    r#"usage: loam <command>
commands:
  run [OPTIONS] NAME [ARGS...]    run an installed wasm command
  surface FILE [--size WxH] [--capture FILE.rgba]
                                  open a native surface or capture one frame
  plugin [list|verify|roots|pack DIR|compose ROOT PROVIDER IMPORT OUT|compose-cached ROOT PROVIDER IMPORT OUT]
                                  inspect, seal, or compose Spore capability exports
  edit <op> ... [--json]          edit a Spore source overlay
  doctor                          print the linked capability profile
  help                            this message"#
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
            let code = soil::command::cli::run_under_loam(args[1..].to_vec())?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        "surface" => {
            let mut path = None;
            let mut size = None;
            let mut capture = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--size" => {
                        let value = args.get(i + 1).context("--size WxH")?;
                        let (w, h) = value.split_once('x').context("--size expects WxH")?;
                        size = Some((
                            w.parse().context("surface width")?,
                            h.parse().context("surface height")?,
                        ));
                        i += 2;
                    }
                    "--capture" => {
                        capture = Some(std::path::PathBuf::from(
                            args.get(i + 1).context("--capture FILE.rgba")?,
                        ));
                        i += 2;
                    }
                    value => {
                        path = Some(std::path::PathBuf::from(value));
                        i += 1;
                    }
                }
            }
            let path = path.context("surface needs a component path")?;
            let size = size.unwrap_or((1000, 700));
            if let Some(output) = capture {
                let frame = loam_surface::capture(&path, size)?
                    .context("surface fixture presented no frame")?;
                std::fs::write(&output, &frame.rgba)
                    .with_context(|| format!("write {}", output.display()))?;
                println!("{}x{} {}", frame.width, frame.height, output.display());
                Ok(())
            } else {
                loam_surface::run_window(&path, Some(size))
            }
        }
        "plugin" => soil::command::plugin::cli(&args[1..]),
        "edit" => std::process::exit(soil::edit::run(&args[1..])),
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
