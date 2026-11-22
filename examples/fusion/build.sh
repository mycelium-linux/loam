#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/../.."
BIN=${SOIL_LANG:-${SOIL_BIN:-}}
if [ -z "$BIN" ]; then
  if [ -x .bin/soil-lang ]; then BIN=.bin/soil-lang; else BIN=target/debug/soil-lang; fi
fi
[ -x "$BIN" ] || { echo "no soil-lang binary at $BIN" >&2; exit 1; }
python3 - <<'PY'
import json, pathlib
fixture = json.loads(pathlib.Path("examples/fusion/fixture.json").read_text())
out = pathlib.Path("examples/fusion/fixture.soil")
out.write_text("(defun fusion-fixture () (json-decode " + json.dumps(json.dumps(fixture)) + "))\n")
PY
"$BIN" \
  crates/soil-lang/stdlib/prelude.soil \
  examples/fusion/fixture.soil \
  examples/fusion/fusion.soil \
  examples/fusion/run.soil
