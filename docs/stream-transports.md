# `soil:streams` transports

Status: **WebSocket and Iroh transports landed; daemon wiring next**, 2026-11-23.

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

## Still to wire

The transports are listeners, not services. To make them reachable:

1. **Config** — a `stream` section in `loam.conf.soil`:
   ```soil
   (daemon
     (stream
       (ws (listen "127.0.0.1:8791") (protocol "capture"))
       (iroh (alpn "soil/streams/1") (key "~/.mycelium/stream.key"))))
   ```
2. **Hub injection** — `WasiHostBuilder::stream_hub(hub)` exists but has no
   caller. The instance build site must pass one shared `StreamHub` so every
   Spore `accept`s from the same queue.
3. **Daemon startup** — spawn one tokio runtime thread per configured transport,
   sharing that hub. `loamd` is std-thread based today, so this is a small
   `tokio::runtime::Runtime::new()?.block_on(...)` in its own thread.

Until 2 lands, both transports are correct but inert — the same state the
interface was in before, with a producer instead of none.

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
