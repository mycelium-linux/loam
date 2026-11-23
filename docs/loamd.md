# loamd

`loamd` is the per-user loam supervisor. It owns one process, one command
registry, one plugin registry, and the command instances it starts.

This is the first daemon slice. It is deliberately **per-user only**:

- socket: `$XDG_RUNTIME_DIR/loam/loamd.sock`, or
  `~/.local/share/loam/loamd.sock`
- socket mode: `0600`
- peer authentication: Unix peer uid must equal the daemon's effective uid
- no root mode, remote listener, group ACL, or service-account model yet

## Run

```sh
loamd
loamd --socket /tmp/loamd.sock --root ~/.local/share/fungos/commands
```

`loamd` scans command and Spore roots at startup. Records and Unibus can be
configured the same way as `loam run`:

```sh
loamd \
  --records-root ~/.local/share/fungos/records \
  --book personal/avery/marbles \
  --collection ''
```

## Control

The same binary is the local control client:

```sh
loamd ping
loamd list
loamd start mb --id mb-1 -- ready --project elmers --json
loamd inspect mb-1
loamd logs mb-1
loamd stop mb-1
loamd shutdown
```

`start` takes a command name followed by its arguments. `--id` defaults to the
command name, so starting the same command twice requires distinct ids.

Useful flags:

```text
--socket PATH
--id ID
--env NAME=VALUE
--cwd DIR
--stdin TEXT
--timeout-ms N
--max-stdout-bytes N
--max-stderr-bytes N
--max-memory-bytes N
```

## Lifecycle

Each instance gets its own Wasmtime `Engine`, `Store`, host state, and limits.
That costs a component compile per instance, but gives `stop` a per-instance
epoch interruption: stopping one instance cannot interrupt another.

Instances are one-shot commands for now. Long-lived `wasi:http` service Spores,
restart policy, and service routing are not part of this slice.

Output is captured in memory, bounded by the per-instance stdout/stderr limits.
`logs` returns the captured text after the instance finishes.

## Protocol

The client and daemon exchange one newline-delimited JSON request and response
over the Unix socket. This is a local admin protocol, not a WIT capability and
not a second package format.

```json
{"op":"ping"}
{"op":"start","id":"mb-1","command":"mb","args":["ready","--json"]}
{"op":"list"}
{"op":"logs","id":"mb-1"}
{"op":"stop","id":"mb-1"}
{"op":"shutdown"}
```

Responses are `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`.

## Security boundary

Same-uid peer credentials are authentication, not authorization. The daemon
does not yet have per-Spore owner/group ACLs or Mycelium grant intersection.
Those belong in the next slice before this becomes a system-wide daemon.
