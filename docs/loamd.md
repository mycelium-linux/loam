# loamd

See [`plugins.md`](plugins.md) for the plugin package, IPC, and sidecar model.

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

`loamd` scans command and Spore roots at startup. `loamd reload` rescans them
without restarting the daemon, and `start` retries the scan once when a command
appeared after the daemon started. `loam install` asks a running daemon to
reload, so a finished install is visible without a second command.

Manifest web surfaces are re-scanned every five seconds as well. When a new
Spore adds a `.myc` name, `loamd` starts the listener if needed, publishes the
new lease, and asks Mycelium to renew the frontdoor leaf certificate:

```sh
mycelium frontdoor renew --write
```

Renewal reuses the local CA, so adding a Spore never turns into a new trust
decision. An install made before `ca-key.pem` was persisted has no way to sign
a new leaf; on that host `loamd` logs the one-time migration command:

```sh
mycelium frontdoor install --write --trust-cert
```

## Configuration

Host defaults can live in `loam.conf.soil`:

```soil
(loam
  (version 1)
  (daemon
    (socket "~/.local/share/loam/loamd.sock")
    (max-instances 8)
    (roots ("~/.local/share/fungos/commands"
            "~/.local/share/fungos/spores"))
    (image-runtime "/usr/local/bin/soil-image")
    (window-runtime "/usr/local/bin/loam-window")
    (log (dir "~/.local/state/loam/logs") (max-bytes 1048576) (keep 4))
    (web (listen "127.0.0.1:8799")))
  (mycelium
    (home "~/.mycelium")
    (publish-leases #t)))
```

`loamd` looks for `$XDG_CONFIG_HOME/loam/loam.conf.soil`, then
`~/.config/loam/loam.conf.soil`. `--config FILE` overrides the search, and
`LOAM_CONFIG` names a file explicitly. Explicit files must exist; a missing
default file is not an error.

Precedence is:

```text
command line > loam.conf.soil > built-in defaults
```

A first `--root` replaces the configured roots; later `--root` flags append.
`--no-publish-leases` disables the Mycelium lease publisher independently of
whether the web surface is served. Live leases, log files, the socket, and
installed package state are runtime state, not configuration.

Records and Unibus can be configured the same way as `loam run`:

```sh
loamd \
  --records-root ~/.local/share/fungos/records \
  --book personal/avery/marbles \
  --collection ''
```

## Spore catalog bridge

A web surface cannot import WIT, so `loamd` exposes the installed catalog over
the same listener. The responses come from the host-side catalog that backs
`soil:spore/catalog`:

```text
GET  /api/spore/entries
GET  /api/spore/running
POST /api/spore/launch   {"id":"org.mycelium.notes","surface":"web","native":true}
POST /api/spore/stop     {"id":"org.mycelium.notes"}
```

`entries` includes id, title, version, summary, surfaces, role
(`app`/`plugin`/`package`), provided WIT interfaces and installed commands.
A web surface launches with a URL target. `"native": true` asks the host to open
it with `loam-window`, which is what the native launcher uses; a canvas surface
is started through `loam surface`. Both appear in `running` until they exit or
`stop` ends them.

The bridge is not ambient: a web surface reaches it only when its manifest
grants `soil:spore` or `soil:spore.catalog`. The launcher Spore
(`examples/spores/home`) declares that grant and reads the catalog instead of
hard-coding a dock, so an install appears on the next page load.

## Control

The same binary is the local control client:

```sh
loamd ping
loamd list
loamd start mb --id mb-1 -- ready --project elmers --json
loamd inspect mb-1
loamd logs mb-1
loamd stop mb-1
loamd reload
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
When the instance finishes, the same output is appended to
`<log-dir>/<id>.log` with its exit code or error. The file rotates before it
crosses `--log-max-bytes`, keeping `--log-keep` generations. The default log
directory is `$XDG_STATE_HOME/loam/logs` or `~/.local/state/loam/logs`.

`loamd logs <id>` returns the in-memory record while the daemon is running and
falls back to the durable file after a restart.

### Plugin sidecars

A plugin-backed command owns one sidecar for that run. `loamd` starts the
command; the command runner starts the sidecar from the plugin's `[plugin]
sidecar`, with the plugin package as its working directory. The sidecar uses
stdin/stdout pipes, not a socket. The adapter can reach it only through
`loam:plugin/ipc`; it never sees the pid, socket, or environment.

`loamd inspect <id>` reports the command instance, not the sidecar as a separate
child. `loamd logs <id>` contains the command's captured stdout/stderr; the
sidecar inherits the daemon's stderr for its own diagnostics. A sidecar that
exits or closes its stdout maps to `unavailable` in the adapter. There is no
restart policy in this slice: the next `start` gets a fresh sidecar.

If the sidecar is missing or non-executable, `loam plugin verify` refuses the
package; install should not produce a command that reaches that state.

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
{"op":"reload"}
{"op":"shutdown"}
```

Responses are `{"ok":true,"data":...}` or `{"ok":false,"error":"..."}`.

## Security boundary

Same-uid peer credentials are authentication, not authorization. The daemon
does not yet have per-Spore owner/group ACLs or Mycelium grant intersection.
Those belong in the next slice before this becomes a system-wide daemon.
