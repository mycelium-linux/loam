# Soil for Pi — v1

A Pi extension that makes `.soil` files structurally editable, checkable,
evaluable and checkpointed. The file on disk stays canonical; the extension
builds a small form outline, edits a candidate, validates it with `soil-lang`,
then writes atomically.

## Install

Run directly while developing:

```sh
pi --extension /path/to/soil/integrations/pi/soil/index.ts
```

Install persistently by symlinking the extension directory:

```sh
mkdir -p ~/.pi/agent/extensions
ln -sfn /path/to/soil/integrations/pi/soil ~/.pi/agent/extensions/soil
```

Restart Pi or run `/reload`.

The extension needs a Soil checkout. It finds one from the current git root or
`SOIL_ROOT`; otherwise it walks up looking for `crates/soil-lang`. It runs
`target/debug/soil-lang`, `target/release/soil-lang`, or falls back to
`cargo run -q -p soil-lang --bin soil-lang --`. Override with `SOIL_LANG_BIN`.

## Tools

| Tool | What it does |
|---|---|
| `soil_read` | Form outline: paths, spans, heads, previews, nested children. |
| `soil_edit` | Top-level `replace` / `insert_before` / `insert_after` / `delete` / `append`. Validates a candidate before applying; restores the original if the post-apply check fails. Supports `dry_run`. |
| `soil_check` | Parse/load with `soil-lang --no-call`. Optional `tests: true` runs the Soil test corpus. |
| `soil_eval` | Evaluate an expression in the context of a file. This can have side effects. |
| `soil_diff` | Git diff for a `.soil` file or all `.soil` changes. |
| `soil_checkpoint` | Commit changed `.soil` files on the current feature branch. Refuses `main`/`master`. |

Commands:

```text
/soil-check path/to/file.soil
/soil-checkpoint [message]
```

`soil_edit` only addresses **top-level** forms in v1. The target `form` is the
zero-based index in `soil_read`'s top-level outline. Nested-form paths are shown
for navigation but not yet accepted as edit targets.

## Raw edits

`Edit` and `Write` on `.soil` files are blocked by default so the model uses
`soil_edit`. To deliberately bypass:

```sh
SOIL_PI_ALLOW_RAW_EDITS=1 pi
```

The hook is an honor system, not a sandbox.

## Safe transaction

`soil_edit` follows the safe path:

1. read the file and keep the original bytes;
2. apply the operation to a candidate string;
3. write the candidate to a temp file;
4. run `soil-lang --no-call` on it;
5. reject on failure, or atomically replace the real file;
6. load-check the real file again;
7. restore the original bytes if the post-apply check fails.

It does **not** evaluate the candidate. Evaluation is a separate explicit tool
because it can turn on lights, publish events, write records or spend tokens.
For side-effecting code, prefer a plan/dry-run function.

## Version control

The extension uses git for durable history:

- checkpoints refuse `main`/`master`;
- only `.soil` files are committed by default;
- `paths` restricts a checkpoint to touched files;
- nothing is auto-committed on every edit.

A typical loop:

```text
soil_edit → soil_check → soil_diff → soil_checkpoint
```

Before stopping, commit the intended files and open a PR as usual.

## Limitations in v1

- top-level form edits only;
- no semantic form diff yet (git diff + before snippet);
- `soil_check` is parse/load, not a type checker;
- `soil_eval` is one-shot, not a persistent REPL;
- no cross-file transaction journal yet; use git if a multi-file edit fails.

The companion skill is at `.agents/skills/working-on-soil/SKILL.md`.
