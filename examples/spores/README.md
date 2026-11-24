# Local web Spores

Three manifest-declared web surfaces used by `loamd` and the local frontdoor:

| Spore | Name | Data |
|---|---|---|
| `home` | `home.myc` | desktop shell; iframes `notes.myc`, `contacts.myc`, `files.myc` |
| `notes` | `notes.myc` | Markdown under `<desktop>/notes/` |
| `contacts` | `contacts.myc` | `soil:records` book `personal/avery/contacts` |
| `agent` | `agent.myc` | right-edge agent sidebar; embed it in any Spore |

Install a package by copying its directory under `~/.mycelium/spores/` and run:

```sh
loamd serve --socket ~/.mycelium/loamd.sock \
  --root ~/.mycelium/spores --web-listen 127.0.0.1:8799
```

The frontdoor derives `home.myc`, `notes.myc` and `contacts.myc` from the
published leases. `mycelium frontdoor install --write` serves them on :443.

## `[web]`

A static web surface is assets only. Adding `[web]` gives it a bounded JSON API
over the host workspace:

```toml
[web]
vfs_root = "notes"
```

`vfs_root` is a workspace-relative prefix. The API is mounted at `/api/vfs/`:

- `GET /api/vfs/list?path=` — files under the prefix
- `GET /api/vfs/read?path=file.md` — UTF-8 text plus version
- `GET /api/vfs/raw?path=image.png` — raw bytes
- `PUT /api/vfs/write?path=file.md[&expected_version=N]` — write; requires
  `soil:vfs-mut` in `[grants]`
- `DELETE /api/vfs/remove?path=file.md` — delete; requires `soil:vfs-mut`

Paths are relative to `vfs_root`; the host rejoins and normalizes them, so a
page cannot escape the declared prefix. The default workspace is
`~/.local/share/fungos/vfs`; `MYCELIUM_VFS_ROOT` overrides it.

## Desktops

A desktop is a top-level VFS folder and a privacy boundary. The apps receive a
`desktop` query parameter and the host prefixes every path with it:

```text
~/fungOS/personal/notes/welcome.md
~/fungOS/fpl/notes/plan.md
~/fungOS/work/notes/untitled.md
```

`GET /api/vfs/desktops` lists the available desktops from the default VFS. A
desktop may carry `<desktop>/desktop.json` for its display name and kind.
`home.myc` keeps the active desktop in local storage and appends `?desktop=` to
its iframe URLs. The app only ever sees paths inside that desktop, so the
desktop can be renamed or moved without changing the app's internal paths.

## Records surfaces

A web Spore may declare a records book:

```toml
[records]
book = "personal/avery/contacts"
collection = "contact"
```

`loamd` then serves the canonical `soil:records` call shape at
`POST /v1/call`:

```json
{"v":1,"call":{"id":"7","interface":"soil:records","function":"scan",
                "params":{"collection":"contact","limit":50}}}
```

Supported functions are `scan`, `query`, `get`, `upsert`, `compare-and-swap`,
and `delete`. `query` takes the canonical filters/cursor/limit shape, which is
what the People search box uses for paged name search. Reads require a records
grant; writes require `soil:records.write`. The page only sees the declared
collection and never names the book itself.

The pages are static assets and talk only to their own origin. A desktop shell
embeds them as iframes; the gateway forwards the browser Host as
`x-forwarded-host`, which is how several Spores share one listener.

## Agent sidebar

`agent.myc` is its own Spore and a reusable surface. Any page opts in with one
line:

```html
<script src="https://agent.myc/embed.js" defer></script>
```

Hovering the right edge opens a panel in an iframe. Apps may publish context by
setting `window.__MYCELIUM_AGENT_CONTEXT = { app, detail }` and dispatching a
`mycelium-agent-context` event. The panel pins open, closes with Escape, and
toggles with `Cmd/Ctrl+.`. Prompts go to the host at `POST /api/agent`, which
speaks ACP to the endpoint and model named in `loam.conf.soil`:

```soil
(agent
  (endpoint "http://127.0.0.1:8792/")
  (workspace "/path/to/trusted/checkout")
  (model "gpt-5.6-luna"))
```

`operator serve` speaks this ACP-over-HTTP surface. The sidebar keeps the
session id in local storage, so a second prompt continues the same session.
