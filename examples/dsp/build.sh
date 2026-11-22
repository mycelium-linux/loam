#!/usr/bin/env sh
# Generate the Soil fixture from the recorded JSON and run the offline DSP graph.
set -eu
cd "$(dirname "$0")/../.."
BIN=${SOIL_LANG:-${SOIL_BIN:-}}
if [ -z "$BIN" ]; then
  if [ -x .bin/soil-lang ]; then BIN=.bin/soil-lang; else BIN=target/debug/soil-lang; fi
fi
[ -x "$BIN" ] || { echo "no soil-lang binary at $BIN" >&2; exit 1; }
mkdir -p examples/dsp/.vfs examples/dsp/.records
python3 - <<'PY'
import json, pathlib
fixture = json.loads(pathlib.Path("examples/dsp/fixture.json").read_text())
out = pathlib.Path("examples/dsp/fixture.soil")
out.write_text("(defun dsp-fixture () (json-decode " + json.dumps(json.dumps(fixture)) + "))\n")
PY
"$BIN" \
  crates/soil-lang/stdlib/prelude.soil \
  crates/soil-lang/stdlib/pipeline.soil \
  crates/soil-lang/bindings/soil-dsp.soil \
  examples/dsp/fixture.soil \
  examples/dsp/run.soil \
  --workspace-root examples/dsp/.vfs \
  --records-root examples/dsp/.records --book soil
