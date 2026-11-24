#!/usr/bin/env bash
# End-to-end: a real Spore accepts a session over WebSocket through loamd.
#
# Builds the stream-echo fixture, installs it as a Spore that declares a
# `soil:streams` grant and one command, starts loamd with a ws listener, dials
# it, and expects the Spore's echo. This is the test the transport PRs were
# missing: it proves the hub wiring, not just the handshake.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null || true' EXIT

FIXTURE="$ROOT/crates/loam/fixtures/stream-echo"
FPL_BOB_LOCAL=1 MACOSX_DEPLOYMENT_TARGET=15.0 CARGO_TARGET_DIR="$WORK/fixture-target" \
  cargo build --manifest-path "$FIXTURE/Cargo.toml" --target wasm32-wasip2 --release >/dev/null

FPL_BOB_LOCAL=1 MACOSX_DEPLOYMENT_TARGET=15.0 CMAKE_POLICY_VERSION_MINIMUM=3.5 \
  cargo build --manifest-path "$ROOT/Cargo.toml" -p loam --bin loamd \
  --features stream-ws,wasi >/dev/null

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')

COMMANDS="$WORK/commands/stream-echo"
mkdir -p "$COMMANDS"
cp "$WORK/fixture-target/wasm32-wasip2/release/loam_stream_echo.wasm" "$COMMANDS/module.wasm"
# The package's manifest is Soil, like everything else an author writes. The
# grant lives in the Spore's `grants`, not on the command.
cp "$FIXTURE/spore.soil" "$COMMANDS/spore.soil"

cat > "$WORK/loam.conf.soil" <<TOML
(loam
  (version 1)
  (daemon (stream (ws (listen "127.0.0.1:$PORT") (protocol "capture")))))
TOML

"$TARGET/debug/loamd" serve --config "$WORK/loam.conf.soil" \
  --socket "$WORK/loamd.sock" --root "$WORK/commands" --log-dir "$WORK/logs" \
  >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 100); do
  [ -S "$WORK/loamd.sock" ] && break
  sleep 0.1
done
[ -S "$WORK/loamd.sock" ] || { echo "daemon did not start"; cat "$WORK/daemon.log"; exit 1; }

"$TARGET/debug/loamd" start stream-echo --id echo --socket "$WORK/loamd.sock" >/dev/null

REPLY=$(python3 - "$PORT" <<'PY'
import base64, os, socket, sys, time
port = int(sys.argv[1])
deadline = time.time() + 20
while time.time() < deadline:
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=2)
        break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit("ws listener never came up")
key = base64.b64encode(os.urandom(16)).decode()
req = (f"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n"
       f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
       f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: capture\r\n\r\n")
s.sendall(req.encode())
handshake = s.recv(1024).decode(errors="replace")
assert "101" in handshake.split("\r\n")[0], handshake
# one masked text frame: "hello"
payload = b"hello"
frame = bytes([0x81, 0x80 | len(payload)]) + b"\x01\x02\x03\x04" + bytes(
    byte ^ b"\x01\x02\x03\x04"[i % 4] for i, byte in enumerate(payload)
)
s.sendall(frame)
data = s.recv(1024)
s.close()
# server frame: 0x81, length, payload
assert data[0] in (0x81, 0x82), data   # host sends binary frames
length = data[1] & 0x7F
print(data[2:2 + length].decode())
PY
)

echo "reply: $REPLY"
[ "$REPLY" = "echo:hello" ] || { echo "expected echo:hello"; cat "$WORK/daemon.log"; exit 1; }
echo "stream e2e ok"
