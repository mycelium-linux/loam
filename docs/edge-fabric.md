# Edge fabric contract

Status: proposal for the Theseus edge relay. The transport is a single
WebSocket carrying JSON-RPC 2.0. The relay owns Mycelium/Iroh; the client never
does.

## Transport

```text
client ── WSS / JSON-RPC 2.0 ── edge relay ── soil:records | vfs | unibus | …
```

- WebSocket subprotocol: `loam.edge.v1`.
- One WebSocket is one session. Requests are multiplexed by JSON-RPC `id`.
- A tool call also carries a client-generated `call_id`, stable across retries,
  cancellation and partial results.
- The relay enforces a session lease and a per-call deadline. It does not
  forward arbitrary URLs, paths, keys or addresses.

## Session

`session/open` is the first request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/open",
  "params": {
    "protocol": "loam.edge.v1",
    "client": { "name": "theseus", "version": "0.1.0", "session": "ses-client" },
    "spore": { "id": "dev.fungos.tui", "digest": "sha256:…" },
    "capabilities": ["records.query"],
    "lease": {
      "token": "opaque",
      "audience": "loam-edge",
      "expires_at": "2026-11-23T09:00:00Z"
    }
  }
}
```

Result:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "session_id": "ses-relay",
    "principal": "buddy:ajmwagar",
    "expires_at": "2026-11-23T09:00:00Z",
    "capabilities": ["records.query"]
  }
}
```

The lease is short-lived and audience-bound. A Worker or edge client holds a
lease, not a Mycelium private key.

## Discovery

`tools/list` returns the tools this session may call:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/list",
  "params": {}
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "tools": [
      {
        "name": "records/query",
        "description": "Filtered, projected read of one records collection",
        "input_schema": { "type": "object" },
        "output_schema": { "type": "object" },
        "mutating": false,
        "streaming": false
      }
    ]
  }
}
```

Tool names are the JSON-RPC methods. `records/query` is called directly rather
than wrapped in a generic `tools/call`, so the wire shape stays small and MCP
translation is mechanical.

## Call shape

Every tool method takes the same envelope fields:

```json
{
  "call_id": "c-01J…",
  "deadline_ms": 15000,
  "lease_token": "optional-per-call-lease"
}
```

`deadline_ms` is relative to receipt by the relay. The relay returns the call
result in the JSON-RPC response:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "call_id": "c-01J…",
    "status": "ok",
    "result": {},
    "partial": false
  }
}
```

A tool failure is a successful JSON-RPC response with `status: "err"`:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "call_id": "c-01J…",
    "status": "err",
    "error": {
      "code": "denied",
      "message": "records.query is not granted",
      "retryable": false,
      "details": {}
    }
  }
}
```

JSON-RPC errors are reserved for malformed protocol, unknown methods, and
transport failures.

## Streaming and cancellation

A long call may send progress before its final response:

```json
{
  "jsonrpc": "2.0",
  "method": "tool/partial",
  "params": { "call_id": "c-01J…", "seq": 0, "chunk": {}, "done": false }
}
```

Cancellation is a notification:

```json
{
  "jsonrpc": "2.0",
  "method": "tool/cancel",
  "params": { "call_id": "c-01J…", "reason": "planner changed" }
}
```

The final response still carries the same `call_id` with
`status: "err"`, `error.code: "cancelled"` (or `deadline`) when the call did not
complete.

If a relay chooses to accept a call asynchronously, the request returns:

```json
{ "call_id": "c-01J…", "status": "accepted" }
```

and the final result arrives as:

```json
{
  "jsonrpc": "2.0",
  "method": "tool/result",
  "params": { "call_id": "c-01J…", "status": "ok", "result": {} }
}
```

## records/query

`records/query` maps directly to `soil:records/query-spec`:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "records/query",
  "params": {
    "call_id": "c-01J…",
    "deadline_ms": 15000,
    "book": "personal/avery/marbles",
    "spec": {
      "collection": "marbles.issue",
      "prefix": null,
      "filters": [
        { "field": "status", "op": "eq", "value": "open" },
        { "field": "project", "op": "eq", "value": "mycelium" }
      ],
      "fields": ["title", "status", "project", "updated"],
      "cursor": null,
      "limit": 100
    }
  }
}
```

Result:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "call_id": "c-01J…",
    "status": "ok",
    "result": {
      "entries": [
        {
          "key": { "collection": "marbles.issue", "id": "mycelium-123" },
          "schema": "marbles.issue@1",
          "revision": 42,
          "updated_at_ms": 1795423000000,
          "origin": "studio",
          "deleted": false,
          "body": { "encoding": "json", "value": { "title": "…" } }
        }
      ],
      "next": "mycelium-123"
    },
    "partial": false
  }
}
```

`body` carries either `{ "encoding": "json", "value": … }` when the relay can
decode the stored body as JSON, or `{ "encoding": "base64", "value": "…" }`
otherwise. `next` is the opaque cursor for the next page; `null` means end of
collection.

Error codes used by `records/query`:

```text
denied         capability or book not granted
invalid        malformed collection, filter, field or cursor
not_found      book or collection does not exist
over_budget    page/body exceeds the relay or call budget
stale          cursor predates compaction
unavailable    records backend is not available
deadline       deadline_ms elapsed
cancelled      tool/cancel received
internal       relay/store failure
```

## Compatibility with Theseus

Theseus maps its current `host-call(interface, function, args)` result
`(ok value)` / `(err code)` onto the call envelope:

```text
host-call succeeds  -> status: "ok"
host-call fails     -> status: "err", error.code from the host value
streaming           -> tool/partial notifications
cancel              -> tool/cancel notification
timeout             -> deadline_ms / error.code: "deadline"
```

Spores are tools, not hosts: a Spore exposed through `soil:mcp` appears as a
normal tool name (`spore/<id>/<export>` or the MCP tool name) with the same call
envelope. Theseus does not instantiate Spore components.

## Non-goals in this slice

- raw Iroh/QUIC from the Worker
- arbitrary URL/path/identity forwarding
- write tools (`records/upsert`, `vfs/write`) before the read path is proven
- HTTP-only fallback; WSS is the session transport
- a second frame format alongside JSON-RPC 2.0
