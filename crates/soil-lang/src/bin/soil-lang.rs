// SPDX-License-Identifier: MIT OR Apache-2.0
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use soil_lang::sexp::Value;
use soil_lang::{FakeHost, Host, HostChain, NativeHost, Runtime, UnibusHost};

fn run() -> Result<()> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut prelude: Option<PathBuf> = None;
    let mut call: Option<String> = Some("main".to_string());
    let mut records_root: Option<PathBuf> = None;
    let mut workspace_root: Option<PathBuf> = None;
    let mut book = "soil".to_string();
    let mut repl = false;
    let mut image = false;
    let mut http_origins: Vec<String> = Vec::new();
    let mut http_credential: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut geode_socket: Option<PathBuf> = None;
    let mut geode_reference: Option<String> = None;
    let mut geode_purpose: Option<String> = None;
    let mut unibus_config: Option<PathBuf> = None;
    let mut unibus_hubs: Vec<String> = Vec::new();
    let mut repeat: u64 = 1;
    let mut interval_ms: u64 = 0;
    let mut program_args: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--call" => {
                call = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--call needs a name"))?,
                );
            }
            "--no-call" => call = None,
            "--prelude" => {
                prelude = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--prelude needs a file"))?,
                ));
            }
            "--records-root" => {
                records_root =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--records-root needs a path")
                    })?));
            }
            "--workspace-root" => {
                workspace_root =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--workspace-root needs a path")
                    })?));
            }
            "--book" => {
                book = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--book needs a name"))?;
            }
            "--repl" => repl = true,
            "--image" => image = true,
            "--allow" => {
                http_origins.push(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--allow needs an origin"))?,
                );
            }
            "--http-credential" => {
                http_credential = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--http-credential needs a value"))?,
                );
            }
            "--http-credential-env" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--http-credential-env needs a variable"))?;
                let value = std::env::var(&name)
                    .map_err(|_| anyhow::anyhow!("--http-credential-env: {name} is not set"))?;
                http_credential = Some(value);
            }
            "--bearer-env" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bearer-env needs a variable"))?;
                let value = std::env::var(&name)
                    .map_err(|_| anyhow::anyhow!("--bearer-env: {name} is not set"))?;
                bearer = Some(value);
            }
            "--geode-socket" => {
                geode_socket =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--geode-socket needs a path")
                    })?));
            }
            "--geode-reference" => {
                geode_reference = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--geode-reference needs a reference"))?,
                );
            }
            "--geode-purpose" => {
                geode_purpose = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--geode-purpose needs a purpose"))?,
                );
            }
            "--unibus-config" => {
                unibus_config =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--unibus-config needs a path")
                    })?));
            }
            "--hub" => {
                unibus_hubs.push(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--hub needs an address"))?,
                );
            }
            "--repeat" => {
                repeat = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--repeat needs a count (0 = forever)"))?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--repeat needs a non-negative integer"))?;
            }
            "--interval-ms" => {
                interval_ms = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--interval-ms needs a duration"))?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--interval-ms needs a non-negative integer"))?;
            }
            "--" => {
                program_args.extend(args.by_ref());
                break;
            }
            "-h" | "--help" => {
                println!(
                    "usage: soil-lang [--call NAME | --no-call] [--repl | --image] [--prelude FILE] \
                     [--records-root DIR] [--book NAME] [--workspace-root DIR] \
                     [--allow ORIGIN]... \
                     [--http-credential VALUE | --http-credential-env VAR \
                      | --bearer-env VAR \
                      | --geode-socket PATH --geode-reference REF --geode-purpose PURPOSE] \
                     [--unibus-config FILE] [--hub ADDR]... \
                     [--repeat N] [--interval-ms MS] \
                     FILE... [-- ARG...]"
                );
                return Ok(());
            }
            other => files.push(PathBuf::from(other)),
        }
    }

    if files.is_empty() && prelude.is_none() && !repl {
        bail!(
            "usage: soil-lang [--call NAME | --no-call] [--repl] [--prelude FILE] \
             [--records-root DIR] [--book NAME] [--workspace-root DIR] \
             [--allow ORIGIN]... \
             [--http-credential VALUE | --http-credential-env VAR \
              | --bearer-env VAR \
              | --geode-socket PATH --geode-reference REF --geode-purpose PURPOSE] \
             [--unibus-config FILE] [--hub ADDR]... \
             [--repeat N] [--interval-ms MS] \
             FILE... [-- ARG...]"
        );
    }

    let wants_geode =
        geode_socket.is_some() || geode_reference.is_some() || geode_purpose.is_some();
    if wants_geode
        && (geode_socket.is_none() || geode_reference.is_none() || geode_purpose.is_none())
    {
        bail!("--geode-socket, --geode-reference and --geode-purpose are used together");
    }
    if wants_geode && (http_credential.is_some() || bearer.is_some()) {
        bail!("--geode-* and the other credentials are mutually exclusive");
    }
    if http_credential.is_some() && bearer.is_some() {
        bail!("--http-credential and --bearer-env are mutually exclusive");
    }
    let wants_http =
        !http_origins.is_empty() || http_credential.is_some() || bearer.is_some() || wants_geode;
    if wants_http && http_origins.is_empty() {
        bail!("a credential needs at least one --allow ORIGIN");
    }
    if wants_http && (workspace_root.is_none() || records_root.is_none()) {
        bail!("--allow needs --workspace-root and --records-root");
    }
    if unibus_config.is_none() && !unibus_hubs.is_empty() {
        bail!("--hub needs --unibus-config");
    }

    let mut host: Box<dyn Host> = match (workspace_root, records_root) {
        (Some(workspace), Some(records)) => {
            let mut native = NativeHost::open(workspace, records, book)?;
            if wants_geode {
                #[cfg(unix)]
                {
                    native = native.with_geode_http(
                        http_origins,
                        geode_socket.expect("validated"),
                        geode_reference.expect("validated"),
                        geode_purpose.expect("validated"),
                    )?;
                }
                #[cfg(not(unix))]
                {
                    bail!("--geode-socket is only available on Unix hosts");
                }
            } else if let Some(token) = bearer {
                native = native.with_bearer_http(http_origins, token)?;
            } else if wants_http {
                native = native.with_http(http_origins, http_credential)?;
            }
            Box::new(native)
        }
        (Some(_), None) => bail!("--workspace-root requires --records-root"),
        (None, Some(records)) => Box::new(NativeHost::open(".", records, book)?),
        (None, None) => Box::new(FakeHost::new()),
    };
    if let Some(path) = unibus_config {
        host = Box::new(HostChain::new(vec![
            host,
            Box::new(UnibusHost::connect(path, &unibus_hubs)?),
        ]));
    }
    let mut runtime = Runtime::new(host);
    runtime.set_args(program_args);

    if let Some(prelude) = prelude {
        runtime.load_file(prelude)?;
    }
    let mut last = Value::Nil;
    for file in files {
        last = runtime.load_file(&file)?;
    }

    if image {
        return run_image(&mut runtime);
    }

    if repl {
        return run_repl(&mut runtime);
    }

    if let Some(name) = call {
        if runtime.has_function(&name) {
            let mut iteration = 0_u64;
            loop {
                let value = runtime.call(&name, &[])?;
                if !matches!(value, Value::Nil) {
                    println!("{value}");
                }
                iteration += 1;
                if repeat != 0 && iteration >= repeat {
                    break;
                }
                if interval_ms > 0 {
                    std::thread::sleep(Duration::from_millis(interval_ms));
                }
            }
        } else if !matches!(last, Value::Nil) {
            println!("{last}");
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    // The evaluator is recursive over lists, and real records books hold
    // thousands of issues. Give the interpreter enough stack for one pass
    // instead of aborting on the 512-entry scan page.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run)?
        .join()
        .map_err(|_| anyhow::anyhow!("soil-lang worker panicked"))?
}

/// A persistent image pipe: one form per line, one result per line, no prompt.
/// The runtime and its function cells survive from one form to the next.
fn run_image(runtime: &mut Runtime) -> Result<()> {
    let stdin = io::stdin();
    let mut buffer = String::new();
    loop {
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(());
        }
        buffer.push_str(&line);
        match runtime.load(&buffer) {
            Ok(value) => {
                if !matches!(value, Value::Nil) {
                    println!("{value}");
                }
                buffer.clear();
            }
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("unterminated list") || message.contains("unterminated string")
                {
                    continue;
                }
                eprintln!("{message}");
                buffer.clear();
            }
        }
    }
}

fn run_repl(runtime: &mut Runtime) -> Result<()> {
    let stdin = io::stdin();
    let mut buffer = String::new();
    loop {
        print!("{}", if buffer.is_empty() { "> " } else { "... " });
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if buffer.is_empty() && line.starts_with(':') {
            let trimmed = line.trim_end();
            let mut parts = trimmed.splitn(2, ' ');
            let command = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            match command {
                ":quit" | ":q" => return Ok(()),
                ":help" | ":h" => println!(
                    ":stats          eval/host counters for this image\n\
                     :time FORM      evaluate and print elapsed ms\n\
                     :bench N FORM   evaluate FORM N times\n\
                     :reset          reset counters\n\
                     :quit           leave"
                ),
                ":stats" => {
                    println!("{value}", value = runtime.load("(runtime-stats)")?);
                }
                ":reset" => {
                    runtime.load("(runtime-reset)")?;
                    println!("stats reset");
                }
                ":time" => {
                    let started = std::time::Instant::now();
                    match runtime.load(rest) {
                        Ok(value) => {
                            println!("{value}  ({ms} ms)", ms = started.elapsed().as_millis())
                        }
                        Err(error) => eprintln!("{error:#}"),
                    }
                }
                ":bench" => {
                    let mut bits = rest.splitn(2, ' ');
                    let runs = bits.next().unwrap_or("10").parse::<u64>().unwrap_or(10);
                    let form = bits.next().unwrap_or("");
                    let started = std::time::Instant::now();
                    for _ in 0..runs {
                        if let Err(error) = runtime.load(form) {
                            eprintln!("{error:#}");
                            break;
                        }
                    }
                    println!(
                        "{runs} run(s) in {ms} ms",
                        ms = started.elapsed().as_millis()
                    );
                }
                _ => eprintln!(
                    "unknown REPL command {command}; try :stats, :time FORM, :bench N FORM, :reset, :quit"
                ),
            }
            buffer.clear();
            continue;
        }
        buffer.push_str(&line);
        match runtime.load(&buffer) {
            Ok(value) => {
                if !matches!(value, Value::Nil) {
                    println!("{value}");
                }
                buffer.clear();
            }
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("unterminated list") || message.contains("unterminated string")
                {
                    continue;
                }
                eprintln!("{message}");
                buffer.clear();
            }
        }
    }
}
