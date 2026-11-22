# home

One bounded snapshot and one command vocabulary over the working home adapters.

`home.soil` is deliberately small: it composes `hue.soil` and `playback.soil`
instead of inventing a second device model.

```lisp
(defun state ()
  (home/snapshot
    (home/config "http://192.168.1.10" "default" "Living room"
                 "http://127.0.0.1:6601")))

(defun pause-music ()
  (home/command
    (home/config "http://192.168.1.10" "default" "Living room"
                 "http://127.0.0.1:6601")
    (list (list :kind "media") (list :verb "pause") (list :value nil))))
```

A snapshot is `home.snapshot/1`:

- `:room` — resolved room plus its lights, or an error for that section
- `:media` — health, MPD status, queue, and the current queue item, or an error
- `:cameras` — an explicit `unavailable` card until a camera adapter exists

Each adapter fails independently. A dead player does not blank the lights, and
a dead bridge does not blank the player. That is the point of the façade.

Commands:

- `(:kind "mood" :mood "sunset" :layout nil)` — verified Hue mood apply
- `(:kind "media" :verb "pause" :value nil)` — playback control
- `(:kind "playlist" :user "avery" :name "Rap")` — deadwax playlist

## SSR dashboard

`ui.soil` renders the snapshot with the shared HTML/CSS stdlib. `dashboard.soil`
is the entry point. There is no client JavaScript.

```sh
export HUE_BRIDGE=http://192.168.10.15
export HUE_ROOM="Living room"
export DEADWAX_SSH=ajm@192.168.10.96   # if deadwax is loopback-only
./examples/home/build.sh
# examples/home/dist/index.html
```

The build script starts a persistent SSH tunnel when `DEADWAX_SSH` is set, then
runs Soil with the Hue origin, the deadwax origin, and the Geode credential
handle. `HUE_KEY` is the development fallback for a raw key; Geode is the
shipping path. `DEADWAX_URL` bypasses the tunnel when deadwax is already
reachable.

The camera card is intentionally honest: `unavailable — no camera adapter
configured`. The Frigate/NVR adapter can fill that section later without
changing the UI contract.

Tests:

```sh
cargo test -p soil-lang --test home_state
```
