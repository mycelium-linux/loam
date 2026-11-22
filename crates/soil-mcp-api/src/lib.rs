// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transport-agnostic MCP catalog and invocation seam.
//!
//! Unibus, streamable HTTP, or stdio adapters implement `Backend`. The WIT
//! stays generic; typed Soil wrappers are generated from the catalog.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    ReadOnly,
    Mutating,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub id: String,
    pub title: String,
    pub lease_expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: String,
    pub output_schema: Option<String>,
    pub mutation: Mutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Denied,
    NotFound(String),
    Invalid(String),
    OverBudget,
    Unavailable,
    Io(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Denied => write!(f, "denied"),
            Failure::NotFound(what) => write!(f, "not found: {what}"),
            Failure::Invalid(why) => write!(f, "invalid: {why}"),
            Failure::OverBudget => write!(f, "over budget"),
            Failure::Unavailable => write!(f, "unavailable"),
            Failure::Io(why) => write!(f, "io: {why}"),
        }
    }
}

impl std::error::Error for Failure {}

pub trait Backend: Send {
    fn servers(&mut self) -> Result<Vec<Server>, Failure>;
    fn tools(&mut self, server: &str) -> Result<Vec<Tool>, Failure>;
    fn call(&mut self, server: &str, tool: &str, arguments: &str) -> Result<String, Failure>;
}

/// A loopback catalog for tests and for generated-wrapper development.
#[derive(Debug, Clone)]
pub struct Loopback {
    server: Server,
    tools: Vec<Tool>,
}

impl Default for Loopback {
    fn default() -> Self {
        Self {
            server: Server {
                id: "loopback".into(),
                title: "Loopback".into(),
                lease_expires_at_ms: u64::MAX,
            },
            tools: vec![
                Tool {
                    name: "echo".into(),
                    description: "Echo JSON arguments".into(),
                    input_schema: r#"{"type":"object"}"#.into(),
                    output_schema: None,
                    mutation: Mutation::ReadOnly,
                },
                Tool {
                    name: "write-note".into(),
                    description: "A mutating example".into(),
                    input_schema: r#"{"type":"object","required":["text"]}"#.into(),
                    output_schema: None,
                    mutation: Mutation::Mutating,
                },
            ],
        }
    }
}

impl Backend for Loopback {
    fn servers(&mut self) -> Result<Vec<Server>, Failure> {
        Ok(vec![self.server.clone()])
    }

    fn tools(&mut self, server: &str) -> Result<Vec<Tool>, Failure> {
        if server != self.server.id {
            return Err(Failure::NotFound(server.into()));
        }
        Ok(self.tools.clone())
    }

    fn call(&mut self, server: &str, tool: &str, arguments: &str) -> Result<String, Failure> {
        if server != self.server.id {
            return Err(Failure::NotFound(server.into()));
        }
        if !self.tools.iter().any(|candidate| candidate.name == tool) {
            return Err(Failure::NotFound(format!("{server}/{tool}")));
        }
        let value: serde_json::Value =
            serde_json::from_str(arguments).map_err(|error| Failure::Invalid(error.to_string()))?;
        Ok(serde_json::json!({ "tool": tool, "arguments": value }).to_string())
    }
}

/// Generate a Soil surface from an MCP catalog.
///
/// This is a projection, not a new ABI. The generic `soil:mcp/call` remains the
/// only host call; these wrappers give the agent readable names and docs.
pub fn render_soil(backend: &mut dyn Backend) -> Result<String, Failure> {
    let servers = backend.servers()?;
    let mut out = String::from("; SPDX-License-Identifier: MIT OR Apache-2.0\n; generated from MCP catalogs\n\n");
    for server in servers {
        let tools = backend.tools(&server.id)?;
        for tool in tools {
            let name = soil_name(&server.id, &tool.name);
            out.push_str(&format!(";; {}\n", one_line(&tool.description)));
            out.push_str(&format!("(defun {name} (arguments)\n"));
            out.push_str(&format!(
                "  (soil:mcp/call \"{}\" \"{}\" (json-encode arguments)))\n\n",
                escape(&server.id),
                escape(&tool.name)
            ));
        }
    }
    Ok(out)
}

fn soil_name(server: &str, tool: &str) -> String {
    format!("mcp/{}/{}", segment(server), segment(tool))
}

fn segment(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_catalog_and_call() {
        let mut backend = Loopback::default();
        assert_eq!(backend.servers().unwrap().len(), 1);
        let tools = backend.tools("loopback").unwrap();
        assert_eq!(tools[0].name, "echo");
        let result = backend
            .call("loopback", "echo", r#"{"x":1}"#)
            .unwrap();
        assert!(result.contains("\"x\":1"));
    }

    #[test]
    fn renders_readable_soil_wrappers() {
        let mut backend = Loopback::default();
        let source = render_soil(&mut backend).unwrap();
        assert!(source.contains("(defun mcp/loopback/echo (arguments)"));
        assert!(source.contains("(defun mcp/loopback/write-note (arguments)"));
        assert!(source.contains("soil:mcp/call"));
    }

    #[test]
    fn rejects_unknown_tools() {
        let mut backend = Loopback::default();
        assert!(matches!(
            backend.call("loopback", "missing", "{}"),
            Err(Failure::NotFound(_))
        ));
    }

    #[test]
    fn schema_is_data_not_parsed_policy() {
        let mut backend = Loopback::default();
        let tools = backend.tools("loopback").unwrap();
        let map: BTreeMap<_, _> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.mutation))
            .collect();
        assert_eq!(map["echo"], Mutation::ReadOnly);
        assert_eq!(map["write-note"], Mutation::Mutating);
    }
}
