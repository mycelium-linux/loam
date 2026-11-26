# Composio MCP plugin

Exports `soil:mcp/mcp@0.1.0` backed by Composio's Streamable HTTP MCP server.

```text
consumer -> soil:mcp -> adapter -> loam:plugin/ipc -> composio-mcp
                                                        |
                                                        +-> geode lease
                                                        +-> loam-mcp --http
```

The sidecar wrapper asks Geode for the `composio.mcp` purpose and passes the
token only in `COMPOSIO_MCP_TOKEN` to `loam-mcp`. Neither the token nor the
endpoint is visible to the consuming Spore.

## Build

```sh
./build.sh
loam plugin pack .
loam plugin verify .
loam install . --allow-unsigned-dev
loam plugin list --json
```

## Geode

```toml
[profiles.composio]
reference = "geode://fungos/composio/mcp-token"
purpose = "composio.mcp"
env = "COMPOSIO_MCP_TOKEN"
```

The secret is stored with:

```sh
printf '%s' "$TOKEN" | geoded --put geode://fungos/composio/mcp-token --purpose composio.mcp
```

Restart `geoded` after a direct store write so the running lease server reloads
the store.

## Test

```sh
printf '%s\n' \
  '{"method":"servers","payload":null}' \
  '{"method":"tools","payload":"composio"}' \
| geode exec --profile composio -- ./bin/loam-mcp \
    --http composio=https://connect.composio.dev/mcp \
    --auth-env composio=COMPOSIO_MCP_TOKEN
```
