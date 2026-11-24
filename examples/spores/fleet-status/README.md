# fleet-status

A read-only web Spore for the machine and the fleet. It keeps the frontdoor
dashboard's visual language, but drops the forms: this is the page you leave
open to answer “what is running, what is connected, and is the frontdoor
healthy?”

The page reads:

- `GET /api/mycelium/status` — Loam's host bridge to
  `mycelium node status --json`, `fleet status --json`,
  `frontdoor status --json`, and `iroh-tun status --json`;
- `GET /api/spore/entries` — the installed Loam catalog and running surfaces.

Both are host-mediated. The Spore has no process, socket, or credential grant.
It needs `soil:mycelium.status` and `soil:spore.catalog`.
