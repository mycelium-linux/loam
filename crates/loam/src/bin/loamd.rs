// SPDX-License-Identifier: MIT OR Apache-2.0
//! The per-user loam supervisor and its local control client.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use soil::{
    command::RecordsBinding,
    config::Config,
    daemon::{self, Options, Request},
};

const DEFAULT_MAX_INSTANCES: usize = 8;

fn usage() -> &'static str {
    r#"usage: loamd [serve] [--config FILE] [--socket PATH] [--root DIR]...
                     [--log-dir DIR] [--log-max-bytes N] [--log-keep N]
                     [--mycelium-home DIR] [--no-publish-leases]
       loamd start COMMAND [ARGS...] [--id ID] [--context ID] [--socket PATH]
       loamd image plan SPORE [--socket PATH]
       loamd image start SPORE [--id ID] [--context ID] [--socket PATH]
       loamd image eval ID SOURCE [--socket PATH]
       loamd image status ID [--socket PATH]
       loamd image stop ID [--socket PATH]
       loamd image list [--socket PATH]
       loamd window start URL [--id ID] [--title TITLE] [--size WxH] [--socket PATH]
       loamd window list [--socket PATH]
       loamd window stop ID [--socket PATH]
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
            | "reload" | "shutdown" | "image" | "window"),
        ) => (operation.to_string(), args[1..].to_vec()),
        _ => ("serve".to_string(), args),
    };

    if operation == "serve" {
        let options = parse_options(&args)?;
        return daemon::serve(options);
    }

    if operation == "image" {
        let (socket, request) = parse_image(&args)?;
        let response = daemon::request(&socket, &request)?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        if response["ok"] == false {
            std::process::exit(1);
        }
        return Ok(());
    }

    if operation == "window" {
        let (socket, request) = parse_window(&args)?;
        let response = daemon::request(&socket, &request)?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        if response["ok"] == false {
            std::process::exit(1);
        }
        return Ok(());
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
    let config = match config_path(args)? {
        Some(path) => Config::load(&path)?,
        None => Config::default(),
    };
    let mut socket = config.socket.unwrap_or_else(daemon::default_socket);
    let mut roots = config.roots;
    let mut max_instances = config.max_instances.unwrap_or(DEFAULT_MAX_INSTANCES);
    let mut records_root = None;
    let mut book = None;
    let mut collection = None;
    let mut unibus_config = None;
    let mut web_listen = config.web_listen;
    let mut image_runtime = config.image_runtime;
    let mut window_runtime = config.window_runtime;
    let mut log_dir = config.log_root;
    let mut log_max_bytes = config.log_max_bytes.unwrap_or(soil::log::DEFAULT_MAX_BYTES);
    let mut log_keep = config.log_keep.unwrap_or(soil::log::DEFAULT_KEEP);
    let mut mycelium_home = config.mycelium_home;
    let mut publish_leases = config.publish_leases.unwrap_or(true);
    let stream = config.stream.clone();
    let mut roots_from_cli = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                // Loaded before the loop, so this only consumes its value.
                let _ = args.get(i + 1).context("--config FILE")?;
                i += 2;
            }
            "--socket" => {
                socket = PathBuf::from(args.get(i + 1).context("--socket PATH")?);
                i += 2;
            }
            "--root" => {
                if !roots_from_cli {
                    roots.clear();
                    roots_from_cli = true;
                }
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
            "--web-listen" => {
                web_listen = Some(args.get(i + 1).context("--web-listen ADDR")?.clone());
                i += 2;
            }
            "--image-runtime" => {
                image_runtime = Some(PathBuf::from(
                    args.get(i + 1).context("--image-runtime PATH")?,
                ));
                i += 2;
            }
            "--window-runtime" => {
                window_runtime = Some(PathBuf::from(
                    args.get(i + 1).context("--window-runtime PATH")?,
                ));
                i += 2;
            }
            "--log-dir" => {
                log_dir = Some(PathBuf::from(args.get(i + 1).context("--log-dir DIR")?));
                i += 2;
            }
            "--log-max-bytes" => {
                log_max_bytes = args
                    .get(i + 1)
                    .context("--log-max-bytes N")?
                    .parse()
                    .context("--log-max-bytes N")?;
                i += 2;
            }
            "--log-keep" => {
                log_keep = args
                    .get(i + 1)
                    .context("--log-keep N")?
                    .parse()
                    .context("--log-keep N")?;
                i += 2;
            }
            "--mycelium-home" => {
                mycelium_home = Some(PathBuf::from(
                    args.get(i + 1).context("--mycelium-home DIR")?,
                ));
                i += 2;
            }
            "--no-publish-leases" => {
                publish_leases = false;
                i += 1;
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
        image_runtime,
        window_runtime,
        web_listen,
        log_root: log_dir,
        log_max_bytes,
        log_keep,
        mycelium_home,
        publish_leases,
        stream,
    })
}

fn config_path(args: &[String]) -> Result<Option<PathBuf>> {
    let mut explicit = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" {
            let path = PathBuf::from(args.get(i + 1).context("--config FILE")?);
            if explicit.replace(path).is_some() {
                bail!("--config may be given once");
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    if explicit.is_some() {
        return Ok(explicit);
    }
    if let Some(path) = std::env::var_os("LOAM_CONFIG").filter(|value| !value.is_empty()) {
        return Ok(Some(PathBuf::from(path)));
    }
    Ok(Config::default_path().filter(|path| path.is_file()))
}

fn parse_image(args: &[String]) -> Result<(PathBuf, Request)> {
    let mut socket = daemon::default_socket();
    let mut id = None;
    let mut context = None;
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
            "--context" => {
                context = Some(args.get(i + 1).context("--context ID")?.clone());
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown image option {other}"),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    let command = positional.first().map(String::as_str).unwrap_or("list");
    let request = match command {
        "list" => Request::ImageList,
        "plan" => Request::ImagePlan {
            spore: positional.get(1).context("image plan needs SPORE")?.clone(),
        },
        "start" => {
            let spore = positional
                .get(1)
                .context("image start needs SPORE")?
                .clone();
            let id = id.unwrap_or_else(|| {
                PathBuf::from(&spore)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".to_string())
            });
            Request::ImageStart { id, spore, context }
        }
        "eval" => {
            let id = positional.get(1).context("image eval needs ID")?.clone();
            let source = positional
                .get(2)
                .context("image eval needs SOURCE")?
                .clone();
            Request::ImageEval { id, source }
        }
        "status" => Request::ImageStatus {
            id: positional.get(1).context("image status needs ID")?.clone(),
        },
        "stop" => Request::ImageStop {
            id: positional.get(1).context("image stop needs ID")?.clone(),
        },
        other => bail!("unknown image operation {other}"),
    };
    Ok((socket, request))
}

fn parse_window(args: &[String]) -> Result<(PathBuf, Request)> {
    let mut socket = daemon::default_socket();
    let mut id = None;
    let mut title = "Loam".to_string();
    let mut size = (1000, 700);
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
            "--title" => {
                title = args.get(i + 1).context("--title TITLE")?.clone();
                i += 2;
            }
            "--size" => {
                let value = args.get(i + 1).context("--size WxH")?;
                let (width, height) = value.split_once('x').context("--size needs WxH")?;
                size = (
                    width.parse().context("--size width")?,
                    height.parse().context("--size height")?,
                );
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown window option {other}"),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    let command = positional.first().map(String::as_str).unwrap_or("list");
    let request = match command {
        "list" => Request::WindowList,
        "start" => {
            let url = positional.get(1).context("window start needs URL")?.clone();
            let id = id.unwrap_or_else(|| format!("window-{}", positional.len()));
            Request::WindowStart {
                id,
                url,
                title,
                width: size.0,
                height: size.1,
            }
        }
        "stop" => Request::WindowStop {
            id: positional.get(1).context("window stop needs ID")?.clone(),
        },
        other => bail!("unknown window operation {other}"),
    };
    Ok((socket, request))
}

fn parse_client(operation: &str, args: &[String]) -> Result<(PathBuf, Request)> {
    let mut socket = daemon::default_socket();
    let mut id = None;
    let mut context = None;
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
            "--context" => {
                context = Some(args.get(i + 1).context("--context ID")?.clone());
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
                context,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_supplies_defaults_and_cli_overrides_them() {
        let temp = tempfile::tempdir().expect("temp");
        let config = temp.path().join("loam.conf.soil");
        std::fs::write(
            &config,
            format!(
                r#"(loam
                     (version 1)
                     (daemon
                       (socket "{socket}")
                       (max-instances 3)
                       (roots ("/config-root"))
                       (log (dir "{logs}") (max-bytes 4096) (keep 9))
                       (web (listen "127.0.0.1:9000")))
                     (mycelium (publish-leases #t)))"#,
                socket = temp.path().join("loamd.sock").display(),
                logs = temp.path().join("logs").display(),
            ),
        )
        .expect("config");
        let options = parse_options(&[
            "--config".into(),
            config.display().to_string(),
            "--root".into(),
            "/cli-root".into(),
            "--log-keep".into(),
            "2".into(),
            "--no-publish-leases".into(),
        ])
        .expect("options");
        assert_eq!(options.socket, temp.path().join("loamd.sock"));
        assert_eq!(options.max_instances, 3);
        assert_eq!(options.roots, [PathBuf::from("/cli-root")]);
        assert_eq!(options.web_listen.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(options.log_root, Some(temp.path().join("logs")));
        assert_eq!(options.log_max_bytes, 4096);
        assert_eq!(options.log_keep, 2);
        assert!(!options.publish_leases);
    }
}
