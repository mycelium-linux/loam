# Soil Lisp performance tooling

The native interpreter exposes lightweight counters so a REPL session or a
script can separate evaluator work from host work.

## Builtins

```soil
(runtime-stats)
;; ((:evals 5438) (:host_calls 200) (:host_ms 0) (:elapsed_ms 8))

(runtime-reset)
```

- `evals` — evaluator forms entered.
- `host_calls` — `host-call` dispatches.
- `host_ms` — wall time spent inside host calls, not the evaluator.
- `elapsed_ms` — since construction or the last reset.

`runtime-stats` is also emitted by `perf/eval`, so a timing includes the work and
the counters for that evaluation.

## stdlib/perf.soil

Load it beside the prelude:

```sh
.bin/soil-lang \
  crates/soil-lang/stdlib/prelude.soil \
  crates/soil-lang/stdlib/perf.soil \
  crates/soil-lang/bindings/soil-time.soil \
  --repl
```

```soil
(perf/measure "ready" 'marbles-ready (list "marbles"))
(perf/bench 'marbles-list (list "marbles") 10)
(perf/eval "(length (marbles-ready \"marbles\"))")
```

`perf/bench` accepts a defined function symbol, not a builtin form, because the
evaluator's `apply` dispatches named functions. Wrap a builtin in a `defun` when
benchmarking it.

## REPL commands

The native `--repl` accepts:

```text
:stats          JSON counters for this image
:time FORM      evaluate once and print elapsed ms
:bench N FORM   evaluate FORM N times
:reset          reset counters
:help           list commands
:quit           leave
```

The REPL keeps one `Runtime`, so `:stats` after repeated work shows cumulative
evaluator and host counters until `:reset`.

## Records query and aggregate pushdown

Load `stdlib/records.soil` for the ergonomic wrappers:

```soil
(records/select "issue/" (list "project" "status"))
(records/where "issue/" "project" "marbles")
(records/aggregate "" "issue/" nil (list "project" "status") "" 0)
```

The underlying WIT-shaped types are records, not SQL strings:

```soil
(records/filter "project" "eq" "marbles")
(records/spec "" "issue/" filters fields nil 4096)
```

`aggregate` supports exact/prefix/contains/exists filters, dotted field paths,
and an optional numeric time bucket. `mb stats` uses it directly. `mb burndown`
uses it for project/all scopes; a goal/epic-scoped burndown still needs
descendant ids and falls back to the issue list. SQL remains inside the records
host; Soil sees typed groups.

## Why both counters

A records command can be slow because it evaluates a large list or because it
crosses the host boundary too often. `evals / host_calls` answers that before
changing the implementation. A high `host_calls` with low `host_ms` means many
cheap calls; high `host_ms` means one expensive host operation. A high `evals`
with low host time means interpreter/list recursion is the cost.
