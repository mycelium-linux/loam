# soil:mcp

MCP tools are data. The ABI is one stable capability, `soil:mcp@0.1.0`:

- `servers() -> list<server>`
- `tools(server) -> list<tool>`
- `call(server, tool, arguments_json) -> result_json`

Tool schemas stay JSON strings. Transport, credentials, leases, cancellation,
and authorization live below this seam (Unibus MCP or another adapter). There
is no runtime WIT generation and no per-tool ABI churn.

## Projection

`soil-mcp-api::render_soil` reads a catalog through the transport-agnostic
`Backend` trait and emits ordinary Soil wrappers:

```soil
(defun mcp/github/create-issue (arguments)
  (soil:mcp/call "github" "create_issue" (json-encode arguments)))
```

The wrappers are generated ergonomics, not a new ABI. Regenerate them when the
catalog changes; the host interface remains `soil:mcp`.

## Backends

`Loopback` is the reference test backend. A production adapter should reuse
Unibus's `mcp.provider.v1`, `mcp.invoke.v1`, and `mcp.result.v1` envelopes rather
than reimplementing Streamable HTTP sessions in Soil.

## Invariants

- Arguments and results are JSON, never untyped binary.
- `mutation` is carried from the catalog so read-only tools can be approved
  differently from mutating tools.
- `content-id`/VFS access stays in `soil:vfs`; MCP does not get a second file API.
- Cancellation and progress are host concerns, not per-tool WIT functions.
