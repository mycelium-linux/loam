# soil-lang.org

The first landing page for Soil, written as a single `.soil` view.

Preview it full-screen through the dev proxy:

```sh
# 1. Rust host (no rebuild needed for the site source)
.bin/soil-ui-proto --source sites/soil-lang/soil-lang.soil \
  --prelude sites/soil-lang/copy.soil --port 7882

# 2. Serve the current web assets from disk and proxy the API
SOIL_UI_UPSTREAM=http://127.0.0.1:7882 SOIL_UI_PORT=7883 \
  node crates/soil-ui-proto/web/dev-proxy.mjs

# 3. Open the bare preview (no editor panel)
open 'http://127.0.0.1:7883/?bare=1'
```

The landing page is a static landscape: nav, hero, why, substrate, learn,
spores, community, footer. `getting-started.html` is the local guide: build,
run a file, and try the examples. It has no actions and no state. All editable
prose lives in `sites/soil-lang/copy.soil`; `soil-lang.soil` owns the landing
structure and CSS, and `getting-started.soil` owns the guide.

Build and serve the static site locally:

```sh
./sites/soil-lang/serve.sh
# open http://127.0.0.1:8000/
# use SKIP_BUILD=1 ./sites/soil-lang/serve.sh after a build
```

Build the static page and every example through the Soil standard library:

```sh
./sites/soil-lang/build.sh
# sites/soil-lang/dist/index.html
# sites/soil-lang/dist/getting-started.html
# sites/soil-lang/dist/examples/*.html
```

Cargo is frozen for this step: the script prefers `.bin/soil-lang` (then
`target/debug/soil-lang`) and fails rather than building. The output is plain
HTML with server-rendered syntax highlighting. `dist/` is generated and checked
in for deployment; the live component path is the dev proxy at
http://127.0.0.1:7883/?bare=1.

## Self-documenting API

`;;;` comments immediately above `defun`/`defmacro`/`defspore` are the source of
truth. The projection lives in Soil:

- `crates/soil-lang/stdlib/doc.soil` extracts comments and renders Markdown,
  `llms.txt`, and the human-readable HTML pages.
- `sites/soil-lang/docs-copy.soil` owns only the page catalog: order, titles,
  summaries, and manifest metadata. The entries on each page still come from
  the `;;;` comments above their forms.
- `sites/soil-lang/docs.sh` feeds the extractor stack-sized source windows and
  writes the generated files. `build.sh` runs it automatically.

```sh
./sites/soil-lang/docs.sh
```

Generated routes:

```
dist/docs/              human-readable API pages
dist/docs/*.md          raw Markdown for agents
dist/llms.txt           llms.txt index
dist/llms-full.txt      full concatenated Markdown
dist/docs.json          Andes navigation manifest
dist/sitemap.xml
dist/robots.txt
```

The live dev proxy serves `/docs/`, `/docs/*.md`, `/llms.txt`,
`/llms-full.txt`, `/sitemap.xml`, and `/robots.txt` from `dist/`.

`docs.json` points at `sites/soil-lang/dist/docs/*.md` relative to the Soil
checkout. Andes stays in its own repository
([`ajmwagar/andes`](https://github.com/ajmwagar/andes)); it is not vendored here.
Tend should consume Andes as a git dependency and point its `repoRoot` at this
checkout.
