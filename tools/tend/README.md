# tend.soil

Data-only project manifest for the Soil build tool.

```lisp
(tend
  (version 1)
  (members "crates/*")
  (test "soil-lang"
    (include "crates/soil-lang/tests/soil/*.soil")
    (prelude "crates/soil-lang/stdlib/prelude.soil"))
  (bench "runtime"
    (include "benches/*.soil")
    (runs 100))
  (dependency "soil-spore" (path "crates/soil-spore")))
```

`tools/tend/manifest.soil` reads and writes this shape. It is a schema reader,
not an evaluator: no forms run, no host calls are available, and unknown keys
are refused.

The same data will drive:

- `tend test`
- `tend bench`
- browser/server placement
- dependency resolution and `tend.lock`

The eventual `tend` command is a privileged tool profile, not a normal Spore
grant.

## Lint

`tools/tend/lint.soil` is the first static analysis slice:

- duplicate definitions
- definitely unreachable user functions from declared entry points
- calls to known user functions with the wrong number of arguments

Dead code is a warning, not an error: quoted symbols, `apply`, and dynamic
evaluation can reach a function that a static call graph does not see.

The linter intentionally does not try to replace the reader. Parenthesis and
syntax balance are already the reader's job; `tend` reports that failure rather
than pretending to recover from it.
