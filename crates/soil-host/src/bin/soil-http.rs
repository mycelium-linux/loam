//! Run one component that imports `soil:http` and exports `run(args)`.
//!
//! ```sh
//! soil-http --allow https://example.com component.wasm https://example.com/feed.xml
//! ```

use std::{env, fs, process::ExitCode, sync::Arc, time::Duration};

use soil_host::http_host::{BearerCredential, HttpHost, HttpPolicy, HttpRuntime};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("soil-http: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).peekable();
    let mut origins = Vec::new();
    let mut identities: Vec<String> = Vec::new();
    let mut cli = false;
    let mut bearer = None;
    while let Some(arg) = args.peek() {
        match arg.as_str() {
            "--allow" => {
                args.next();
                origins.push(args.next().ok_or("--allow requires ORIGIN")?);
            }
            "--mesh-identity" => {
                args.next();
                identities.push(args.next().ok_or("--mesh-identity requires ORIGIN")?);
            }
            "--cli" => {
                cli = true;
                args.next();
            }
            "--bearer-env" => {
                args.next();
                let name = args.next().ok_or("--bearer-env requires a VARIABLE")?;
                let token =
                    env::var(&name).map_err(|_| format!("--bearer-env: {name} is not set"))?;
                bearer = Some(BearerCredential::new(token));
            }
            _ => break,
        }
    }
    if origins.is_empty() {
        return Err(
            "usage: soil-http --allow ORIGIN [--allow ORIGIN]... [--bearer-env VARIABLE] [--mesh-identity ORIGIN]... COMPONENT.wasm [ARGS...]".into(),
        );
    }
    let path = args.next().ok_or("missing COMPONENT.wasm")?;
    let guest_args: Vec<String> = args.collect();
    let bytes = fs::read(&path).map_err(|error| format!("read {path}: {error}"))?;
    let mut policy = HttpPolicy::new(origins, 16 * 1024 * 1024, Duration::from_secs(30))
        .map_err(|error| format!("policy: {error:#}"))?;
    // A second decision, not a wider allowlist: an origin still has to be
    // allowed before it can be told who is calling.
    for origin in &identities {
        policy
            .require_mesh_identity(origin)
            .map_err(|error| format!("--mesh-identity: {error:#}"))?;
    }
    let runtime = HttpRuntime::new().map_err(|error| format!("runtime: {error:#}"))?;
    let host = match bearer {
        Some(credential) => HttpHost::new(policy).with_credentials(Arc::new(credential)),
        None => HttpHost::new(policy),
    };
    if cli {
        // A CLI Spore returns what to print; a smoke-test component returns
        // nothing and prints for itself. Two ABIs, one policy.
        let answer = runtime
            .run_cli(&bytes, host, &guest_args)
            .map_err(|error| format!("{error:#}"))?;
        print!("{answer}");
        return Ok(());
    }
    runtime
        .run_component(&bytes, host, &guest_args)
        .map_err(|error| format!("{error:#}"))
}
