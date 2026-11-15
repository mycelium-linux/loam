# Agents

Status: contract. `wit/soil-agents.wit` is reviewed; the runner attaches the
fixture cast and any ACP agent (`--agents demo|acp|all`), which covers Operator
and `delta-acp` today. The dispatcher (bus -> edgerunner session) is the next
slice (see "Adapters" below).

## The one concept

Every harness has its own words: Operator has turns, tools, a `finish_status`
terminal and an ACP server; Pi has sessions, steps and an RPC/JSON mode; a
Claude Code plugin sees hooks and permissions. Those are transport and protocol
differences, not product differences. A display that shows what an agent is
doing should not have to be written once per harness.

`soil:agents` is that single concept: **one normalized agent**, and the set of them a surface may see — identity, phase,
goal, activity, prompts, usage — plus the three things a person may do to it
(answer, send, interrupt). An **adapter** translates one harness into it. Views
are written against the contract and never learn which harness is underneath.

Two interfaces, so the authority splits with the grant:

| Interface | Needs | Functions |
|---|---|---|
| `observe` | `soil:agents.observe` | `agents`, `get`, `intent`, `transcript`, `subscribe` |
| `control` | `soil:agents.control` | `respond`, `send`, `interrupt` |

A read-only wall imports `observe` alone. A window that can answer imports both.

## Facts, not moods

The contract carries what happened, not how it feels. There is no `energy`,
`mood`, or `hunger` field, because those are opinions about a session, and two
views that disagree about an opinion produce two different creatures for the
same agent. A view that wants a creature derives vitals from the facts it
already has:

| Vital | Derived from |
|---|---|
| energy | `usage.context-tokens / usage.context-window` |
| health | the status of recent `activity` |
| hunger | `prompts` (how much it needs you) |
| growth | `usage.turns` |

The same rule keeps the *look* out of the contract: a skin is a view, and the
next skin is not an ABI change.

## The one invariant

`phase = needs-you` **if and only if** `prompts` is non-empty.

Every way an agent can need a person funnels into one record, so a surface
renders one card shape instead of one per harness:

| Harness event | `prompt-kind` |
|---|---|
| permission request (a hook, a sandbox gate) | `approval` |
| `finish_status(status = decision)` | `choice` |
| `finish_status(status = blocked)` | `question` |
| an interactive question tool | `question` or `choice` |

An adapter whose harness has no answer channel still satisfies the invariant. It
synthesizes a prompt from the terminal report and implements `respond` by sending
the person's words back through the channel that harness does have — for Operator
that is the next `session/prompt`. Answering is idempotent: the prompt id makes a
second answer `stale` rather than a second action.

## Adapters

An adapter is a host concern, for three reasons:

1. **No ambient authority.** A guest cannot open a socket or spawn a process, and
   it should not be given one to watch an agent.
2. **One classifier.** Deciding that `grep -rn` is Files and `cargo build` is
   Shell belongs in one place per harness, not in every view.
3. **The harness protocol is not a UI contract.** ACP versions and Pi's RPC
   frames move; the WIT does not.

`crates/soil-agent-host` holds the seam: a plain `Agent` record, an
`AgentBackend` trait, the fixture cast, and `CompositeAgents`, which merges
several adapters behind one roster (stable ids, needs-you first, control routed
to the owning adapter). `crates/soil/src/acp.rs` is the ACP client, and
`crates/soil/src/webgpu` maps these records to the generated WIT types because
hosts do not depend on generated types (`CatalogHost` and `TerminalHost` follow
the same rule).

| Backend | Speaks | State it needs |
|---|---|---|
| `FixtureAgents` | a scripted cast | none; answers advance the script |
| `AcpBackend` | ACP JSON-RPC (stdio today) | a command, or an edgerunner `service_url` later |
| `delta-acp` | ACP agent over the Delta CLI | the `delta-acp` binary |
| Pi | its JSON/RPC mode | the process or endpoint, and a session id |

Operator's `session/status` already carries the facts this contract wants:
`running`, `context { tokens, window }`, `turn`, `finish`, `usage`, `model`,
`workspace`. `finalize`/`finish` maps directly onto `phase`, and
`finish_status(blocked|decision)` is the synthesized prompt. Pi's JSON events
carry steps and usage; its adapter classifies steps into the same activity
stations.

## Bounded by the host

Nothing here is an unbounded pipe:

- `activity` and `prompts` are short lists; an adapter keeps the last N.
- `transcript` is paged with a cursor and a host-capped `limit`.
- `details` on a prompt is preformatted evidence (a command, a diff, a failing
  test), capped like a tool preview, never a credential.
- `label` is a name, never content: two path segments, a host, a tool name.
- `workspace` may be redacted by host policy.

## Noticing a new session

An adapter that enumerates sessions (`AcpBackend` lists through `session/list`)
should not poll its harness on every tick. A session that appears somewhere else
reaches it as a bus notice instead. Three kinds carry the loop:

| Kind | Published by | Body |
|---|---|---|
| `agent.task.v1` | whoever asks for work (a surface, a person) | `{text, harness, environment, title?}` |
| `agent.session.v1` | the dispatcher, once a session exists | `{id, harness, title, workspace?, model?}` |
| `agent.task.failed.v1` | the dispatcher, when creation fails | `{cause, harness?, title?}` |

`text` is required; `harness` defaults to `operator` and `environment` to
`local`. The failure `cause` is the `soil:agents/types.error` name
(`unsupported` for a host with no dispatcher, `io` for a fleet that refused),
so a reader needs no second vocabulary.

The host owns the loop, because it owns the bus and the transport. Each tick
that a guest already makes — `agents`, `get`, `transcript`, `subscribe` — reads
the two notice kinds before answering. A task notice becomes a session through
the dispatcher, the created session is announced, and the adapter re-lists once.
A notice is acted on **once**, identified by `(kind, producer, issued-at, body)`:
an in-process bus replays its retained window on every read, and a replayed task
must not become a second session.

A re-list **merges**, it does not replace. The list owns `title`, `workspace`,
`model` and existence; the stream owns activity, prompts, usage and the
transcript. So a session created through `session/new` that streams before it is
listable is not mistaken for a removal, and a rename does not wipe a question
that is still waiting for a person.

### Runbook

```sh
# A task becomes a session, and the session is announced.
printf 'agent.task.v1\t{"text":"fix the flaky test","harness":"operator","environment":"local"}\n' > task.txt
soil-webgpu --frame out.bgr --edgerunner http://127.0.0.1:7777 --bus-seed task.txt colony.wasm
```

The request is `POST /v1/sessions` with `{prompt, agent, runner, auto_start}`:
the task's `harness` is edgerunner's `agent`, its `environment` is the `runner`,
and edgerunner decides whether that is a runner it has. Without `--edgerunner`
the host still watches the bus and answers `agent.task.failed.v1` with
`cause: unsupported`, which is a fact a person can act on instead of an empty
wall.

## Not in this contract

These are separate authorities, and a surface that answers questions should not
have them:

- **Spawn.** Starting a session is a process, and process authority is not a UI
  grant. The host dispatches instead: a Spore publishes `agent.task.v1` and the
  host's dispatcher asks edgerunner for the session (see
  [Noticing a new session](#noticing-a-new-session)).
- **Configure.** Choosing a model, editing policies, or trusting a workspace is
  a different decision with a different lifetime.
- **Mutate history.** The transcript is an observation. Fork, rewind, and edits
  go through the harness's own session surface.

## The first surface

[`spores/colony`](https://github.com/mycelium-linux/spores/tree/main/spores/colony)
is the proving Spore: one window per agent, a board of every agent, and a focus
view with the transcript and a composer. It imports `observe` and `control` and
knows nothing about Operator or Pi beyond the `harness` string it prints.

Where colony sits in the wider operator stack — the thread store it still needs,
the composer, mush, the VFS and the build plane — is
[the operator-UI map](operator-ui.md).

## Open questions

- Whether `respond` should carry a deadline or the host should expire prompts on
  its own clock. Today the harness withdraws a prompt; the surface never guesses.
- Whether one surface may hold two bindings (a split view) or a host opens two
  surfaces. Leases are one-per-Spore today, so this is a host question.
- Whether `usage` should carry cost. It is provider- and plan-dependent, and a
  wrong cost is worse than no cost.
