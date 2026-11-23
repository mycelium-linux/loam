# Plugin IPC

`loam:plugin/ipc@0.1.0` is the narrow request surface between a wasm adapter and
the native implementation behind it. It is IPC, not a capability: the adapter
can only send a method name and a JSON payload to the one backend Loam chose.

```wit
package loam:plugin@0.1.0;

interface ipc {
  enum error { denied, unavailable, invalid, io }

  request: func(method: string, payload: list<u8>) -> result<list<u8>, error>;
}

world guest { import ipc; }
```

`payload` is JSON bytes. The host and sidecar exchange it as one
newline-delimited JSON object per line; the adapter never sees a pid, socket,
file descriptor, or the sidecar's environment.

## Canonical wire protocol

Request:

```json
{"method":"poll","payload":{"cursor":"c-1"}}
```

Success:

```json
{"ok":true,"payload":{"events":[]}}
```

Error:

```json
{"ok":false,"error":"denied"}
```

`payload` is canonical. For compatibility with the existing Drawbridge host,
Loam also emits the identical object under `params`, and accepts either
`payload` or `result` in a response:

```json
{"method":"poll","payload":{},"params":{}}
{"ok":true,"result":{}}
```

New sidecars should read `payload` and return `payload`. A sidecar that only
understands `params`/`result` remains compatible for this protocol version.

## Rules

- One request object per line; one response object per line.
- An empty request payload is JSON `null`.
- A malformed request returns an error response and does not kill the sidecar.
- A malformed response maps to `invalid` and the host drains that line.
- Sidecar EOF or process exit maps to `unavailable`.
- An unknown method maps to `invalid`.
- Empty or malformed method names are `invalid` before anything is sent.
- Loam enforces a bounded line size (4 MiB by default). An oversized line is
  drained to the next newline and maps to `invalid`; the sidecar stays alive.
- A backend that is absent, missing, or already exited maps to `unavailable`.

## Error mapping

| sidecar `error` | WIT error |
|---|---|
| `denied` | `denied` |
| `invalid` | `invalid` |
| `io` | `io` |
| anything else, EOF, exit | `unavailable` |

JSON-RPC is not used here. This is a tiny sidecar protocol, not a network
protocol and not a second plugin manifest.

## Host behavior

`loam` spawns one sidecar for a plugin-backed command run, sets the working
directory to the plugin package, and connects stdin/stdout with pipes. Requests
are serialized: one request, then one response. The child is killed and reaped
when the run ends.

The sidecar inherits the Loam process environment. The adapter cannot read that
environment through this interface. Do not put credentials in command-line
arguments, and do not make the adapter the place where a secret is resolved.

## Compatibility

```text
protocol  loam:plugin/ipc@0.1.0
payload   canonical
params    request alias
result    response alias
framing   newline-delimited JSON
limit     4 MiB per line
```
