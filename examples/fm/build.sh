#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/../.."
BIN=${SOIL_LANG:-${SOIL_BIN:-}}
if [ -z "$BIN" ]; then
  if [ -x .bin/soil-lang ]; then BIN=.bin/soil-lang; else BIN=target/debug/soil-lang; fi
fi
[ -x "$BIN" ] || { echo "no soil-lang binary at $BIN" >&2; exit 1; }
mkdir -p examples/fm/.vfs examples/fm/.records
python3 - <<'PY'
import json, math, pathlib, random
rate = 2_400_000
n = 2048
center = 100_000_000
stations = [(-1_100_000, 0.7), (-350_000, 1.0), (800_000, 0.55)]
rng = random.Random(7)
samples = []
for i in range(n):
    i_acc = 0.0
    q_acc = 0.0
    for offset, amplitude in stations:
        angle = 2.0 * math.pi * offset * i / rate
        i_acc += amplitude * math.cos(angle)
        q_acc += amplitude * math.sin(angle)
    i_acc += rng.uniform(-0.02, 0.02)
    q_acc += rng.uniform(-0.02, 0.02)
    samples.extend([round(i_acc, 6), round(q_acc, 6)])
fixture = {
    "name": "fm-broadcast-synthetic",
    "center_hz": center,
    "sample_rate_hz": rate,
    "samples": samples,
}
out = pathlib.Path("examples/fm/fixture.soil")
out.write_text("(defun fm-fixture () (json-decode " + json.dumps(json.dumps(fixture)) + "))\n")
PY
"$BIN" \
  crates/soil-lang/stdlib/prelude.soil \
  crates/soil-lang/stdlib/pipeline.soil \
  crates/soil-lang/bindings/soil-dsp.soil \
  examples/fm/fixture.soil \
  examples/fm/run.soil \
  --workspace-root examples/fm/.vfs \
  --records-root examples/fm/.records --book soil
