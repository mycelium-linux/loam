# AGENTS.md

Read [`.agents/skills/working-on-soil/SKILL.md`](.agents/skills/working-on-soil/SKILL.md)
before editing `.soil` files. It covers form shapes, capability boundaries,
safe structural edits, and the test harness.

## Layout

- `crates/soil-lang` — reader, interpreter, stdlib, and Wasm backends.
- `wit/` — canonical capability contracts; generated bindings live in
  `crates/soil-lang/bindings/` and are not hand-edited.
- `docs/README.md` — docs index.
- `docs/loam.md` — language/runtime split and the LOAM VM.
- `docs/capabilities.md` — the full package/grant table.
- `integrations/pi/soil` — structural `.soil` editing extension.
- `integrations/pi/mush` — Mush shell/VFS extension.
- `sites/soil-lang` — the static site source. `sites/soil-lang/dist` is generated;
  do not commit it.

## Local site

```sh
./sites/soil-lang/serve.sh
# or serve the last build
SKIP_BUILD=1 ./sites/soil-lang/serve.sh
```

## Rules

- Work in a worktree, commit on a branch, push, and open a PR.
- Keep host credentials, transports, and side effects out of `.soil` files.
- Never use a live device or provider in a test.
- Do not edit generated `crates/soil-lang/bindings/*` by hand.
- `spores-public` is currently a submodule and `spores` is a symlink into it;
  treat both as external until the dependency is moved out of the checkout.
