# marbles

A read-only Marbles report surface rendered by Soil. The host supplies one
snapshot assembled from the reports the CLI already emits:

- `marbles burndown --json`
- `marbles portfolio --json`
- `marbles goal list --json` and `marbles goal show <id> --json`
- `marbles plan gantt <plan> --json` when `MARBLES_PLAN` is set
- `marbles list --json`

`ui.soil` decides every pixel: burndown chart, portfolio rollup, goal progress,
and the ready queue. No client JavaScript, no Canvas, no Lua.

This is a projection, not a second tracker. The Rust/SQLite or `soil:records`
store stays authoritative.

## Build

```sh
./examples/marbles/build.sh
# examples/marbles/dist/index.html
```

Cargo is frozen: the script uses `.bin/soil-lang` or `target/debug/soil-lang`.
The fixture in `snapshot.json` is synthetic but uses the real report field
shapes.

## Refresh from the records store

`mb` is the records-backed command, so this path does not use a fixture:

```sh
MARBLES_DIR=/path/to/repo ./examples/marbles/refresh.sh
./examples/marbles/build.sh
```

`refresh.sh` runs the four reports from `MARBLES_DIR` (default `.`) and writes
the resolved scope into `snapshot.json`:

- project scope when a `.marbles/project.toml` is found;
- store scope with `MARBLES_SCOPE=store`.

`mb list`, `mb goal list`, and `mb burndown` are project-scoped by the shim's
project discovery. `mb portfolio` is store-wide; the UI selects the project
rollup named by `scope.slug`. Set `MARBLES_PLAN=<name>` to include the plan
Gantt; without it the plan section is omitted. See
[`docs/marbles-scope.md`](../../docs/marbles-scope.md) for the long-term
contract and explicit `--project`/`--folder`/`--goal` flags.

## Soil CLI

`examples/marbles/mb.soil` is the Soil `mb` replacement for the records-backed
command surface. Reads use the host's typed `query` projection, so
`list --project` does not decode every issue in the book. Writes append the same
Marbles JSON bodies and audit events, and `claim` uses `compare-and-swap` on the
revision and origin it read.

```sh
.bin/soil-lang   --records-root "$HOME/.local/share/fungos/records"   --book personal/avery/marbles   crates/soil-lang/stdlib/prelude.soil   crates/soil-lang/stdlib/cli.soil   crates/soil-lang/bindings/soil-records.soil   crates/soil-lang/bindings/soil-time.soil   crates/soil-lang/bindings/wasi-cli.soil   examples/marbles/mb.soil -- list --project marbles --limit 5

# lifecycle
... examples/marbles/mb.soil -- create "Measure the thing" \
  --project marbles --prefix marbles --available-at 0
... examples/marbles/mb.soil -- claim <id> --as agent-7 --ttl 900
... examples/marbles/mb.soil -- review <id> --pr https://github.com/org/repo/pull/1
... examples/marbles/mb.soil -- close <id> --pr https://github.com/org/repo/pull/1 --commit <sha>
```

Ported commands: `list`, `ready`, `show`, `history`, `create`, `update`,
`claim`, `touch`, `release`, `review`, `close`, `supersede`, `sweep`,
`dep add|rm`, `label add|rm`, `stats`, `projects`, `doctor`, `project rm`,
`goal`, `epic`, `task`, `folder`, `burndown`, `portfolio`, and
`plan show|gantt|ready|create`.

`plan create` accepts an inline JSON spec. The native Rust backend also accepts
TOML; this Soil slice does not have a TOML reader yet.

The records are the same on both sides: entity ids, schemas, schemas’ bodies,
dependency edges, events, and outbox rows. Verified both ways against the
installed Rust Spore: it reads Soil-created issues/events/stats, and Soil reads
Rust-created issues/events.

The parser, spec builders, and record helpers are reusable. Load the same files
in a Soil REPL and call the generic entry point:

```soil
(marbles "list" (list "--project" "marbles"))

; tiny projections of the same command
(marbles-ready "marbles")
(marbles-list "marbles")
(marbles-show "marbles-an4z")
```

There is no `defmacro` in the runtime yet, so the source stays small with data
builders (`cli-spec`, `cli-command-entry`, `cli-opt`) and one `marbles` function
rather than a generated wrapper per command.

For timing and counter-based profiling, load `stdlib/perf.soil` and use
`perf/measure`, `perf/bench`, `perf/eval`, or the REPL’s `:stats`, `:time`,
`:bench`, and `:reset` commands. See
[`docs/soil-lisp-perf.md`](../../docs/soil-lisp-perf.md).

`query`, `aggregate`, and `compare-and-swap` are currently native `soil-lang`
host extensions. They still need to be promoted into the canonical
`soil:records` WIT and the `soil`/`soil-command` hosts for a wasm `mb`.

## Live host

The same view runs through the dev host with the snapshot as a prelude:

```sh
./examples/marbles/build.sh   # writes .state.soil for the host too

.bin/soil-ui-proto \
  --source examples/marbles/ui.soil \
  --prelude examples/marbles/.state.soil \
  --port 7892

SOIL_UI_UPSTREAM=http://127.0.0.1:7892 SOIL_UI_PORT=7893 \
  node crates/soil-ui-proto/web/dev-proxy.mjs

open 'http://127.0.0.1:7893/?bare=1'
```

The static build is pure SSR; the live host adds the existing Soil reconciler
so source edits hot-reload. Actions are intentionally no-ops for now.

## Ported views

- **Burndown** — scope, done, remaining, burn/day, projected days, actual and
  ideal series.
- **Portfolio** — folder/project/goal rollup with progress bars, ready work,
  and staleness.
- **Goals** — descendant progress plus the `goal show` tree.
- **Plan** — Gantt rows, estimates, critical path, concurrency.
- **Queue** — ready/in-flight/review/blocked lanes and recently closed.

`stats` and project/all-scoped `burndown` now use the host-side `aggregate`
pushdown, so they return group counts instead of every issue body. `doctor`,
`plan`, and `portfolio` still materialize issues in Soil and remain slower on a
large book. Promote `query`, `aggregate`, and `compare-and-swap` into canonical
WIT for a wasm build.
