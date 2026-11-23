# Local web Spores

Three manifest-declared web surfaces used by `loamd` and the local frontdoor:

| Spore | Name | Data |
|---|---|---|
| `home` | `home.myc` | desktop shell; iframes `notes.myc`, `contacts.myc`, `files.myc` |
| `notes` | `notes.myc` | Markdown files under the `notes/` VFS root |
| `contacts` | `contacts.myc` | `contacts/contacts.json` in the VFS |
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
toggles with `Cmd/Ctrl+.`. It is UI only: the harness is deliberately not
connected, so a submitted prompt is recorded in the thread without claiming an
edit happened.
