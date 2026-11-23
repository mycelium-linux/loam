//! The command CLI, shared by `soil command` and the `soil-command` binary.
//!
//! One parser, one policy, two front doors: the standalone binary exists so a
//! host without the `soil` runner can still run commands, and `soil command` is
//! the same surface inside the runner people actually have. Anything that
//! drifts between them would be a second definition of what a command may
//! reach, which is exactly what this file prevents.
//!
//! The return value is the guest's exit code, not the CLI's: a command that
//! exits 3 has not failed the runner, it has said 3.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::{CommandRegistry, CommandRunner, RecordsBinding, RunRequest};

fn on_path(dir: &std::path::Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|candidate| candidate == dir))
        .unwrap_or(false)
}

/// Where shims go when `--dir` is not given: the directory fungos already owns,
/// beside `commands/` and `records/`.
fn default_shim_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/share/fungos/bin")
}

pub fn run(args: Vec<String>) -> Result<i32> {
    run_as(args, &[])
}

/// The same CLI inside the `soil` binary. The only difference is what a shim
/// has to say to come back here: `soil command …` rather than `soil-command …`.
pub fn run_under_soil(args: Vec<String>) -> Result<i32> {
    run_as(args, &["command"])
}

/// The same CLI inside the one `loam` binary. A shim written by `loam run
/// shims` has to come back through `loam run …`, not through the old
/// `soil command …` spelling.
pub fn run_under_loam(args: Vec<String>) -> Result<i32> {
    run_as(args, &["run"])
}

fn run_as(args: Vec<String>, subcommand: &[&str]) -> Result<i32> {
    let command_root = std::env::var_os("FUNGOS_COMMAND_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/share/fungos/commands")
        });
    // Explicit `--root` flags replace the default pair; otherwise this sees
    // exactly what a shell sees: hand-installed commands, then installed Spores.
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut records_root = None;
    let mut book = None;
    let mut collection = None;
    let mut unibus_config: Option<PathBuf> = None;
    // Nothing is inherited: a command gets exactly the variables a caller names.
    let mut env = Vec::new();
    let mut list = false;
    let mut shims = false;
    let mut warm = false;
    let mut shim_dir: Option<PathBuf> = None;
    let mut prune = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(PathBuf::from(args.get(i + 1).context("--root DIR")?));
                i += 2;
            }
            "--dir" => {
                shim_dir = Some(PathBuf::from(args.get(i + 1).context("--dir DIR")?));
                i += 2;
            }
            "--prune" => {
                prune = true;
                i += 1;
            }
            "--env" => {
                let assignment = args.get(i + 1).context("--env NAME=VALUE")?;
                let (name, value) = assignment
                    .split_once('=')
                    .with_context(|| format!("--env expects NAME=VALUE, got {assignment:?}"))?;
                if name.is_empty() {
                    bail!("--env expects NAME=VALUE, got {assignment:?}");
                }
                env.push((name.to_string(), value.to_string()));
                i += 2;
            }
            "--env-from" => {
                // One variable from the caller's environment, named by a policy
                // file the shim sourced. Unset is not an error: a policy may
                // derive a value that only exists in some directories.
                let name = args.get(i + 1).context("--env-from NAME")?;
                if let Ok(value) = std::env::var(name) {
                    env.push((name.clone(), value));
                }
                i += 2;
            }
            "--records-root" => {
                records_root = Some(PathBuf::from(
                    args.get(i + 1).context("--records-root DIR")?,
                ));
                i += 2;
            }
            "--book" => {
                book = Some(args.get(i + 1).context("--book ID")?.clone());
                i += 2;
            }
            "--collection" => {
                collection = Some(args.get(i + 1).context("--collection NAME")?.clone());
                i += 2;
            }
            "--unibus-config" => {
                unibus_config = Some(PathBuf::from(
                    args.get(i + 1).context("--unibus-config FILE")?,
                ));
                i += 2;
            }
            "--list" => {
                list = true;
                i += 1;
            }
            "run" => {
                i += 1;
                break;
            }
            // Not `run`: a shim run has no guest argv to protect, so the flags
            // after it are still this binary's.
            "shims" => {
                shims = true;
                i += 1;
            }
            "warm" => {
                warm = true;
                i += 1;
            }
            // `loam run NAME ...` is the public form. Older shims spell it
            // `loam run run NAME ...`; accept both rather than making callers
            // know which parser layer they are talking to.
            other if subcommand.len() == 1 && subcommand[0] == "run" && !other.starts_with('-') => {
                break;
            }
            other => bail!("unknown option {other}"),
        }
    }
    let roots = if roots.is_empty() {
        crate::command::roots(command_root)
    } else {
        roots
    };
    let registry = CommandRegistry::scan_roots(&roots)?;
    // A package that could not be read is worth saying out loud, but it is not a
    // reason to refuse the commands that did load.
    for refusal in registry.refused() {
        eprintln!("soil: {}: {}", refusal.name, refusal.reason);
    }
    if list {
        for entry in registry.entries() {
            println!("{}\t{}\t{}", entry.name, entry.version, entry.summary);
        }
        return Ok(0);
    }
    if warm {
        let report = crate::command::warm(&roots)?;
        for name in &report.warmed {
            println!("warmed {name}");
        }
        for name in &report.already {
            println!("already warm {name}");
        }
        for (name, why) in &report.skipped {
            eprintln!("skipped {name}: {why}");
        }
        return Ok(0);
    }
    if shims {
        let dir = shim_dir.unwrap_or_else(default_shim_dir);
        let runner = std::env::current_exe().context("resolve this binary")?;
        let report = crate::command::write_shims(&registry, &dir, &runner, subcommand, prune)?;
        for path in &report.written {
            println!("wrote {}", path.display());
        }
        for (name, why) in &report.kept {
            eprintln!("kept {name}: {why}");
        }
        for path in &report.pruned {
            println!("pruned {}", path.display());
        }
        if !report.written.is_empty() && !on_path(&dir) {
            eprintln!(
                "note: {} is not on PATH; add it to use these from a shell",
                dir.display()
            );
        }
        return Ok(0);
    }
    if prune || shim_dir.is_some() {
        bail!("--dir and --prune belong to `shims`");
    }
    let Some(name) = args.get(i).cloned() else {
        bail!(
            "usage: loam run [--root DIR]... [--env NAME=VALUE]... [--env-from NAME]... [--list] NAME [ARGS...]\n       loam run [--root DIR]... shims [--dir DIR] [--prune]\n       loam run [--root DIR]... warm"
        );
    };
    let entry = registry.get(&name).context("unknown command")?.clone();
    let records = match (records_root, book, collection) {
        (Some(root), Some(book), Some(collection)) => Some(RecordsBinding {
            root,
            book,
            collection,
        }),
        (None, None, None) => None,
        _ => bail!("records binding needs --records-root, --book and --collection"),
    };
    // Only a pipe is input. A human at a prompt has nothing to send, and
    // reading a terminal to EOF blocks until Ctrl-D — which turns `mb list` into
    // a hang. `is_terminal` is the difference between "no stdin" and "stdin
    // that happens to be open".
    let mut stdin = Vec::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_end(&mut stdin)?;
    }
    // A command that lists `soil:unibus` in `requires` gets the interface
    // linked even when no hub is configured; the host then answers
    // `unavailable` and the command can retry the publish later. The shim
    // sources a policy file, so a host can name the hub there instead of
    // putting it in every argv. An explicit flag still wins.
    let unibus_config = unibus_config.or_else(|| {
        std::env::var_os("UNIBUS_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let bus = match &unibus_config {
        Some(path) => {
            Some(Arc::new(crate::UnibusBus::from_config_file(path)?) as Arc<dyn crate::Bus>)
        }
        None => None,
    };
    let plugins = crate::command::plugin::PluginRegistry::scan_roots(&roots)?;
    let output = CommandRunner::new(records, bus)?
        .with_plugins(plugins)
        .run(
            &entry,
            &RunRequest {
                args: args[i + 1..].to_vec(),
                stdin,
                // Commands do not inherit the host environment; `--env` is the only
                // way a variable reaches one.
                env,
                cwd: None,
            },
        )?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if output.code != 0 {
        return Ok(output.code);
    }
    Ok(0)
}
