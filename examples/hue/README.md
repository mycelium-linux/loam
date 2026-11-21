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

```sh
export HUE_BRIDGE=http://192.168.1.10
export HUE_KEY=...            # the application key, never in the source

cargo run -p soil-lang --bin soil-lang -- \
  --workspace-root . \
  --records-root /tmp/soil-records \
  --prelude crates/soil-lang/stdlib/prelude.soil \
  --allow "$HUE_BRIDGE" \
  --http-credential-env HUE_KEY \
  --repl \
  crates/soil-lang/bindings/soil-http.soil \
  examples/hue/hue.soil
```

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

Store the returned `username` as `HUE_KEY` (or in Geode) and restart the REPL
with `--http-credential-env`.

## What is not here yet

- Bridge discovery (mDNS/SSDP) and the Geode lease itself are host work.
- The Room/light UI is still `spores/lights-view`; this library is the seam it
  will call once the view is ported.
- `hue/pair` is the one call that intentionally has no credential: it is the
  pairing exchange itself.
