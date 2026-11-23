---
name: loam
description: Expose a Spore as a .myc web app through loamd and the Mycelium frontdoor; diagnose 404s, missing leases, stale web surfaces, and TLS SAN failures without restarting loamd blindly. Use when installing or testing a web Spore, adding a dev/test app to DNS, or debugging why a .myc name returns 404.
---

# Loam web apps and `.myc` names

A `.myc` name is not registered by restarting `loamd`. It is the result of four
separate things agreeing:

```text
package manifest
  -> web surface + anycast claim
  -> loamd serves the files and publishes a Mycelium lease
  -> frontdoor derives a route from the lease
  -> TLS certificate covers the name
```

Change the thing that is wrong. Restarting `loamd` fixes only one of those:
the static web listener's startup scan.

## 1. The package must declare both facts

A package with a web page alone is not enough. It also needs an anycast claim.
With `spore.toml`:

```toml
[launch]
default = "web"
surfaces = ["web"]

[[surfaces]]
name = "studio"
kind = "web"
entry = "web/index.html"

[anycast]
name = "studio"
kind = "web"
alpns = ["soil/http/1"]
ttl_secs = 240
```

With `spore.soil`:

```soil
(spore
  (id "dev.local:studio")
  (version "0.1.0")
  (anycast
    (name "studio")
    (kind "web")
    (alpns ("soil/http/1"))
    (ttl_secs 240))
  (surfaces
    (surface "studio" (kind "web") (entry "web/index.html"))))
```

The `entry` file must exist. `entry` is relative to the package root.

For a workspace-backed web app, `spore.toml` may also declare a bounded VFS API:

```toml
[web]
vfs_root = "studio"
```

That gives the page `/api/vfs/*` under that prefix. It does not give the page
ambient filesystem access.

## 2. Install the package; do not hand-edit daemon state

```sh
loam install /path/to/studio
loam status --json
```

`loam install` copies the package into the command/Spore root that `loamd`
scans. A package already in a scanned root can be used directly.

Find the roots and the web listener:

```sh
loamd --help
mycelium frontdoor status --json
```

`loamd` serves manifest web surfaces on `--web-listen` (default
`127.0.0.1:8799`) and publishes each anycast claim to the Mycelium service
catalog. The lease is renewed at roughly `ttl / 3`.

## 3. Restarting `loamd`: when it is actually needed

The static web listener discovers packages when the daemon starts.

- Editing files inside an already-discovered package: **no restart**.
  Static files are read per request.
- Changing a command or plugin package: `loamd reload`.
- Changing the frontdoor `routes.json` or a lease: **no `loamd` restart**.
  The frontdoor refreshes its route table itself.
- Installing a *new* web package after `loamd` started: the current listener
  must rescan, so one `loamd` restart is the startup boundary. This is not the
  DNS registration step; do not repeat it for every file change.
- `loamd reload` refreshes commands and plugins, not the static web listener.

If you are restarting `loamd` more than once while iterating on one app, you are
probably fixing the wrong layer.

## 4. Debug in this order

### Is the package served locally?

```sh
curl -sS -D- -H 'Host: studio.myc' http://127.0.0.1:8799/
```

A response here proves the package, surface entry, and web listener are good.
A 404 or the wrong app here is a package/root/discovery problem, not DNS.

### Is the lease published?

```sh
mycelium services --json | jq '.. | objects | select(.names? // [] | index("studio.myc"))'
jq '.observations[] | select(.value.names[]? == "studio.myc")' \
  ~/.mycelium/application-services.json
```

No lease means the manifest has no `[anycast]`, the package was not discovered,
or the Mycelium identity/home is wrong.

### Did the frontdoor derive the route?

```sh
mycelium frontdoor status --json | jq '{health, routes, tls_verified, tls_error}'
curl -sS -D- http://127.0.0.1:8443/ -H 'Host: studio.myc'
```

`health.body.derived_routes` and the route table should mention the name. A
frontdoor with the name in `routes.json` but not in the catalog is using an
explicit override, not derived routing. Do not add derived names to
`routes.json`; that defeats the lease path.

### Is TLS the only failure?

```sh
curl -k -sS -D- https://studio.myc/
curl -sS -D- https://studio.myc/        # without -k
```

If `-k` works, routing is fine and the problem is certificate SANs or trust.
Restarting `loamd` cannot fix that. Check:

```sh
openssl x509 -in ~/.mycelium/frontdoor/server.pem -noout -ext subjectAltName
mycelium frontdoor status --json
```

`*.myc` does **not** cover `studio.myc` in OpenSSL/macOS, and it never covers
multi-label names such as `drawbridge.dev.myc`. The certificate must list the
actual names, and the local CA must be trusted.

## 5. Correct development loop

1. Edit the package under its source directory.
2. Keep `spore.toml`/`spore.soil` declaring the web surface and anycast.
3. Run `loam install PATH` when the package is new or the manifest/entry moved.
4. For static file edits, reload the browser. Do not restart `loamd`.
5. For a command/plugin change, run `loamd reload`.
6. For a new web package, restart `loamd` once so the listener rescans.
7. Use `mycelium frontdoor status --json` and `curl -H 'Host: name.myc'` to
   separate listener, lease, frontdoor, and TLS failures.

## Common mistakes

- Adding `studio.myc` to `routes.json` when the package has no anycast claim.
- Expecting a web page to be served without `[[surfaces]] kind = "web"`.
- Putting the page under the wrong root, or pointing `entry` at a missing file.
- Restarting `loamd` when only the frontdoor or TLS certificate is stale.
- Treating `*.myc` as a wildcard for every `.myc` name.
- Editing daemon state instead of the package manifest.
- Confusing a 404 (routing/serving) with a TLS warning (certificate identity).
