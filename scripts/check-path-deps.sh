#!/usr/bin/env bash
# Fail if a Cargo path dependency escapes this checkout. Sibling worktrees are
# local build accidents, not reproducible dependency sources.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"

# Cargo gives up before it prints anything to parse when the missing manifest is
# the escaping one, so a failed resolve is the finding, not a broken script.
meta="$(cargo metadata --format-version 1 --no-deps)" || {
  echo "cargo cannot resolve this workspace; a path dependency probably escapes the checkout" >&2
  exit 1
}

printf '%s' "$meta" | python3 -c '
import json, pathlib, sys
root = pathlib.Path(sys.argv[1]).resolve()
meta = json.load(sys.stdin)
bad = []
for pkg in meta["packages"]:
    for dep in pkg.get("dependencies", []):
        path = dep.get("path")
        if not path:
            continue
        resolved = pathlib.Path(path).resolve()
        if resolved != root and root not in resolved.parents:
            bad.append((pkg["name"], dep["name"], str(resolved)))
if bad:
    for pkg, dep, path in bad:
        print(f"{pkg}: {dep} -> {path}", file=sys.stderr)
    sys.exit(1)
' "$root"
echo "path dependencies stay inside $root"
