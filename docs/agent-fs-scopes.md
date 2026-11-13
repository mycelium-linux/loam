# Agent filesystem scopes

Status: **design. One piece is already enforced by the storage layer** (see
"What is already built"), the rest is not written down anywhere.

How a human, a local agent, and an agent in a remote sandbox work from one
shared core namespace without any of them deleting it.

## Decision: one namespace, grants instead of roots

The obvious shape — one VFS root per agent, isolated from every other root — is
what [Mush service accounts](mush-service-accounts.md) already assumes, and it is
wrong for this workload. Agents are supposed to see the *same* core files; the
isolation belongs to what they may change, not to what they may read.

So there is one namespace and two scopes inside it:

| scope | what it is | write | `rm` |
|---|---|---|---|
| `core/` | projects, notes, the shared desktop — the fleet's real files | through a narrow, recorded path (`project attach`, `checkout`) | **denied** by default |
| `worktrees/<agent>/<name>/` | one agent's working copy, derived from core | full | allowed; the whole point is that it is disposable |

A scope is a path prefix plus a grant, not a second VFS root. That keeps a
checkout cheap: copying `core/projects/soil` into a worktree moves no bytes,
because the content is already addressed and the namespace entry is a pointer.

An agent sees a **projection** of the namespace rather than the namespace: the
prefixes it may read, the prefixes it may write, and nothing else. See
[The projection is the firewall](#the-projection-is-the-firewall).

## What is already built

Most of this is not new machinery:

- **Read and write are separate grants.** `soil:vfs` is read-only; `soil:vfs-mut`
is a different package with `write`, `chmod`, `remove`, `mkdir`, `rmdir`,
  `rename`. A Spore that never gets `vfs-mut` cannot mutate anything.
- **`remove` is an unlink, not an erase.** `soil:vfs-mut@0.1.0` says it exactly:
  *"Unlink a namespace entry. Content and history are retained."* In the store,
  `Vfs::remove` deletes the `files` row and queues a `remove` op; the `versions`
  rows and the content-addressed bytes are untouched. Recovery is possible until
  `gc`.
- **A cache node already refuses writes.** `Vfs::remove` returns `ReadOnly` when
  `role == NodeRole::Cache`, and a cache node "may hydrate and evict, but does not
  author writes".

That last one is the load-bearing fact for the remote case: **the shared core on
a sandbox is a cache node, and its worktree is a full node.** The `rm` guard is
then enforced by the storage layer, not by the shell that happens to be running.

## Core is read-mostly, and `rm` is not an agent capability

For an agent attached to core:

- `rm` fails on a prefix the projection gives read-only mode, and it fails as a
  permission error rather than silently escalating.
- Content changes are allowed only through the operations that have a record:
  writing a file (a new version, visible in `history`), or moving it into the
  agent's own worktree.
- Deleting from core is a **human** action. An agent that believes a core file
  must go writes that as a proposed change — a bead, a patch, a message — and the
  human or the landing step performs it.

This is the same rule as `rm` on a Git branch you do not own: the agent's job is
to produce the diff, not to rewrite the trunk.

## Trash is a view, not a place

Because `remove` retains content and history, a trash is presentation plus one
policy decision — where a removed entry is remembered.

The shape:

```text
<trash>/<UTC timestamp>/<original path>   a namespace entry pointing at the
                                          content that was at <original path>
```

- Client and desktop `trash` commands (macOS-style) read this view, and
  `trash restore <path>` re-writes the entry from the newest version of the
  content it holds.
- FUSE `unlink`/`rmdir` in a writable scope route through the same path, so
  Finder and `rm` cannot disagree about what deletion means.
- `trash purge` is the only irreversible step, and it is a **host** action with an
  age gate, not a Spore capability. It is the same clock as `gc`: a trash entry
  that is still referenced by a live path is never purgeable.
- A trash entry is an ordinary namespace entry, so it replicates and shows up in
  `history` like anything else. The reason a delete is recoverable is not a
  special record; it is that the bytes were never unreferenced.

## Escalation, when a core write really is needed

Some workflows legitimately rewrite core: attaching a repository, recording a
worktree, restoring a backup. Those are host operations today (`project attach`,
`worktree record`), and they already carry a record.

For anything else, escalation is a grant, not a flag:

- bound to **one principal** and **one path prefix**;
- **one-shot**: consumed by the operation it authorises, not left standing;
- **recorded** in `soil:records` with who asked, what for, and what changed.

A standing "may write core" grant would be the same mistake as a standing
`sudo` line per agent: it converts every future mistake into an authorised one.

This is the same shape as a share ticket in [Desktops](desktop.md#sharing-one-item):
one item, one named principal, a use budget and an expiry. A ticket grants a
*filesystem item*, never a mount or a scope. Handing somebody a file must not
make the projection wider, and it must not make the storage layer believe the
recipient has a writable prefix.

## The projection is the firewall

Because every filesystem operation goes through `soil:vfs` inside a Spore, the
host does not have to defend the store against whatever the shell runs. It hands
each agent a projection — the only paths that exist for that agent, with a mode
per prefix:

```text
agent A   projects/soil/worktrees/A/**   read+write
          projects/soil/**               read
          everything else                not present

agent B   projects/soil/worktrees/B/**   read+write
          projects/soil/**               read
          everything else                not present
```

Five rules, and each one is the difference between a firewall and a filter:

- **Outside every prefix is `not-found`, never `denied`.** `denied` confirms a
  path exists, which is the disclosure the projection exists to prevent. `denied`
  is then reserved for one case: a path the agent *can* see, in read-only mode —
  the only place where admitting existence is not a leak. Both codes already exist
  in `soil:vfs-mut`'s error enum, so this needs no interface change.
- **`ls` lists only what it may read.** A hidden prefix takes its children with
  it; a listing that names a forbidden directory has already leaked it.
- **Mode is per prefix**, so read-only core and read-write worktrees are one
  mechanism with two values, not two mechanisms.
- **The projection is host state, bound to the socket.** It is not a Spore
  capability, so an installed wasm command cannot widen its own view.
- **Escalation widens one prefix for one operation** and is recorded (below). It
  never widens a socket permanently.

This is why the same-UID caveat stops applying to the filesystem. A sandbox is one
UID, but an agent can only reach a *socket*, and a socket opens onto one
projection. Reaching another agent's socket yields that agent's projection — still
a projection, still not the store, because nothing that holds the store accepts a
raw path from a guest.

What it is not is process isolation. A same-UID process can still reach another
agent's socket and run commands as that principal. That is an argument for one
POSIX account per trust level, not an argument that the projection leaks.

## The remote shape

A boat sandbox holds two roots, and that is the whole isolation story:

```text
desktop (origin)                sandbox
  core/  ──── sync pull ────▶  core/      NodeRole::Cache   read + hydrate only
                               worktrees/<agent>/  NodeRole::Full   read/write
```

- Transport is the existing pull-based sync over `boat-ssh`, already verified end
  to end from a boat sandbox ([fungos-vfs README](https://github.com/FuturePresentLabs/fungos-vfs)).
- `mush --desktop <id>` selects the synced desktop entry, so the sandbox's shell
  opens on the same namespace a human sees locally.
- Worktrees sync *back* by `worktree record` + `project attach`, which is already
  how a worktree crosses machines — the bytes follow the ops.
- Nothing here requires FUSE. FUSE is for human tools and Finder; a Spore uses
  `soil:vfs` directly.

## What operator does

Operator attaches Mush as an HTTP MCP server and calls `mcp__mush__mush_exec`
([Operator → Mush over MCP](mush-operator.md)); bash stays for the host checkout,
compilers, `git` and network. That split is what keeps this design small: mush
owns the namespace, bash owns the machine.

With scopes, the two tools stop being interchangeable in a useful way:

| Task | Tool |
|---|---|
| read or edit a shared file, search, records | `mush_exec` |
| create and work in an isolated copy | `mush_exec` inside `worktrees/<agent>/` |
| `cargo`, `git`, `gh`, a real compiler | bash, against a checkout, as today |
| delete something in core | nobody, by default |

## Not in this slice

- Merge policy between a worktree and core. Worktrees record and land through
  Git today; this document does not invent a VFS merge.
- Per-agent POSIX separation on the remote. Projections keep an agent out of the
  store and out of other agents' paths; they do not stop a same-UID process from
  reaching another agent's socket and acting as that principal. One POSIX account
  per trust level is still the answer for that.
- Multi-writer contention on one core path. `expected-version` on `write` gives
  optimistic concurrency; conflict *resolution* stays `diverged`.

## Open questions

1. **Is trash per scope or global?** A global view is easier for a user and hides
   which scope lost the file. Per-scope is more honest and more clicks.
2. **Does a core `rm` from a human escalate or just work?** Today the design says
   refused for agents and unstated for humans; macOS behaviour would be trash for
   both.
3. **Who owns the trash retention number** — the same 30-day `compact` horizon, or
   its own age gate? Two clocks on one store is how the divergence
   `compact`/pin interaction got subtle.