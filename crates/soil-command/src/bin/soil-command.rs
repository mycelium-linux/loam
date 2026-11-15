use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use soil_command::{CommandRegistry, CommandRunner, HttpBinding, RecordsBinding, RunRequest};
use soil_host::http_host::HttpPolicy;

/// A CLI command's response cap and deadline. Both belong to the host: a Spore
/// that could name them could hold a pipe open or pull a body it was never
/// granted.
const HTTP_MAX_BYTES: usize = 4 * 1024 * 1024;
const HTTP_TIMEOUT_SECS: u64 = 30;

fn main() {
    if let Err(error) = run() {
        eprintln!("soil-command: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = std::env::var_os("FUNGOS_COMMAND_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/share/fungos/commands")
        });
    let mut records_root = None;
    let mut book = None;
    let mut collection = None;
    let mut http_allow: Vec<String> = Vec::new();
    let mut http_identity: Vec<String> = Vec::new();
    let mut list = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = PathBuf::from(args.get(i + 1).context("--root DIR")?);
                i += 2;
            }
            "--http-allow" => {
                http_allow.push(args.get(i + 1).context("--http-allow ORIGIN")?.clone());
                i += 2;
            }
            "--http-mesh-identity" => {
                http_identity.push(
                    args.get(i + 1)
                        .context("--http-mesh-identity ORIGIN")?
                        .clone(),
                );
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
            "--list" => {
                list = true;
                i += 1;
            }
            "run" => {
                i += 1;
                break;
            }
            other => bail!("unknown option {other}"),
        }
    }
    let registry = CommandRegistry::scan(&root)?;
    if list {
        for entry in registry.entries() {
            println!("{}\t{}\t{}", entry.name, entry.version, entry.summary);
        }
        return Ok(());
    }
    let Some(name) = args.get(i).cloned() else {
        bail!("usage: soil-command [--list] run NAME [ARGS...]");
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
    let mut stdin = Vec::new();
    std::io::stdin().read_to_end(&mut stdin)?;
    let http = if http_allow.is_empty() {
        if !http_identity.is_empty() {
            bail!("--http-mesh-identity needs an origin in --http-allow");
        }
        None
    } else {
        // The origin allowlist is the command's reachability; the identity list
        // is which of those origins it answers as this node. A body cap and a
        // deadline belong to the host because a command cannot be trusted to
        // pick them for itself.
        let mut policy = HttpPolicy::new(
            http_allow.clone(),
            HTTP_MAX_BYTES,
            std::time::Duration::from_secs(HTTP_TIMEOUT_SECS),
        )?;
        for origin in &http_identity {
            policy.require_mesh_identity(origin)?;
        }
        Some(HttpBinding { policy })
    };
    let output = CommandRunner::new(records, http)?.run(
        &entry,
        &RunRequest {
            args: args[i + 1..].to_vec(),
            stdin,
            // Commands do not inherit the host environment.
            env: Vec::new(),
            cwd: None,
        },
    )?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if output.code != 0 {
        std::process::exit(output.code);
    }
    Ok(())
}
