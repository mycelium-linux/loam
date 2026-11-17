# Two profiles: installed and dev

The desktop app is the profile you rely on: one launcher module inside
`fungOS.app` and the packages under the stable root. Development needs a second
one beside it, because a half-built launcher currently replaces the one you use
and a broken Spore can empty the list you open every day.

A profile is two things and nothing more:

| | Installed | Dev |
| --- | --- | --- |
| Launcher module | `fungOS.app/Contents/Resources/launcher.wasm` | `~/.local/share/fungos/dev/launcher.wasm` |
| Spore packages | `~/.local/share/fungos/spores` | `~/.local/share/fungos/dev/spores` |

## How the catalogue chooses

`canvas-winit` searches roots in order and the **first root to offer an id
wins**, so the same app can exist twice and only one of them is seen:

1. `--spores-overlay DIR` (repeatable), then `SOIL_SPORES_OVERLAY`
2. `~/.local/share/fungos/spores`
3. `~/Library/Application Support/fungOS/spores`
4. `SOIL_SPORES_ROOTS`, kept last so an environment convenience can never
   shadow an installed package

`--spores DIR` still means *only* this root, with nothing else visible. It is
the right flag for a test and the wrong one for daily work, which is the whole
reason the overlay exists.

## The dev profile

`scripts/fungos-dev` drives both halves:

```sh
fungos-dev build-launcher spores/spores/launcher-gpui   # dev launcher module
fungos-dev install spores/spores/dfm-report             # dev package only
fungos-dev run                                          # canary launcher
fungos-dev status                                       # what each holds
fungos-dev promote dfm-report --replace                 # graduate it
```

`install` builds a package for `wasm32-wasip2` when it is a crate, refreshes its
lock if `soil-spore` is on `PATH`, and copies it into the dev root. `promote`
copies it into the installed root, which is the only command that touches what
the app runs.

Nothing about this is a security boundary: both roots are yours, the same UID
runs both, and a dev package keeps the same grants and consent rules. It is a
workflow boundary — the point is that a broken build is visible in one window
and the daily app keeps opening.

## What stays put

- The `.app` bundle is never touched. `promote` writes to the packages root, and
  a new launcher module reaches the bundle only through a release.
- The dev launcher compiles with `--release`, because a debug component is tens
  of times larger and the runtime compiles it on every launch.
