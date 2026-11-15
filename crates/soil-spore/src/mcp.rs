//! The MCP projection of a package's commands.
//!
//! A tool list is a second description of the same interface, and a second
//! description drifts. So there is no MCP table to fill in: the tools are
//! rendered from `[[subcommands]]`, which is also what the man page's SYNOPSIS and
//! any future `--help` come from. Editing one subcommand moves every projection
//! at once, which is the only arrangement where a reviewer can trust them.
//!
//! What the projection preserves, deliberately:
//!
//! - **The manifest is the capability statement.** A client that lists tools
//!   from a digest-verified manifest is reading the same facts the host uses to
//!   refuse a capability, not an assertion the server makes about itself.
//! - **The man page is a resource, not a description.** Descriptions are
//!   re-sent on every call and truncated by clients; depth belongs behind a
//!   fetch the caller makes when it wants it.
//! - **Arguments keep their spelling.** `--registry URL` becomes a string
//!   property named `--registry`, so a tool result can be pasted into a shell
//!   without a translation table.

use serde_json::{Value, json};

use crate::{Error, Manifest, Result};

/// One tool, in the shape MCP's `tools/list` expects.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, with every property spelled the way the
    /// CLI spells it.
    pub input_schema: Value,
}

impl Tool {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

/// The `tools/list` result for one package.
///
/// An empty list is an error rather than a valid answer: a package that opts
/// into `[mcp]` and exposes nothing is a misconfiguration a client should hear
/// about, not a server that silently does nothing.
pub fn tools(manifest: &Manifest) -> Result<Vec<Tool>> {
    let server = manifest
        .mcp
        .as_ref()
        .and_then(|mcp| mcp.name.clone())
        .unwrap_or_else(|| manifest.spore.name.clone());
    if manifest.subcommands.is_empty() {
        return Err(Error::Invalid(
            "[mcp] projects [[subcommands]], and this package declares none".into(),
        ));
    }
    Ok(manifest
        .subcommands
        .iter()
        .map(|command| Tool {
            // A client shows the server name beside the tool, so the tool is
            // named after the subcommand; qualifying it twice would read as
            // `bob bob_builders`.
            name: format!("{}_{}", server, command.name).replace('-', "_"),
            description: command.summary.clone(),
            input_schema: schema(manifest, command),
        })
        .collect())
}

pub fn schema(manifest: &Manifest, command: &crate::Subcommand) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for arg in &command.args {
        let (kind, description) = match &arg.value {
            Some(value) => ("string", format!("{} ({value})", arg.summary)),
            None if arg.is_option() => ("boolean", arg.summary.clone()),
            None => ("string", arg.summary.clone()),
        };
        let mut property = json!({"type": kind});
        if !description.trim().is_empty() {
            property["description"] = json!(description.trim());
        }
        if let Some(default) = &arg.default {
            property["default"] = json!(default);
        }
        properties.insert(arg.name.clone(), property);
        if arg.required {
            required.push(json!(arg.name));
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
        "x-spore": manifest.spore.id,
    })
}

/// Where the man page lives as an MCP resource.
pub fn man_resource(manifest: &Manifest) -> Value {
    json!({
        "uri": format!("spore://{}/man", manifest.spore.id),
        "name": format!("{}(1)", manifest.spore.name),
        "description": manifest.spore.summary,
        "mimeType": "text/markdown",
    })
}

/// The whole `tools/list` result, ready to be the `result` of a JSON-RPC reply.
pub fn tools_list(manifest: &Manifest) -> Result<Value> {
    let tools: Vec<Value> = tools(manifest)?.iter().map(Tool::to_json).collect();
    Ok(json!({ "tools": tools }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        let mut manifest = Manifest::parse(
            r#"
            schema = 1
            [spore]
            id = "dev.fpl.bob"
            name = "bob"
            version = "0.1.0"
            license = "MIT"
            summary = "Ask the build plane who can build"
            [engine]
            soil = ">=0.1"
            target = "wasm32-wasip2"
            abi = { http = "0.1.0" }
            [module]
            path = "module.wasm"
            [launch]
            default = "native"
            surfaces = ["native"]
            [mcp]
            enabled = true
            [[subcommands]]
            name = "builders"
            summary = "Live builders and their capacity"
            [[subcommands.args]]
            name = "--registry"
            value = "URL"
            summary = "Registry to ask"
            [[subcommands.args]]
            name = "--json"
            summary = "Print the list as JSON"
            "#,
        )
        .expect("parse");
        manifest.validate().expect("validate");
        manifest
    }

    #[test]
    fn a_command_becomes_one_tool_with_a_real_schema() {
        let tools = tools(&manifest()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "bob_builders");
        assert_eq!(tools[0].description, "Live builders and their capacity");
        let schema = &tools[0].input_schema;
        assert_eq!(schema["properties"]["--registry"]["type"], "string");
        assert_eq!(schema["properties"]["--json"]["type"], "boolean");
        assert_eq!(schema["required"], json!([]));
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn the_synopsis_and_the_schema_come_from_the_same_declaration() {
        let manifest = manifest();
        let line = manifest.subcommands[0].synopsis(&manifest.spore.name);
        assert_eq!(line, "bob builders [--registry URL] [--json]");
        let tools = tools(&manifest).unwrap();
        assert_eq!(
            tools[0].input_schema["properties"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_required_argument_is_required_in_both() {
        let mut manifest = manifest();
        manifest.subcommands[0].args[0].required = true;
        assert_eq!(
            manifest.subcommands[0].synopsis("bob"),
            "bob builders --registry URL [--json]"
        );
        let tools = tools(&manifest).unwrap();
        assert_eq!(tools[0].input_schema["required"], json!(["--registry"]));
    }

    #[test]
    fn opting_into_mcp_without_commands_is_refused() {
        let mut manifest = manifest();
        manifest.subcommands.clear();
        assert!(manifest.validate().is_err());
        assert!(tools(&manifest).is_err());
    }

    #[test]
    fn the_man_page_is_a_resource() {
        let resource = man_resource(&manifest());
        assert_eq!(resource["uri"], "spore://dev.fpl.bob/man");
    }
}
