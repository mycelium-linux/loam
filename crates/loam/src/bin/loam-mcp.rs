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
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

fn main() {
    if let Err(error) = run() {
        eprintln!("loam-mcp: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut servers = BTreeMap::new();
    let mut auth_envs: BTreeMap<String, String> = BTreeMap::new();
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
            "--http" => {
                let spec = args.get(i + 1).context("--http id=url")?;
                let (id, url) = spec
                    .split_once('=')
                    .with_context(|| format!("--http expects id=url, got {spec:?}"))?;
                if id.is_empty() || url.trim().is_empty() {
                    bail!("--http needs a non-empty id and url");
                }
                servers.insert(
                    id.to_string(),
                    McpServer::http(url, auth_envs.get(id).cloned()),
                );
                i += 2;
            }
            "--auth-env" => {
                let spec = args.get(i + 1).context("--auth-env id=ENV")?;
                let (id, name) = spec
                    .split_once('=')
                    .with_context(|| format!("--auth-env expects id=ENV, got {spec:?}"))?;
                if id.is_empty() || name.trim().is_empty() {
                    bail!("--auth-env needs a non-empty id and env name");
                }
                auth_envs.insert(id.to_string(), name.to_string());
                if let Some(server) = servers.get_mut(id) {
                    if let Some(config) = &mut server.http {
                        config.auth_env = Some(name.to_string());
                    }
                }
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "usage: loam-mcp [--server ID=COMMAND]... [--http ID=URL [--auth-env ID=ENV]]..."
                );
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
    http: Option<HttpConfig>,
    http_state: Option<HttpState>,
}

struct HttpConfig {
    url: String,
    auth_env: Option<String>,
}

struct HttpState {
    url: String,
    auth_env: Option<String>,
    session: Option<String>,
    next_id: u64,
}

impl McpServer {
    fn loopback() -> Self {
        Self {
            command: None,
            child: None,
            http: None,
            http_state: None,
        }
    }

    fn new(command: &str) -> Self {
        Self {
            command: Some(command.to_string()),
            child: None,
            http: None,
            http_state: None,
        }
    }

    fn http(url: &str, auth_env: Option<String>) -> Self {
        Self {
            command: None,
            child: None,
            http: Some(HttpConfig {
                url: url.to_string(),
                auth_env,
            }),
            http_state: None,
        }
    }

    fn http_state(&mut self) -> std::result::Result<&mut HttpState, &'static str> {
        if self.http_state.is_none() {
            let config = self.http.as_ref().ok_or("unavailable")?;
            self.http_state = Some(
                HttpState::new(&config.url, config.auth_env.clone()).map_err(|_| "unavailable")?,
            );
        }
        self.http_state.as_mut().ok_or("unavailable")
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
        if self.http.is_some() {
            let state = self.http_state()?;
            let result = state
                .request("tools/list", json!({}))
                .map_err(|_| "unavailable")?;
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            return Ok(Value::Array(tools.into_iter().map(map_tool).collect()));
        }
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
        if self.http.is_some() {
            let state = self.http_state()?;
            return state
                .request("tools/call", json!({"name": tool, "arguments": arguments}))
                .map_err(|_| "unavailable");
        }
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

impl HttpState {
    fn new(url: &str, auth_env: Option<String>) -> Result<Self> {
        let mut state = Self {
            url: url.to_string(),
            auth_env,
            session: None,
            next_id: 1,
        };
        state.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "loam-mcp", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        state.notify("notifications/initialized", json!({}))?;
        Ok(state)
    }

    fn auth_header(&self) -> Result<Option<String>> {
        let Some(name) = &self.auth_env else {
            return Ok(None);
        };
        let value =
            std::env::var(name).with_context(|| format!("MCP auth env {name} is not set"))?;
        if value.trim().is_empty() {
            bail!("MCP auth env {name} is empty");
        }
        Ok(Some(format!("Bearer {value}")))
    }

    fn post(&mut self, body: &Value) -> Result<ureq::Response> {
        let mut request = ureq::post(&self.url)
            .timeout(Duration::from_secs(120))
            .set("content-type", "application/json")
            .set("accept", "application/json, text/event-stream");
        if let Some(auth) = self.auth_header()? {
            request = request.set("authorization", &auth);
        }
        if let Some(session) = &self.session {
            request = request.set("mcp-session-id", session);
        }
        let response = request
            .send_json(body)
            .map_err(|error| anyhow!("MCP HTTP request: {error}"))?;
        if let Some(session) = response.header("mcp-session-id") {
            self.session = Some(session.to_string());
        }
        Ok(response)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.post(&body)?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self.post(&body)?;
        let text = response.into_string().context("read MCP HTTP response")?;
        let mut parsed: Option<Value> = None;
        for line in text.lines() {
            if let Some(payload) = line.strip_prefix("data: ") {
                if let Ok(value) = serde_json::from_str::<Value>(payload) {
                    parsed = Some(value);
                }
            }
        }
        if parsed.is_none() {
            parsed = serde_json::from_str(&text).ok();
        }
        let response = parsed.ok_or_else(|| {
            anyhow!(
                "MCP HTTP returned no JSON: {}",
                text.chars().take(300).collect::<String>()
            )
        })?;
        if let Some(error) = response.get("error") {
            bail!("MCP error: {error}");
        }
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            bail!("MCP HTTP answered the wrong request id");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
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
