#!/usr/bin/env sh
# Generate the API docs from Soil `;;;` doc comments.
#
# The page catalog (order, titles, summaries) lives in docs-copy.soil. The
# entries on each page come from the `;;;` comments in the stdlib source.
# Extraction runs in Soil. The evaluator is recursive and this machine's stack
# is small, so the wrapper feeds it fixed-size source windows and joins the
# entries; projection to Markdown, llms.txt, and HTML stays in docs.soil.
set -eu
cd "$(dirname "$0")/../.."

BIN=${SOIL_LANG:-}
if [ -z "$BIN" ]; then
  if [ -x .bin/soil-lang ]; then
    BIN=.bin/soil-lang
  else
    BIN=target/debug/soil-lang
  fi
fi
if [ ! -x "$BIN" ]; then
  echo "no soil-lang binary at $BIN; cargo is frozen" >&2
  exit 1
fi

mkdir -p sites/soil-lang/dist/docs
BIN="$BIN" python3 - <<'PY'
import json
import os
import subprocess
import sys
import tempfile

root = os.getcwd()
bin_path = os.environ["BIN"]
prelude = [
    "crates/soil-lang/stdlib/prelude.soil",
    "crates/soil-lang/stdlib/ui.soil",
    "crates/soil-lang/stdlib/highlight.soil",
    "crates/soil-lang/stdlib/doc.soil",
    "sites/soil-lang/docs-copy.soil",
]

def chunks(text, limit=18):
    lines = text.splitlines()
    out = []
    start = 0
    while start < len(lines):
        end = min(start + limit, len(lines))
        # Keep a trailing doc-comment run with the form it documents.
        while end > start and (
            lines[end - 1].lstrip().startswith(";;;") or not lines[end - 1].strip()
        ):
            end -= 1
        if end <= start:
            end = min(start + limit, len(lines))
        out.append("\n".join(lines[start:end]))
        start = end
    return out

def run_soil(extra):
    command = [bin_path] + prelude + extra
    proc = subprocess.run(command, cwd=root, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(proc.returncode)
    value = proc.stdout.strip()
    if not value:
        return None
    return json.loads(json.loads(value))

def catalog():
    with tempfile.TemporaryDirectory() as tmp:
        app = os.path.join(tmp, "catalog.soil")
        with open(app, "w") as handle:
            handle.write("(defun main () (json-encode docs-copy))\n")
        return run_soil([app])

docs_copy = catalog()
sources = [
    (page["slug"], page["title"], page["summary"])
    for page in docs_copy["pages"]
]

def extract(chunk):
    with tempfile.TemporaryDirectory() as tmp:
        source = os.path.join(tmp, "source.soil")
        app = os.path.join(tmp, "app.soil")
        with open(source, "w") as handle:
            handle.write("(defun soil-doc-chunk () (json-decode ")
            handle.write(json.dumps(json.dumps(chunk)))
            handle.write("))\n")
        with open(app, "w") as handle:
            handle.write("(defun main () (json-encode (doc-extract-public (soil-doc-chunk))))\n")
        return run_soil([source, app]) or []

pages = []
for slug, title, summary in sources:
    with open(os.path.join(root, "crates/soil-lang/stdlib", slug + ".soil")) as handle:
        source = handle.read()
    entries = []
    for chunk in chunks(source):
        entries.extend(extract(chunk))
    pages.append({
        "slug": slug,
        "title": title,
        "summary": summary,
        "entries": entries,
    })

with tempfile.TemporaryDirectory() as tmp:
    source = os.path.join(tmp, "pages.soil")
    with open(source, "w") as handle:
        handle.write("(defun soil-doc-pages () (json-decode ")
        handle.write(json.dumps(json.dumps(pages)))
        handle.write("))\n")
    files = run_soil([source, "sites/soil-lang/docs.soil"])
    if not isinstance(files, list):
        raise SystemExit("docs.soil did not return a file list")
    for item in files:
        path = os.path.join(root, "sites/soil-lang/dist", item["path"])
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as handle:
            handle.write(item["text"])

# Andes manifest. Andes lives in its own repository; tend can consume it as a
# git dependency and point `repoRoot` at this checkout.
manifest = {
    "name": docs_copy["name"],
    "description": docs_copy["description"],
    "navigation": [{
        "group": docs_copy["nav-group"],
        "audience": docs_copy["audience"],
        "pages": [
            {
                "title": "Overview",
                "slug": "api",
                "file": "sites/soil-lang/dist/docs/index.md",
                "description": "Generated API documentation.",
            }
        ] + [
            {
                "title": title,
                "slug": slug,
                "file": "sites/soil-lang/dist/docs/%s.md" % slug,
                "description": summary,
            }
            for slug, title, summary in sources
        ],
    }],
}
with open(os.path.join(root, "sites/soil-lang/dist/docs.json"), "w") as handle:
    json.dump(manifest, handle, indent=2)
    handle.write("\n")

# Routes crawlers expect. The llms.txt index is generated in Soil above.
base = "https://soil-lang.org"
urls = ["/", "/docs/"] + [
    "/docs/%s.html" % slug for slug, _, _ in sources
]
sitemap = [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
]
for url in urls:
    sitemap.append("  <url><loc>%s%s</loc></url>" % (base, url))
sitemap.append("</urlset>")
with open(os.path.join(root, "sites/soil-lang/dist/sitemap.xml"), "w") as handle:
    handle.write("\n".join(sitemap) + "\n")
with open(os.path.join(root, "sites/soil-lang/dist/robots.txt"), "w") as handle:
    handle.write("User-agent: *\nAllow: /\n\n")
    handle.write("Sitemap: %s/sitemap.xml\n" % base)
    handle.write("# LLM index: %s/llms.txt\n" % base)

print("wrote sites/soil-lang/dist/docs/, llms.txt, llms-full.txt, sitemap.xml, robots.txt")
PY
