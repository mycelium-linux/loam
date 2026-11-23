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
  install PATH --root DIR [--allow-unsigned-dev] [--provider-root DIR]...
                                  verify, compose, warm, and atomically install a package
  status --root DIR [--json]      show managed installs
  rollback ID --root DIR          switch an install back one generation
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
        "install" => install_cli(&args[1..]),
        "status" => status_cli(&args[1..]),
        "rollback" => rollback_cli(&args[1..]),
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

fn install_cli(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut root = None;
    let mut allow_unsigned_dev = false;
    let mut provider_roots = Vec::new();
    let mut composition_cache = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = Some(std::path::PathBuf::from(
                    args.get(i + 1).context("--root DIR")?,
                ));
                i += 2;
            }
            "--provider-root" => {
                provider_roots.push(std::path::PathBuf::from(
                    args.get(i + 1).context("--provider-root DIR")?,
                ));
                i += 2;
            }
            "--compose-cache" => {
                composition_cache = Some(std::path::PathBuf::from(
                    args.get(i + 1).context("--compose-cache DIR")?,
                ));
                i += 2;
            }
            "--allow-unsigned-dev" => {
                allow_unsigned_dev = true;
                i += 1;
            }
            other if other.starts_with('-') => bail!("unknown install option {other}"),
            other => {
                if path.replace(std::path::PathBuf::from(other)).is_some() {
                    bail!("install takes one package path");
                }
                i += 1;
            }
        }
    }
    let report = soil::install::install(soil::install::InstallOptions {
        path: path.context("install needs PATH")?,
        root: root.context("install needs --root DIR")?,
        allow_unsigned_dev,
        provider_roots,
        composition_cache,
    })?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn status_cli(args: &[String]) -> Result<()> {
    let mut root = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = Some(std::path::PathBuf::from(
                    args.get(i + 1).context("--root DIR")?,
                ));
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            other => bail!("unknown status option {other}"),
        }
    }
    let report = soil::install::status(&root.context("status needs --root DIR")?)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if report.installs.is_empty() {
            println!("no managed installs in {}", report.root.display());
            return Ok(());
        }
        println!(
            "{:<24} {:<12} {:<24} {:>5}",
            "ID", "VERSION", "PACKAGE", "WARM"
        );
        for record in &report.installs {
            println!(
                "{:<24} {:<12} {:<24} {:>5}",
                record.id,
                record.version,
                record.package,
                if record.warm { "yes" } else { "no" }
            );
        }
    }
    Ok(())
}

fn rollback_cli(args: &[String]) -> Result<()> {
    let mut id = None;
    let mut root = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = Some(std::path::PathBuf::from(
                    args.get(i + 1).context("--root DIR")?,
                ));
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown rollback option {other}"),
            other => {
                if id.replace(other.to_string()).is_some() {
                    bail!("rollback takes one install id");
                }
                i += 1;
            }
        }
    }
    let report = soil::install::rollback(
        &root.context("rollback needs --root DIR")?,
        &id.context("rollback needs ID")?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
