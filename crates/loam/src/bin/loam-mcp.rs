//! A small MCP stdio sidecar.
//!
//! The plugin adapter speaks `loam:plugin/ipc` to this process. This process
//! owns the MCP child processes, their initialization, and their JSON-RPC
//! request ids. The adapter never sees a pid or a socket.
//!
//! With no `--server`, the sidecar exposes a loopback catalog so the plugin
//! path can be tested without a network or a child process. With
//! `--server id=command args...`, it speaks newline-delimited JSON-RPC to that
//! child.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

fn main() {
    if let Err(error) = run() {
        eprintln!("loam-mcp: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut servers = BTreeMap::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" => {
                let spec = args.get(i + 1).context("--server id=command")?;
                let (id, command) = spec
                    .split_once('=')
                    .with_context(|| format!("--server expects id=command, got {spec:?}"))?;
                if id.is_empty() || command.trim().is_empty() {
                    bail!("--server needs a non-empty id and command");
                }
                servers.insert(id.to_string(), McpServer::new(command));
                i += 2;
            }
            "--help" | "-h" => {
                println!("usage: loam-mcp [--server ID=COMMAND]...");
                return Ok(());
            }
            other => bail!("unknown argument {other}"),
        }
    }
    if servers.is_empty() {
        servers.insert("loopback".into(), McpServer::loopback());
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"ok": false, "error": "invalid", "message": error.to_string()})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = request.get("payload").cloned().unwrap_or(Value::Null);
        let response = match handle(&mut servers, method, payload) {
            Ok(payload) => json!({"ok": true, "payload": payload}),
            Err(error) => json!({"ok": false, "error": error}),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle(
    servers: &mut BTreeMap<String, McpServer>,
    method: &str,
    payload: Value,
) -> std::result::Result<Value, &'static str> {
    match method {
        "servers" => Ok(Value::Array(
            servers
                .keys()
                .map(|id| {
                    json!({
                        "id": id,
                        "title": id,
                        "leaseExpiresAtMs": u64::MAX,
                    })
                })
                .collect(),
        )),
        "tools" => {
            let server = payload.as_str().ok_or("invalid")?;
            let server = servers.get_mut(server).ok_or("not-found")?;
            server.tools()
        }
        "call" => {
            let server = payload
                .get("server")
                .and_then(Value::as_str)
                .ok_or("invalid")?;
            let tool = payload
                .get("tool")
                .and_then(Value::as_str)
                .ok_or("invalid")?;
            let arguments = payload
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or("invalid")?;
            let server = servers.get_mut(server).ok_or("not-found")?;
            server.call(tool, arguments)
        }
        _ => Err("invalid"),
    }
}

struct McpServer {
    command: Option<String>,
    child: Option<McpChild>,
}

impl McpServer {
    fn loopback() -> Self {
        Self {
            command: None,
            child: None,
        }
    }

    fn new(command: &str) -> Self {
        Self {
            command: Some(command.to_string()),
            child: None,
        }
    }

    fn child(&mut self) -> std::result::Result<&mut McpChild, &'static str> {
        if self.child.is_none() {
            let command = self.command.as_deref().ok_or("unavailable")?;
            let mut parts = command.split_whitespace();
            let program = parts.next().ok_or("unavailable")?;
            let child = McpChild::spawn(program, parts).map_err(|_| "unavailable")?;
            self.child = Some(child);
        }
        self.child.as_mut().ok_or("unavailable")
    }

    fn tools(&mut self) -> std::result::Result<Value, &'static str> {
        if self.command.is_none() {
            return Ok(json!([
                {
                    "name": "echo",
                    "description": "Echo JSON arguments",
                    "inputSchema": "{\"type\":\"object\"}",
                    "outputSchema": null,
                    "mutation": "read-only"
                },
                {
                    "name": "write-note",
                    "description": "A mutating example",
                    "inputSchema": "{\"type\":\"object\",\"required\":[\"text\"]}",
                    "outputSchema": null,
                    "mutation": "mutating"
                }
            ]));
        }
        let result = self
            .child()?
            .request("tools/list", json!({}))
            .map_err(|_| "unavailable")?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(Value::Array(tools.into_iter().map(map_tool).collect()))
    }

    fn call(&mut self, tool: &str, arguments: &str) -> std::result::Result<Value, &'static str> {
        let arguments: Value = serde_json::from_str(arguments).map_err(|_| "invalid")?;
        if self.command.is_none() {
            return Ok(json!({
                "tool": tool,
                "arguments": arguments,
            }));
        }
        let result = self
            .child()?
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .map_err(|_| "unavailable")?;
        Ok(result)
    }
}

fn map_tool(tool: Value) -> Value {
    let read_only = tool
        .get("annotations")
        .and_then(|annotations| annotations.get("readOnlyHint"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "description": tool.get("description").cloned().unwrap_or(Value::Null),
        "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null).to_string(),
        "outputSchema": tool.get("outputSchema").map(Value::to_string),
        "mutation": if read_only { "read-only" } else { "unknown" },
    })
}

struct McpChild {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpChild {
    fn spawn<'a>(program: &str, args: impl Iterator<Item = &'a str>) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn MCP server {program}"))?;
        let input = child.stdin.take().context("MCP server stdin")?;
        let output = child.stdout.take().context("MCP server stdout")?;
        let mut server = Self {
            child,
            input,
            output: BufReader::new(output),
            next_id: 1,
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "loam-mcp", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        server.notify("notifications/initialized", json!({}))?;
        Ok(server)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let request = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.input, "{request}")?;
        self.input.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.input, "{request}")?;
        self.input.flush()?;
        for _ in 0..64 {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                bail!("MCP server closed stdout");
            }
            let Ok(response) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                bail!("MCP error: {error}");
            }
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }
        bail!("MCP server did not answer {method}")
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_catalog_and_call() {
        let mut servers = BTreeMap::new();
        servers.insert("loopback".into(), McpServer::loopback());
        let catalog = handle(&mut servers, "servers", Value::Null).unwrap();
        assert_eq!(catalog.as_array().unwrap().len(), 1);
        let tools = handle(&mut servers, "tools", json!("loopback")).unwrap();
        assert!(
            tools
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "echo")
        );
        let result = handle(
            &mut servers,
            "call",
            json!({"server": "loopback", "tool": "echo", "arguments": "{\"x\":1}"}),
        )
        .unwrap();
        assert_eq!(result["tool"], "echo");
    }
}
