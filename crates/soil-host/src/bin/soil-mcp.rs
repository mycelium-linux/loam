//! The fleet's MCP endpoint: installed Spores, projected as tools.
//!
//! One address - `mcp.myc` - rather than one stdio server per process. A client
//! that reaches this listener gets the tools of every `--spore` it was started
//! with, named `<spore>_<subcommand>`, and a `tools/call` runs that Spore's own
//! component through the same bounded HTTP host everything else uses.
//!
//! Three rules this keeps:
//!
//! - **The manifest is the tool list.** Nothing here is authored beside
//!   `[[subcommands]]`, so a tool cannot describe an argument the CLI does not
//!   take.
//! - **The host is the authority.** A call is one `run(args)` through
//!   `HttpRuntime::run_cli`, with the policy this process was started with;
//!   the Spore cannot widen it.
//! - **Text in, text out.** A tool result is what the Spore printed. `--json`
//!   is how a caller asks for structure, instead of a second return type.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use soil_host::http_host::{HttpHost, HttpPolicy, HttpRuntime};
use soil_spore::Manifest;
use tiny_http::{Header, Response, Server};

const PROTOCOL: &str = "2025-06-18";

/// One tool, with everything needed to run it.
struct Tool {
    spore: Manifest,
    root: PathBuf,
    command: String,
    description: String,
    schema: Value,
}

struct State {
    runtime: HttpRuntime,
    policy: HttpPolicy,
    tools: BTreeMap<String, Tool>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("soil-mcp: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut spores = Vec::new();
    let mut origins = Vec::new();
    let mut identities = Vec::new();
    let mut listen = "127.0.0.1:8000".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--spore" => spores.push(PathBuf::from(args.next().context("--spore DIR")?)),
            "--allow" => origins.push(args.next().context("--allow ORIGIN")?),
            "--mesh-identity" => identities.push(args.next().context("--mesh-identity")?),
            "--listen" => listen = args.next().context("--listen ADDR")?,
            other => anyhow::bail!("unknown option {other}"),
        }
    }
    if spores.is_empty() {
        anyhow::bail!(
            "usage: soil-mcp --spore DIR [--spore DIR]... [--listen ADDR] [--allow ORIGIN]... [--mesh-identity ORIGIN]..."
        );
    }
    let mut policy = HttpPolicy::new(origins, 4 * 1024 * 1024, Duration::from_secs(30))?;
    for origin in &identities {
        policy.require_mesh_identity(origin)?;
    }

    let mut tools = BTreeMap::new();
    for root in &spores {
        let manifest = Manifest::load(root).with_context(|| format!("load {}", root.display()))?;
        let server = manifest
            .mcp
            .as_ref()
            .and_then(|mcp| mcp.name.clone())
            .unwrap_or_else(|| manifest.spore.name.clone());
        for command in &manifest.subcommands {
            let name = format!("{}_{}", server, command.name).replace('-', "_");
            tools.insert(
                name,
                Tool {
                    command: command.name.clone(),
                    description: command.summary.clone(),
                    schema: soil_spore::mcp::schema(&manifest, command),
                    spore: manifest.clone(),
                    root: root.clone(),
                },
            );
        }
    }
    if tools.is_empty() {
        anyhow::bail!("no [[subcommands]] in the spores given; there is nothing to serve");
    }

    let state = Arc::new(State {
        runtime: HttpRuntime::new()?,
        policy,
        tools,
    });
    let server =
        Server::http(&listen).map_err(|error| anyhow::anyhow!("bind {listen}: {error}"))?;
    println!(
        "soil-mcp on {listen}: {} tools from {} spore(s)",
        state.tools.len(),
        spores.len()
    );
    for request in server.incoming_requests() {
        let state = state.clone();
        std::thread::spawn(move || {
            let mut request = request;
            let mut body = Vec::new();
            if request.as_reader().read_to_end(&mut body).is_err() {
                return;
            }
            let reply = handle(&state, &body);
            let header = Header::from_bytes("Content-Type", "application/json").unwrap();
            let response = match reply {
                Some(value) => Response::from_string(value.to_string()),
                // A notification has no reply, and MCP says so with 202.
                None => Response::from_string(String::new()).with_status_code(202),
            }
            .with_header(header);
            let _ = request.respond(response);
        });
    }
    Ok(())
}

/// One JSON-RPC message in, one reply out. `None` for a notification.
fn handle(state: &State, body: &[u8]) -> Option<Value> {
    let request: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => return Some(error_reply(Value::Null, -32700, &error.to_string())),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if request.get("id").is_none() {
        // `notifications/initialized` and friends: nothing to say.
        return None;
    }
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "soil-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": state
                .tools
                .iter()
                .map(|(name, tool)| json!({
                    "name": name,
                    "description": tool.description,
                    "inputSchema": tool.schema,
                }))
                .collect::<Vec<_>>(),
        })),
        "tools/call" => call(state, &params),
        other => Err((-32601, format!("no method {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_reply(id, code, &message),
    })
}

fn call(state: &State, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "tools/call needs a name".to_string()))?;
    let tool = state
        .tools
        .get(name)
        .ok_or((-32602, format!("no tool {name}")))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let argv = argv_for(tool, &arguments)?;
    let bytes = std::fs::read(tool.root.join("module.wasm"))
        .map_err(|error| (-32603, format!("{}: {error}", tool.root.display())))?;
    let host = HttpHost::new(state.policy.clone());
    let text = state
        .runtime
        .run_cli(&bytes, host, &argv)
        .map_err(|error| (-32603, format!("{error:#}")))?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

/// Arguments are spelled the way the CLI spells them, so a call is a shell
/// command with its words in a different order.
fn argv_for(tool: &Tool, arguments: &Value) -> Result<Vec<String>, (i64, String)> {
    let command = tool
        .spore
        .subcommands
        .iter()
        .find(|command| command.name == tool.command)
        .ok_or((-32603, "tool has no subcommand".to_string()))?;
    let mut argv = vec![command.name.clone()];
    for arg in &command.args {
        let Some(value) = arguments.get(&arg.name) else {
            if let Some(default) = &arg.default {
                argv.push(arg.name.clone());
                argv.push(default.clone());
            }
            continue;
        };
        if arg.is_option() {
            argv.push(arg.name.clone());
        }
        match value {
            Value::Bool(true) if !arg.is_option() => {
                return Err((-32602, format!("{} takes a value", arg.name)));
            }
            Value::Bool(true) => {}
            Value::Bool(false) => {
                // A false switch is the switch not being passed.
                if arg.is_option() {
                    argv.pop();
                }
            }
            Value::String(text) => argv.push(text.clone()),
            Value::Number(number) => argv.push(number.to_string()),
            other => {
                return Err((-32602, format!("{} is not a scalar: {other}", arg.name)));
            }
        }
    }
    Ok(argv)
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
