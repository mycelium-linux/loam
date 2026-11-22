#!/usr/bin/env bash
# Build one SSR home snapshot. Cargo is frozen; use an existing soil-lang.
set -euo pipefail

here=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)

SOIL_LANG=${SOIL_LANG:-}
if [[ -z "$SOIL_LANG" ]]; then
  if [[ -x "$root/.bin/soil-lang" ]]; then
    SOIL_LANG="$root/.bin/soil-lang"
  else
    SOIL_LANG="$root/target/debug/soil-lang"
  fi
fi
if [[ ! -x "$SOIL_LANG" ]]; then
  echo "home: no soil-lang binary at $SOIL_LANG (build soil-lang or set SOIL_LANG)" >&2
  exit 2
fi

hue=${HUE_BRIDGE:-}
if [[ -z "$hue" ]]; then
  echo "home: set HUE_BRIDGE, for example HUE_BRIDGE=http://192.168.10.15" >&2
  exit 2
fi
case "$hue" in
  http://* | https://*) ;;
  *) hue="http://$hue" ;;
esac
hue=${hue%/}

room=${HUE_ROOM:-Living room}
local_port=${DEADWAX_LOCAL_PORT:-$((20000 + $$ % 20000))}
deadwax=${DEADWAX_URL:-http://127.0.0.1:6601}
case "$deadwax" in
  http://* | https://*) ;;
  *) deadwax="http://$deadwax" ;;
esac
deadwax=${deadwax%/}

cache=${XDG_CACHE_HOME:-"$HOME/.cache"}/soil/home
workspace=${HOME_WORKSPACE:-"$cache/workspace"}
records=${HOME_RECORDS:-"$cache/records"}
mkdir -p "$workspace" "$records" "$here/dist"

if [[ -n "${DEADWAX_SSH:-}" ]]; then
  if ! curl -fsS --max-time 1 "http://127.0.0.1:$local_port/health" >/dev/null 2>&1; then
    ssh -f -N -o BatchMode=yes -o ConnectTimeout=5 -o ExitOnForwardFailure=yes \
      -o ServerAliveInterval=5 \
      -L "$local_port":127.0.0.1:6601 "$DEADWAX_SSH"
    ready=0
    for _ in $(seq 1 100); do
      if curl -fsS --max-time 1 "http://127.0.0.1:$local_port/health" >/dev/null 2>&1; then
        ready=1
        break
      fi
      sleep 0.1
    done
    if [[ $ready -ne 1 ]]; then
      echo "home: could not reach deadwax through $DEADWAX_SSH" >&2
      exit 2
    fi
  fi
  deadwax="http://127.0.0.1:$local_port"
fi

python3 - "$here/.config.soil" "$hue" "$room" "$deadwax" <<'PY'
import json
import sys

path, hue, room, deadwax = sys.argv[1:]
config = {
    "hue-base": hue,
    "hue-credential": "default",
    "room": room,
    "deadwax-base": deadwax,
}
with open(path, "w") as handle:
    handle.write("(defun home-config () (json-decode ")
    handle.write(json.dumps(json.dumps(config)))
    handle.write("))\n")
PY

credential_args=()
if [[ -n "${HUE_KEY:-}" ]]; then
  credential_args=(--http-credential-env HUE_KEY)
else
  geode_socket=${HUE_GEODE_SOCKET:-"$HOME/.geode/geode.sock"}
  if [[ ! -S "$geode_socket" ]]; then
    echo "home: no Geode socket at $geode_socket (or set HUE_KEY for a dev key)" >&2
    exit 2
  fi
  credential_args=(
    --geode-socket "$geode_socket"
    --geode-reference "${HUE_GEODE_REFERENCE:-geode://home/hue/bridge}"
    --geode-purpose "${HUE_GEODE_PURPOSE:-hue.control}"
  )
fi

"$SOIL_LANG" \
  --workspace-root "$workspace" \
  --records-root "$records" \
  --prelude "$root/crates/soil-lang/stdlib/prelude.soil" \
  --allow "$hue" \
  --allow "$deadwax" \
  "${credential_args[@]}" \
  "$root/crates/soil-lang/stdlib/ui.soil" \
  "$root/crates/soil-lang/bindings/soil-http.soil" \
  "$root/examples/hue/hue.soil" \
  "$root/examples/playback/playback.soil" \
  "$root/examples/home/home.soil" \
  "$root/examples/home/ui.soil" \
  "$root/examples/home/.config.soil" \
  "$root/examples/home/dashboard.soil" > "$here/dist/index.html"

echo "wrote $here/dist/index.html"
