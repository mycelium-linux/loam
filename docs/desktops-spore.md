# Desktops Spore

Status: **landed, v0 is a view.** The package is
[`spores/desktops`](https://github.com/fungOS-linux/spores/tree/main/spores/desktops):
a Teams and Desktops management surface. `soil-desktop` holds the model,
`desktops-spore` turns a snapshot and an intent into one manifest write,
`soil-desktop-store` persists it over `soil:records`, and `soil-desktops` is the
operable host surface.

v0 has **no module**. It renders the snapshot and emits intents; the host owns
the actor, the namespace write grant and the compare-and-swap. That is the same
split as the `git` Spore: a view that could widen its own grant would be a
second authority. A write-through module is a separate decision, and the narrow
`soil:desktops` capability below is the shape it should use.

This note fixes what the package is, what it may do, and what it must never
decide, so the UI can be written without inventing a second authority.

```text
dev.fungos.desktops
  module.wasm          logic: read, validate, write one manifest at a time
  ui/desktops.lua      Canvas view over a snapshot
  spore.toml           grants: soil:records, soil:principal
  wit/world.wit        import soil:records, soil:principal; export run(args)
```

The module exports `run(args: list<string>)`, the command surface `crm` and
`contacts` already use: the action name followed by one JSON payload. Mush and
the Canvas UI are both clients of that surface; neither gets a second
implementation. The module depends on `desktops-spore` and `soil-desktop` (git
pins to this repository) so the schema ids and validation are not copied.

## What it is for

A person needs one place to:

- create a Team, rename it, add or remove a member, change a role;
- create a Desktop, choose its scope, hand it to a Team;
- add or remove a Desktop member;
- mint a ticket for one file or record to one principal;
- see, and revoke, the tickets a Desktop has issued.

That is the whole management surface. Files, threads, checkouts and service
placement are other surfaces; this Spore does not mount, sync or launch them.

## What it may decide

Nothing authoritative. The Spore may:

- read the manifests the host made visible to it;
- validate an intent against the same `soil-desktop` model the host uses;
- write a manifest with a compare-and-swap on `generation`;
- render a snapshot and explain a refusal.

It may not:

- choose which records book it sees;
- widen a namespace grant;
- add a member, mint a ticket or rewrite a manifest the host has not granted
  write access to;
- treat a ticket as membership;
- see a principal's teams, roles or identity beyond `soil:principal`'s
  projection.

The host resolves namespaces per principal. A Desktop `Owner` gets `teams` and
`desktops` read/write; a `Writer` gets read on both and write on neither. The
Spore cannot tell the difference between "the button is disabled" and "the host
will refuse", and it should not try: it always sends the intent and renders the
refusal.

## Records the Spore reads

The directory book (chosen by the host, never by the Spore) holds:

```text
team:<id>                 soil.desktop/team@1
desktop:<id>              soil.desktop/desktop@1
redemption:<ticket>:<n>   soil.desktop/redemption@1
```

The bodies are the JSON of `soil_desktop::schema`; the schema id is the
contract. A Spore that writes a body the host's decoder rejects has produced a
bug, not a new schema version.

## Snapshot

The UI is a projection, so the module ships one shape and no host paths:

```json
{
  "revision": 12,
  "actor": "oidc:https://issuer#subject",
  "teams": [
    {
      "id": "fpl",
      "name": "FPL",
      "owner": "avery",
      "members": [{ "principal": "kwame", "role": "writer", "expires_at_ms": null }],
      "generation": 3
    }
  ],
  "desktops": [
    {
      "id": "fpl-work",
      "name": "FPL work",
      "scope": "team",
      "owner": { "kind": "team", "team": "fpl" },
      "members": [],
      "tickets": [
        {
          "id": "ticket-d7af…",
          "item": "file:docs/q3.md",
          "grantee": "dana",
          "role": "reader",
          "expires_at_ms": 1700000060000,
          "max_uses": 1
        }
      ],
      "generation": 7
    }
  ]
}
```

`revision` is the sum of the manifests' generations, or the records watermark;
it exists so Canvas can skip an unchanged redraw. It is presentation state, not
authority.

## Actions

Actions are intents. The host validates, writes and returns the new manifest;
the Spore never writes a local copy and pretends it succeeded.

```text
team_create           { id, name }
team_member_set       { team, principal, role?, remove? }
desktop_create        { id, name, scope, team? }
desktop_member_set    { desktop, principal, role?, remove? }
ticket_issue          { desktop, item, principal, role, ttl_secs?, uses? }
ticket_revoke         { desktop, ticket }
```

The action names are the `snake_case` serde tags of `desktops_spore::Intent`;
the payload is one JSON object. The core state machine lives in
[`crates/desktops-spore`](../crates/desktops-spore), like `viewer-spore`: it
turns a snapshot and one intent into one manifest write, with no records handle.
The package only reads, mints a ticket id, calls `apply`, and compares-and-swaps.

Rules the UI must show honestly:

- A generation conflict is **not** a failure to hide. Re-read, show what
  changed, and let the person decide. A last-writer-wins retry silently discards
  somebody else's membership edit.
- A ticket is printed once. The snapshot shows the ticket because the manifest
  holds it, so "copy link" is a convenience, not a secret reveal.
- A refusal has a reason (`not a member`, `not granted`, `expired`,
  `exhausted`, `denied`). Show it; do not collapse every "no" into "error".

## Link and redemption

Proposed link shape:

```text
soil://desktop/<desktop-id>/ticket/<ticket-id>
```

The ticket id is a handle, not a bearer secret: redemption requires the caller's
principal to match `grantee`. A forwarded link is useless to anyone else, which
is the property that makes it safe to paste into a chat.

Redemption is not this Spore's action. A guest does not have write access to the
directory book and should not need it. The host that owns the desktop records
the redemption (today: `soil-desktops redeem`; later: a narrow host capability
that accepts one ticket id and writes one redemption record). Follow-up, not a
silent widening of the Spore's grants.

## Why not a new capability yet

The management path can be `soil:records` because the host already maps
namespaces per principal and the model lives in both the host and the Spore.
A new `soil:desktops` WIT interface would be a second place to encode the same
rules before there is a consumer that cannot use records.

The redemption path is the exception: a guest needs to write one record without
reading or writing the manifest that grants it. That is the capability worth
adding, and it should be narrow:

```text
redeem: func(desktop: string, ticket: string) -> result<grant, error>
```

with no `list`, no `get` and no manifest write.

## Acceptance tests

1. Create a Team, create a Desktop owned by it, add a member, and confirm the
   member sees the Desktop after a reload.
2. A `Writer` opens the Spore: both lists render, every mutating action is
   refused by the host with `not granted`.
3. Two windows edit the same Team; the second save reports a generation
   conflict and re-reads instead of overwriting.
4. Mint a ticket for one file to a non-member. The snapshot shows it; the
   non-member still sees no Desktop.
5. Redeem the ticket from the grantee, then open the same file through access:
   readable. Open a sibling file: refused, and the grantee is still not a
   member.
6. Revoke the ticket and confirm the next access decision refuses.
