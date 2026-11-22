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

## Documentation

Top-level `;;;` comments are documentation and stay Markdown. The reader
preserves their raw text and source span; `tools/tend/doc.soil` extracts them
without reflowing the Markdown.

```lisp
;;; # Claim
;;;
;;; `tend claim <id>` checks the current version first.
(defun claim (ctx)
  ...)
```

Comments before the first form are the module documentation. Documentation
rendering and `tend doc --open` are separate from the extraction step.

## Hygiene

`tools/tend/hygiene.soil` checks the license header required by the repository:

```lisp
; SPDX-License-Identifier: MIT OR Apache-2.0
```

This is intentionally a separate check from parsing. A missing license line is
reported even when the file parses.

## Subcommands

Project-local commands are declared in `tend.soil`:

```lisp
(command "fmt"
  (summary "Format Soil sources")
  (run "tools/tend/fmt.soil" "main")
  (usage "tend fmt [--check] [paths...]"))
```

Built-in names are reserved. Command implementations are ordinary Soil
functions; the manifest stays data-only.

## Formatter

`tools/tend/fmt.soil` formats from the token stream, so comments and reader
syntax survive:

- nested comments are preserved in place
- `'x` stays `'x` rather than being rewritten
- `defun`, `let`, `if`, `cond`, `begin`, and `do` have explicit layout
- other lists stay inline when short and become two-space-indented lists when long
- top-level forms are separated by a blank line

The formatter is still conservative. It does not try to invent a layout for
forms it does not understand.

## Dispatch

`tools/tend/dispatch.soil` resolves a command name against the manifest and the
built-in command table:

- project command → returns its `run` target and args
- built-in command → returns the built-in name
- built-in shadowing → refused
- unknown command → refused

Execution belongs to the host; dispatch is pure Soil policy.
