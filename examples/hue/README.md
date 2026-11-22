# Hue in Soil

`hue.soil` is the Philips Hue v1 local-bridge layer written in Soil. It owns
targets, scenes, brightness, color conversion, and the bridge's in-band error
decoding. It does **not** own the connection or the bridge key:

- the guest names the host handle `default`;
- the URL carries the literal `__CREDENTIAL__` where Hue expects the key;
- the host substitutes the key after the origin allowlist check.

The same file is useful in two places: as a library for a Soil program, and
interactively from the REPL while automating lights.

## REPL

The preferred path is Geode: the key stays encrypted in the daemon and the
guest only names the handle `default`.

```sh
export HUE_BRIDGE=http://192.168.1.10

cargo run -p soil-lang --bin soil-lang -- \
  --workspace-root . \
  --records-root /tmp/soil-records \
  --prelude crates/soil-lang/stdlib/prelude.soil \
  --allow "$HUE_BRIDGE" \
  --geode-socket "$HOME/.geode/geode.sock" \
  --geode-reference geode://home/hue/bridge \
  --geode-purpose hue.control \
  --repl \
  crates/soil-lang/bindings/soil-http.soil \
  examples/hue/hue.soil
```

For a key you have not put in Geode yet, `--http-credential-env HUE_KEY` is the
development fallback. It keeps the key out of the source but not out of the
host process environment.

Then, in the REPL:

```lisp
(defun home () (hue/bridge "http://192.168.1.10" "default"))

(hue/status (home))
(hue/lights (home))
(hue/groups (home))
(hue/scenes (home))

(hue/on (home) (hue/group "0"))
(hue/off (home) (hue/light "3"))
(hue/toggle (home) (hue/group "1"))
(hue/bri (home) (hue/light "3") 180)
(hue/color-hex (home) (hue/light "3") "#ff8800")
(hue/scene (home) (hue/group "1") "Relax")
```

`hue/status`, `hue/lights`, `hue/groups`, and `hue/scenes` return the decoded
bridge JSON. Every write returns `(ok <bridge reply>)` or
`(err "<description>")`, so a program can test with `ok?` instead of parsing
prose.

## CLI

`hue_cli.soil` is the command surface. It is written in Soil in the same shape
as `examples/marbles/mb.soil`: parse `wasi:cli` argv with `cli-parse-argv`,
dispatch, and exit non-zero on a usage or bridge error. `examples/hue/hue` is a
thin host launcher; it only supplies the origin allowlist and the Geode handle
that the Soil host cannot set for itself.

```sh
export HUE_BRIDGE=http://192.168.1.10          # the exact origin the host allows
examples/hue/hue status
examples/hue/hue rooms
examples/hue/hue plan sunset --room "Living room"   # no writes
examples/hue/hue mood sunset --room "Living room"
examples/hue/hue mood ocean --room "Living room" --json
examples/hue/hue scene Relax --room "Living room"
examples/hue/hue on --room "Living room"
examples/hue/hue bri 120 --room "Living room"
```

`--room` is matched case-insensitively. `--group ID` is the escape hatch when
two groups share a name or you already know the bridge id. Without either,
the target is the bridge's all-lights group `0`. `--json` prints the bridge
reply or the plan instead of the one-line summary.

Snapshot and restore:

```sh
SNAP=$(examples/hue/hue snapshot --room "Living room")
examples/hue/hue mood ocean --room "Living room"
examples/hue/hue restore --snapshot-json "$SNAP"
```

`hue/apply-plan-verified` treats the bridge's silent `[]` reply as a failure.
That is deliberate: the v1 bridge returns `[]` when it ignores a malformed
write, and the old code reported that as success.

The launcher reads:

| variable | meaning |
|---|---|
| `HUE_BRIDGE` | bridge origin, e.g. `http://192.168.1.10` |
| `HUE_GEODE_SOCKET` | default `$HOME/.geode/geode.sock` |
| `HUE_GEODE_REFERENCE` | default `geode://home/hue/bridge` |
| `HUE_GEODE_PURPOSE` | default `hue.control` |
| `HUE_WORKSPACE` / `HUE_RECORDS` | host VFS/records roots |

The key never enters argv or the Soil program. The host leases it from Geode
and substitutes the `__CREDENTIAL__` path segment.

The fake-bridge regression tests live in
[`crates/soil-lang/tests/hue_cli.rs`](../../crates/soil-lang/tests/hue_cli.rs);
they exercise apply-and-verify, the empty-reply failure, `plan` with no writes,
and snapshot/restore without a real bridge.

## Pair once

Press the physical link button on the bridge, then:

```sh
cargo run -p soil-lang --bin soil-lang -- \
  --workspace-root . --records-root /tmp/soil-records \
  --prelude crates/soil-lang/stdlib/prelude.soil \
  --allow "$HUE_BRIDGE" \
  --repl crates/soil-lang/bindings/soil-http.soil examples/hue/hue.soil
```

```lisp
(hue/pair (hue/bridge "http://192.168.1.10" nil) "soil#hue")
```

Store the returned `username` in Geode under `geode://home/hue/bridge` with the
purpose `hue.control`, restart `geoded`, and use the `--geode-*` flags above.

A lease can be checked without printing the secret:

```sh
printf '%s\n' '{"op":"lease","reference":"geode://home/hue/bridge","purpose":"hue.control","ttl_ms":60000}' \
  | nc -U "$HOME/.geode/geode.sock"
```

## Colour theory and spatial scenes

The library can turn a mood into a coordinated set of per-light writes instead
of one flat group action:

```lisp
(hue/mood "aurora")                       ; palette, brightness, spread, transition
(hue/atmosphere-plan (home) "81" "sunset" nil)  ; plan only; no bridge writes
(hue/atmosphere (home) "81" "aurora" nil)       ; apply it
```

Built-in moods: `aurora`, `sunset`, `ocean`, `ember`, `forest`, `nebula`,
`movie`, `focus`, `candle`.

Colour theory is exposed directly:

```lisp
(hue/harmony 190 0.68 0.55 "analogous")   ; HSL triples
(hue/harmony-rgb 190 0.68 0.55 "triadic") ; sRGB
(hue/hsl->rgb 210 0.7 0.5)
(hue/atmosphere-harmony (home) "81" 190 "split" nil)
```

Every request is clamped to the target lamp's CIE `colorgamut` before it is
sent, so a palette that looks good on paper does not get rejected by a bulb.
The bridge's own group order is the fallback spatial axis; for a real gradient,
pass a layout:

```lisp
(defun living-room-layout ()
  (list
    (list "3" 0.10 0.20)   ; light-id, x, y in normalized room space
    (list "8" 0.30 0.20)
    (list "4" 0.50 0.50)
    (list "7" 0.85 0.70)))

(hue/atmosphere (home) "81" "ocean" (living-room-layout))
```

Without a layout the gradient runs in bridge order. With one, `x` is the
position along the gradient and `y` is reserved for future 2D/radial fields.

## Timed demos

Demos are timed in Soil: each frame plans the room, writes it, waits on
`soil:time/tick`, then advances the phase. The palette is cyclic, so a moving
gradient loops without a seam.

```lisp
(hue/demo-aurora (home) "81" (living-room-layout))   ; ~6s, then restore
(hue/demo-ocean (home) "81" (living-room-layout))    ; ~9s
(hue/demo-ember (home) "81" (living-room-layout))    ; ~8s
(hue/demo-nebula (home) "81" (living-room-layout))   ; ~8s
(hue/demo-party (home) "81" (living-room-layout))    ; ~8s
(hue/demo-movie (home) "81" (living-room-layout))    ; ~8s
(hue/demo-sunrise (home) "81" (living-room-layout))  ; ~12s
(hue/demo-focus (home) "81" (living-room-layout))    ; ~8s
```

`hue/demo*` snapshots every lamp first and restores `on`, brightness and the
current colour mode afterwards. `hue/animate` leaves the final frame in place:

```lisp
(hue/animate (home) "82" "ocean" nil 40 200)
(hue/demo-for (home) "81" "aurora" (living-room-layout) 20000 300)
```

Keep `frame-ms` at 150 or more for a whole room; the bridge rate-limits bursts
of individual light writes.

## What is not here yet

- `discover` works without `HUE_BRIDGE`: the launcher allows the public
  discovery origin. A native host still has to allow the bridge origin before a
  status or mood call, which is why the launcher takes that origin from
  `HUE_BRIDGE`.
- mDNS/SSDP discovery is still host work.
- The Room/light UI is still `spores/lights-view`; this library is the seam it
  will call once the view is ported.
- `hue/pair` is the one call that intentionally has no credential: it is the
  pairing exchange itself.
