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
  install PATH [--root DIR] [--allow-unsigned-dev] [--provider-root DIR]...
                                  verify, compose, warm, and atomically install a package
  status [--root DIR]... [--json] show managed installs (default: command + Spore roots)
  image plan PATH [--root DIR]... [--json]
                                  build the granted WIT/provider host plan for an image Spore
  rollback ID [--root DIR]        switch an install back one generation
  surface FILE [--size WxH] [--capture FILE.rgba]
                                  open a native surface or capture one frame
  plugin [list|verify [PATH]|inspect ID|roots|pack DIR|compose ROOT PROVIDER IMPORT OUT|compose-cached ROOT PROVIDER IMPORT OUT]
                                  inspect, seal, or compose Spore capability exports
                                  (--json, --allow-unsigned-dev)
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
        "image" => image_cli(&args[1..]),
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

fn default_command_root() -> std::path::PathBuf {
    std::env::var_os("FUNGOS_COMMAND_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".local/share/fungos/commands")
        })
}

fn default_roots() -> Vec<std::path::PathBuf> {
    let commands = default_command_root();
    let spores = soil::command::spore_root(&commands);
    if spores == commands {
        vec![commands]
    } else {
        vec![commands, spores]
    }
}

fn default_install_root(path: &std::path::Path) -> std::path::PathBuf {
    let commands = default_command_root();
    if path.join("spore.toml").is_file() {
        soil::command::spore_root(&commands)
    } else {
        commands
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
    let path = path.context("install needs PATH")?;
    let root = root.unwrap_or_else(|| default_install_root(&path));
    let report = soil::install::install(soil::install::InstallOptions {
        path,
        root,
        allow_unsigned_dev,
        provider_roots,
        composition_cache,
    })?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if let Err(error) = reload_daemon() {
        eprintln!("loam: installed, but loamd reload failed: {error:#}");
    }
    Ok(())
}

fn reload_daemon() -> Result<()> {
    // The live daemon predates the current default path on several machines.
    // Try the configured/default socket first, then the two historical paths
    // without changing the daemon's binding. A reload is not a restart, so a
    // wrong candidate is skipped rather than allowed to fail the install.
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LOAM_LOAMD_SOCKET").filter(|value| !value.is_empty()) {
        candidates.push(std::path::PathBuf::from(path));
    }
    if let Some(socket) = soil::config::Config::default_path()
        .and_then(|path| soil::config::Config::load(&path).ok())
        .and_then(|config| config.socket)
    {
        candidates.push(socket);
    }
    candidates.push(soil::daemon::default_socket());
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        candidates.push(home.join(".local/run/hub.sock"));
        candidates.push(home.join(".mycelium/loamd.sock"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut last_error = None;
    for socket in candidates {
        if !seen.insert(socket.clone()) || !socket.exists() {
            continue;
        }
        match soil::daemon::request(&socket, &soil::daemon::Request::Reload) {
            Ok(response) if response["ok"] != false => return Ok(()),
            Ok(response) => {
                last_error = Some(anyhow::anyhow!(
                    "{}",
                    response["error"].as_str().unwrap_or("reload was refused")
                ));
            }
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn image_cli(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("image needs a command (plan)");
    };
    if command != "plan" {
        bail!("unknown image command {command}");
    }
    let mut path = None;
    let mut roots = Vec::new();
    let mut json = false;
    let mut allow_unsigned_dev = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(std::path::PathBuf::from(
                    args.get(i + 1).context("--root DIR")?,
                ));
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--allow-unsigned-dev" => {
                allow_unsigned_dev = true;
                i += 1;
            }
            value if value.starts_with('-') => bail!("unknown image option {value}"),
            value => {
                if path.replace(std::path::PathBuf::from(value)).is_some() {
                    bail!("image plan takes one package path");
                }
                i += 1;
            }
        }
    }
    let package = path.context("image plan needs a package path")?;
    let registry = if roots.is_empty() {
        soil::command::plugin::PluginRegistry::scan_default_dev(allow_unsigned_dev)?
    } else {
        soil::command::plugin::PluginRegistry::scan_roots_dev(&roots, allow_unsigned_dev)?
    };
    let plan = soil::image::ImageHostPlan::build(&package, &registry)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "{} {}: {} granted import(s), {} refused",
            plan.spore_id,
            plan.spore_version,
            plan.imports.len(),
            plan.refused.len()
        );
        for import in &plan.imports {
            println!(
                "  {} -> {} {} ({})",
                import.interface,
                import.provider.id,
                import.provider.version,
                import.provider.module_sha256
            );
        }
        for refusal in &plan.refused {
            println!("  refused {}: {}", refusal.interface, refusal.reason);
        }
    }
    Ok(())
}

fn status_cli(args: &[String]) -> Result<()> {
    let mut roots = Vec::new();
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(std::path::PathBuf::from(
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
    if roots.is_empty() {
        roots = default_roots();
    }
    let report = soil::install::status_roots(&roots)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if report.installs.is_empty() {
        for root in &report.roots {
            println!("no managed installs in {}", root.display());
        }
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<20} {:>5}  ROOT",
        "ID", "VERSION", "PACKAGE", "WARM"
    );
    for record in &report.installs {
        println!(
            "{:<24} {:<12} {:<20} {:>5}  {}",
            record.id,
            record.version,
            record.package,
            if record.warm { "yes" } else { "no" },
            record.root.display()
        );
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
    let id = id.context("rollback needs ID")?;
    let root = match root {
        Some(root) => root,
        None => {
            let status = soil::install::status_roots(&default_roots())?;
            let mut matches = status
                .installs
                .into_iter()
                .filter(|record| record.id == id)
                .map(|record| record.root);
            let first = matches
                .next()
                .with_context(|| format!("no managed install {id:?}"))?;
            if let Some(second) = matches.next() {
                bail!(
                    "install {id:?} is in more than one root ({} and {}); pass --root",
                    first.display(),
                    second.display()
                );
            }
            first
        }
    };
    let report = soil::install::rollback(&root, &id)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
