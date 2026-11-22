# Playback in Soil

Short answer to “soil:playback or Unibus?”: **both, at different layers**.

- **Unibus is the wire.** The owner publishes `audio.playback.state.v1`,
  `audio.playback.event.v1`, and receives `audio.playback.command.v1`. That is
  how several controllers see one queue and how focus/desk arbitration works.
- **`soil:playback` is the guest edge.** A Soil program should not need a bus
  config, a provider token, or a command envelope. It asks for `snapshot`,
  `send`, `search`, and `desk`; the host adapter speaks Unibus and owns the
  credentials. `send` carries an expected revision so a stale command fails
  instead of landing on the wrong queue.

The canonical `soil:playback` WIT is described in `spores/docs/playback.md`,
but it is not in this Soil checkout yet. This directory is the practical slice
until it lands: HTTP control of the existing `deadwax serve` catalog/MPD API
and the Spotify Web API, using `soil:http`. The vocabulary is deliberately the
same so the transport can be swapped later.

## deadwax / MPD

```lisp
(defun wax () (deadwax/source "http://pi.local:6601"))

(deadwax/status (wax))
(deadwax/queue (wax))
(deadwax/search (wax) "board of canada" 20)
(deadwax/playlists (wax))

(deadwax/control (wax) "toggle" nil)
(deadwax/control (wax) "next" nil)
(deadwax/control (wax) "volume" 65)
(deadwax/control (wax) "add" "file:///srv/music/track.flac")
(deadwax/control (wax) "play-playlist" (list "avery" "Late Night"))
```

`deadwax serve` is the owner: the Soil program names a catalog and a CID/URI,
never a path or a sample.

## Spotify

Spotify needs one host-owned bearer token. Run the REPL with
`--allow https://api.spotify.com --bearer-env SPOTIFY_TOKEN`; the guest only
names the handle `default`.

```lisp
(defun spot ()
  (spotify/source "default" nil))          ; active device

(defun pi ()
  (spotify/source "default" "raspotify"))  ; device id

(spotify/devices (spot))
(spotify/status (spot))

(spotify/control (spot) "play" "spotify:album:1DFixLWuPkv3KT3TnV35m3")
(spotify/control (pi) "volume" 40)
(spotify/control (pi) "next" nil)
(spotify/transfer (spot) "raspotify" #t)
```

`spotify/device-id-by-name` turns a `devices` result into the id used above.

## CLI

`playback_cli.soil` is the command surface, in the same shape as the Hue and
Marbles CLIs. `examples/playback/playback` is a thin host launcher; it supplies
the deadwax origin and, when asked, an SSH tunnel to a loopback-only deadwax.

```sh
export DEADWAX_SSH=ajm@192.168.10.96     # home-pi's deadwax is 127.0.0.1:6601
examples/playback/playback health
examples/playback/playback status
examples/playback/playback now
examples/playback/playback queue
examples/playback/playback playlists
examples/playback/playback playlist avery "Rise ‘n Grind"
examples/playback/playback search "board of canada"
examples/playback/playback volume 65
```

The first command starts a persistent `ssh -f -N -L 6601:127.0.0.1:6601`
tunnel and later commands reuse it. Set `DEADWAX_URL` when deadwax is already
reachable; set `DEADWAX_LOCAL_PORT` to move the tunnel.

`SPOTIFY_TOKEN` enables the `spotify-*` commands:

```sh
export SPOTIFY_TOKEN=...
examples/playback/playback spotify-devices
examples/playback/playback spotify-status
examples/playback/playback --spotify --device <id> spotify-play spotify:album:...
```

The deadwax CLI never needs a credential. The token stays in the host process;
the Soil program only names the opaque `default` handle.

The fake-deadwax regression tests live in
[`crates/soil-lang/tests/playback_cli.rs`](../../crates/soil-lang/tests/playback_cli.rs).

## Common verbs

`playback/control` accepts the same verbs for both owners where they make sense:

```lisp
(playback/control (wax) "next" nil)
(playback/control (spot) "next" nil)

(playback/control (wax) "volume" 60)
(playback/control (spot) "volume" 60)
```

Verbs: `play`, `pause`, `next`, `prev`, `volume`, `seek`, `queue` (Spotify),
`add`/`play-position`/`play-playlist` (deadwax), `shuffle`/`repeat` (Spotify).

## Why not a macro here?

Soil’s evaluator has `defun` but not `defmacro` yet. Adding a one-off macro just
for these wrappers would fork the language. The dispatcher gives the same
reduction in public surface without new evaluator semantics. When macros land,
the `cond` in `deadwax/control` and `spotify/control` can become a table if it
is still worth it.
