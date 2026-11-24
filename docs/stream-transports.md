# `soil:streams` transports

Status: **landed and wired**, 2026-11-23.

The interface and host implementation were already here — `wit/stream/stream.wit`,
`src/stream.rs`, linked from `service.rs` and `command_service.rs`. What was
missing is anything that calls `StreamHub::attach`: the hub had no producer, so
`accept(protocol)` always returned `none`.

Two producers now exist.

## WebSocket (`stream_ws.rs`, feature `stream-ws`)

One connection is one session.

- The **`Sec-WebSocket-Protocol`** header names the Spore protocol; the server
  echoes it, because a client that asks for a subprotocol and does not get it
  back is required to fail the handshake.
- Each binary message is one session frame (max 1 MiB, matching `stream.rs`).
- `ws://` by default; `wss://` when a cert and key are configured. TLS is the
  host's, and a half-configured pair refuses to start.
- Peer address is recorded in `info.peer`; it is an attestation.

## Iroh (`stream_iroh.rs`, feature `stream-iroh`)

One bidirectional QUIC stream is one session.

- The **first line** of the stream is the protocol (UTF-8, ≤64 bytes, newline
  terminated); the rest is payload. In-band rather than ALPN so one connection
  can carry several named sessions — one dial for a capture bridge.
- The endpoint secret key is created with `0600` when absent, so the endpoint id
  (and any lease naming it) survives restarts.
- `allow` is an endpoint-id list; empty means "anyone who knows this id", which
  is not the same as "anyone on the network".
- Relays are disabled; this is direct QUIC. A relay is a deployment choice, not
  a transport default.

## Tests

```sh
cargo test -p loam --features stream-ws,stream-iroh,wasi stream_
# stream_ws::…   a_websocket_session_is_queued_by_subprotocol_and_round_trips  ok
# stream_iroh::… an_iroh_stream_carries_its_protocol_line                      ok
```

Both assert the same three things: the session is queued under the protocol the
caller named, bytes round-trip in both directions through `StreamSession`, and
the peer is recorded.

## Wiring

Three pieces, all in this change:

1. **Config** — a `stream` section in `loam.conf.soil`:
   ```soil
   (daemon
     (stream
       (ws (listen "127.0.0.1:8791") (protocol "capture")
           (cert "/path/fullchain.pem") (key "/path/privkey.pem"))
       (iroh (alpn "soil/streams/1") (bind "0.0.0.0:0")
             (key "~/.mycelium/stream.key") (allow "<endpoint-id>"))))
   ```
   `cert` and `key` go together; a half pair is a config error. Every other
   option has a default (`ws` port 8791, `iroh` bind `0.0.0.0:0`, ALPN
   `soil/streams/1`, empty allow list).

2. **One hub, shared** — `loamd` creates a single `StreamHub` and passes it to
   every `CommandRunner` (`with_streams`). A Spore `accept`s by protocol, so it
   does not know or care which transport carried the session. The interface is
   linked only when the command's `requires` names `soil:streams*`: a command
   without it cannot accept even when the host runs listeners.

3. **One runtime thread** — `loamd` is std-thread based, so the listeners run on
   a dedicated `loamd-streams` thread with its own tokio runtime. Transports
   fail independently: a bad listen address for one logs and skips, leaving the
   other running.

A build without the transport features still starts; a configured listener it
cannot serve is logged, not fatal.

## Verified end to end

```sh
scripts/test-stream-e2e.sh
# reply: echo:hello
# stream e2e ok
```

That is the real test: a **Spore**, not a handshake. The script builds
`crates/loam/fixtures/stream-echo` (a command that exports `wasi:cli/run` and
imports `soil:streams/duplex`), installs it with `requires = ["soil:streams"]`,
starts `loamd` with a ws listener, dials it, and expects the Spore's echo back
through the session.

It also covers the two things that were previously unproven:

- `soil:streams` is in `RUNNER_CAPABILITIES`, so a command may declare it.
- the command `Host` implements `duplex::*` over the daemon's hub, so the
  session really reaches Spore code.

Worth knowing when writing such a fixture: a loam command must export
`wasi:cli/run`, and the hub's `accept` is a **poll** — a fixture without a clock
has to spin, so the test dials immediately after `loamd start`.

## Earlier handshake check

```sh
loamd serve --config …   # ws listen on a free port
# GET / with Upgrade + Sec-WebSocket-Protocol: capture
# → HTTP/1.1 101 Switching Protocols, sec-websocket-protocol: capture
lsof -nP -iUDP | grep loamd   # iroh bound its UDP sockets
```

## WIT follow-ups (before more transports)

- `abort()` — `close` is graceful only; cancelling currently means draining.
  Maps to `RESET_STREAM`.
- `busy`/`would-block` — QUIC has backpressure; `write` should surface it rather
  than short-write silently.
- `deadline` on `read` — the host imposes one; make it visible so a Spore can
  tell "slow" from "gone".
- Document the protocol-key mapping once: WSS subprotocol, Iroh first line,
  WebTransport `:protocol`.
- Per-Spore quota/policy for `accept`, or one Spore can drain the hub.

## What this is not

WebTransport. It is HTTP/3 with a browser-trusted certificate, which Iroh's
node-key TLS is not. If a browser needs streams, terminate WebTransport at an
edge and forward over Iroh:

```text
browser ──WebTransport(H3, real cert)──▶ edge ──iroh──▶ loam ──soil:streams──▶ spore
```
