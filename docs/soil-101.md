# Soil 101 for Loam users

Soil is the language and package format. Loam is the native runtime that runs
the result. This page is the shortest path from a Soil form to a page served at
a local `.myc` name.

## A form

Soil is a Lisp. Forms are the unit of work:

```soil
(defun greet (name)
  (str "Hello, " name "!"))
```

`defun`, `str`, and the function call are forms. A Soil program can expose a
command, a web surface, a Canvas view, or a component contract.

## A Spore is a package

A Spore is not a second language. It is a directory with a manifest, assets, and
optionally a component:

```text
hello-soil/
  spore.toml
  web/index.html
```

The important manifest fields are the surface and the claim:

```toml
[launch]
default = "web"
surfaces = ["web"]

[[surfaces]]
name = "hello"
kind = "web"
entry = "web/index.html"

[anycast]
name = "hello"
kind = "web"
alpns = ["soil/http/1"]
ttl_secs = 240
```

The surface says **how to draw it**. The anycast claim says **what name it
wants**. A package with a page but no claim is local-only.

## Run the demo

The checked-in example is static so it runs on every host while the Spore
compiler is still a bootstrap:

```sh
cp -R examples/hello-soil ~/.mycelium/spores/
loamd serve --root ~/.mycelium/spores --web-listen 127.0.0.1:8799
```

Verify the local listener first:

```sh
curl -sS -D- -H 'Host: hello.myc' http://127.0.0.1:8799/
```

Then the Mycelium frontdoor derives `hello.myc` from the published lease and
serves it on `:443`:

```sh
mycelium frontdoor status --json
curl -k -sS -D- https://hello.myc/
```

Use `-k` only to separate routing from certificate trust. If `-k` works but the
normal request warns, the page is fine and the certificate or local CA is the
problem.

## Commands, services, and views

Entry points are roles, not separate package kinds:

- A web surface serves `web/index.html` and its assets.
- A command is a `wasi:cli` component run by `loam run` or `loamd start`.
- A service exports `wasi:http/incoming-handler` and is routed by the gateway.
- A Canvas surface ships Lua and opens through `loam surface`.

The manifest may declare several. The host decides which one it can present.

## What Loam does not do

Loam does not compile Soil source, interpret forms, format files, or run an
editor. Those are Soil tools. Loam verifies the package, links granted WIT
imports, instantiates the component, and enforces the grant boundary.

The current Soil bootstrap compiler cannot yet lower arbitrary host calls into
typed WIT imports. The web demo above is deliberately static; the manifest and
routing path are real, and the component path becomes real when the compiler
can emit a complete Spore component.

## Debugging a 404

```text
curl -H 'Host: hello.myc' http://127.0.0.1:8799/
  -> package, entry, or web listener
mycelium services --json
  -> lease publication
mycelium frontdoor status --json
  -> derived route and refresh
curl -k https://hello.myc/
  -> TLS only
```

Do not restart `loamd` for every edit. Static files are read per request.
`loamd reload` refreshes commands and plugins. A new web package needs one
startup rescan; a frontdoor route or certificate change does not.
