// SPDX-License-Identifier: MIT OR Apache-2.0
//! The per-user loam supervisor and its local control client.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use soil::{
    command::RecordsBinding,
    daemon::{self, Options, Request},
};

const DEFAULT_MAX_INSTANCES: usize = 8;

fn usage() -> &'static str {
    r#"usage: loamd [serve] [--socket PATH] [--root DIR]...
       loamd start COMMAND [ARGS...] [--id ID] [--socket PATH]
       loamd list|ping|inspect ID|logs ID|stop ID|reload|shutdown [--socket PATH]"#
}

fn main() {
    if let Err(error) = run() {
        eprintln!("loamd: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "help" | "--help" | "-h"))
    {
        println!("{}", usage());
        return Ok(());
    }
    let (operation, args) = match args.first().map(String::as_str) {
        Some(
            operation @ ("serve" | "ping" | "list" | "inspect" | "logs" | "start" | "stop"
            | "reload" | "shutdown"),
        ) => (operation.to_string(), args[1..].to_vec()),
        _ => ("serve".to_string(), args),
    };

    if operation == "serve" {
        let options = parse_options(&args)?;
        return daemon::serve(options);
    }

    let (socket, request) = parse_client(&operation, &args)?;
    let response = daemon::request(&socket, &request)?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if response["ok"] == false {
        std::process::exit(1);
    }
    Ok(())
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut socket = daemon::default_socket();
    let mut roots = Vec::new();
    let mut max_instances = DEFAULT_MAX_INSTANCES;
    let mut records_root = None;
    let mut book = None;
    let mut collection = None;
    let mut unibus_config = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = PathBuf::from(args.get(i + 1).context("--socket PATH")?);
                i += 2;
            }
            "--root" => {
                roots.push(PathBuf::from(args.get(i + 1).context("--root DIR")?));
                i += 2;
            }
            "--max-instances" => {
                max_instances = args
                    .get(i + 1)
                    .context("--max-instances N")?
                    .parse()
                    .context("--max-instances N")?;
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
            other => bail!("unknown loamd option {other}"),
        }
    }

    if roots.is_empty() {
        let command_root = std::env::var_os("FUNGOS_COMMAND_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".local/share/fungos/commands")
            });
        roots = soil::command::roots(command_root);
    }
    let records = match (records_root, book, collection) {
        (Some(root), Some(book), Some(collection)) => Some(RecordsBinding {
            root,
            book,
            collection,
        }),
        (None, None, None) => None,
        _ => bail!("records binding needs --records-root, --book and --collection"),
    };
    let bus = match unibus_config.or_else(|| {
        std::env::var_os("UNIBUS_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }) {
        Some(path) => {
            Some(Arc::new(soil::UnibusBus::from_config_file(&path)?) as Arc<dyn soil::Bus>)
        }
        None => None,
    };

    Ok(Options {
        socket,
        roots,
        records,
        bus,
        max_instances,
    })
}

fn parse_client(operation: &str, args: &[String]) -> Result<(PathBuf, Request)> {
    let mut socket = daemon::default_socket();
    let mut id = None;
    let mut command = None;
    let mut argv = Vec::new();
    let mut env = BTreeMap::new();
    let mut cwd = None;
    let mut stdin = None;
    let mut timeout_ms = None;
    let mut max_stdout_bytes = None;
    let mut max_stderr_bytes = None;
    let mut max_memory_bytes = None;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                socket = PathBuf::from(args.get(i + 1).context("--socket PATH")?);
                i += 2;
            }
            "--id" => {
                id = Some(args.get(i + 1).context("--id ID")?.clone());
                i += 2;
            }
            "--arg" => {
                argv.push(args.get(i + 1).context("--arg VALUE")?.clone());
                i += 2;
            }
            "--env" => {
                let assignment = args.get(i + 1).context("--env NAME=VALUE")?;
                let (name, value) = assignment
                    .split_once('=')
                    .with_context(|| format!("--env expects NAME=VALUE, got {assignment:?}"))?;
                env.insert(name.to_string(), value.to_string());
                i += 2;
            }
            "--cwd" => {
                cwd = Some(args.get(i + 1).context("--cwd DIR")?.clone());
                i += 2;
            }
            "--stdin" => {
                stdin = Some(args.get(i + 1).context("--stdin TEXT")?.clone());
                i += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    args.get(i + 1)
                        .context("--timeout-ms N")?
                        .parse()
                        .context("--timeout-ms N")?,
                );
                i += 2;
            }
            "--max-stdout-bytes" => {
                max_stdout_bytes = Some(
                    args.get(i + 1)
                        .context("--max-stdout-bytes N")?
                        .parse()
                        .context("--max-stdout-bytes N")?,
                );
                i += 2;
            }
            "--max-stderr-bytes" => {
                max_stderr_bytes = Some(
                    args.get(i + 1)
                        .context("--max-stderr-bytes N")?
                        .parse()
                        .context("--max-stderr-bytes N")?,
                );
                i += 2;
            }
            "--max-memory-bytes" => {
                max_memory_bytes = Some(
                    args.get(i + 1)
                        .context("--max-memory-bytes N")?
                        .parse()
                        .context("--max-memory-bytes N")?,
                );
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown loamd option {other}"),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    if operation == "start" {
        command = positional.first().cloned();
    } else if let Some(value) = positional.first() {
        id = Some(value.clone());
    }
    let request = match operation {
        "ping" => Request::Ping,
        "list" => Request::List,
        "inspect" => Request::Inspect {
            id: id.context("inspect needs ID")?,
        },
        "logs" => Request::Logs {
            id: id.context("logs needs ID")?,
        },
        "stop" => Request::Stop {
            id: id.context("stop needs ID")?,
        },
        "reload" => Request::Reload,
        "shutdown" => Request::Shutdown,
        "start" => {
            let command = command.context("start needs COMMAND")?;
            argv.extend(positional.into_iter().skip(1));
            Request::Start {
                id: id.unwrap_or_else(|| command.clone()),
                command,
                args: argv,
                env,
                cwd,
                stdin,
                timeout_ms,
                max_stdout_bytes,
                max_stderr_bytes,
                max_memory_bytes,
            }
        }
        other => bail!("unknown loamd operation {other}"),
    };
    Ok((socket, request))
}
