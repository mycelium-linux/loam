//! A thin `soil:mcp` adapter over `loam:plugin/ipc`.
//!
//! The adapter owns no MCP transport. It converts the typed WIT calls into
//! JSON requests for the `loam-mcp` sidecar and maps the answers back into the
//! canonical `soil:mcp` types.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    generate_all,
});

use loam::plugin::ipc;
use serde_json::{Value, json};
use crate::exports::soil::mcp::mcp::Guest;
use crate::soil::mcp::types::{Error, Mutation, Server, Tool};

struct Component;

impl Guest for Component {
    fn servers() -> Result<Vec<Server>, Error> {
        let value = request("servers", Value::Null)?;
        let servers = value.as_array().ok_or(Error::Invalid)?;
        servers
            .iter()
            .map(|server| {
                Ok(Server {
                    id: string(server, "id")?,
                    title: string(server, "title")?,
                    lease_expires_at_ms: server
                        .get("leaseExpiresAtMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX),
                })
            })
            .collect()
    }

    fn tools(server: String) -> Result<Vec<Tool>, Error> {
        let value = request("tools", Value::String(server))?;
        let tools = value.as_array().ok_or(Error::Invalid)?;
        tools
            .iter()
            .map(|tool| {
                Ok(Tool {
                    name: string(tool, "name")?,
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: string(tool, "inputSchema")?,
                    output_schema: tool
                        .get("outputSchema")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    mutation: match tool
                        .get("mutation")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                    {
                        "read-only" => Mutation::ReadOnly,
                        "mutating" => Mutation::Mutating,
                        _ => Mutation::Unknown,
                    },
                })
            })
            .collect()
    }

    fn call(server: String, tool: String, arguments: String) -> Result<String, Error> {
        let value = request(
            "call",
            json!({"server": server, "tool": tool, "arguments": arguments}),
        )?;
        serde_json::to_string(&value).map_err(|_| Error::Invalid)
    }
}

fn request(method: &str, payload: Value) -> Result<Value, Error> {
    let payload = serde_json::to_vec(&payload).map_err(|_| Error::Invalid)?;
    let bytes = ipc::request(method, &payload).map_err(map_error)?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)
}

fn map_error(error: ipc::Error) -> Error {
    match error {
        ipc::Error::Denied => Error::Denied,
        ipc::Error::Unavailable => Error::Unavailable,
        ipc::Error::Invalid => Error::Invalid,
        ipc::Error::Io => Error::Io,
    }
}

fn string(value: &Value, key: &str) -> Result<String, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(Error::Invalid)
}

export!(Component);
