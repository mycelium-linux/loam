# Transcripts as CRDT documents in VFS

Status: **design, 2026-07-09.** This note records the target model for agent
transcripts. It does not replace `soil-agent-records` yet; it explains how that
reader and the harness in `~/src/fungOS/soil` fit into a mergeable transcript.

## Why

An agent session is already a sequence of events: user prompts, assistant
messages, thoughts, tool calls, tool results, usage, prompts, and annotations.
Today those events are recorded in a book (`fungos/agents/<session>`) and read
back by `soil-agent-records`. That is a recording, not a document two writers
can merge.

We want the transcript to be a **CRDT document in VFS**:

- another agent or a human can append while offline;
- two devices can exchange ops in any order and converge;
- a comment can be anchored to a message or a span;
- a view can page through the conversation without owning it;
- the harness remains a producer, not the source of truth.

## Shape

```text
Transcript
  id              transcript id (normally the agent session id)
  session         optional Desktop session id
  ops             append-only CRDT operations
  head            derived view / snapshot pointer
  attachments     content ids, never inline bytes
```

One transcript is one logical document. It is not one mutable JSON file.

## Operation

```text
op
  id              content hash of the canonical op
  kind            message | thought | tool-call | tool-result | usage |
                  prompt | edit | delete | comment | reaction
  actor           principal or agent id
  session         transcript id
  lamport         per-actor counter
  parents[]       causal op ids this op saw
  at_ms           wall clock, for display only
  target          optional event id, for edit/delete/comment/reaction
  anchor          optional { event, start, end, quote }
  payload         kind-specific body
```

Rules:

- **Ops are immutable.** An edit is a new op targeting the original; a delete
  is a tombstone op; a comment is a new op. Nothing rewrites history.
- **Merge is union.** Duplicate ids dedupe. Missing parents are held until they
  arrive; the visible transcript is the causal closure.
- **Display order** is causal, then `(lamport, actor, id)`. Wall clocks never
  decide order across devices.
- **Message bodies** use a multi-value register or a last-writer-wins register
  keyed by `(lamport, actor)`. The view must be able to show an unresolved
  conflict instead of silently dropping one.
- **Tool calls/results** are paired by stable call id, not by position.
- **Attachments** are content ids with display metadata. Media bytes stay in
  the content store.

## Anchors and comments

A comment is an op:

```text
kind: comment
target: event-id
anchor: { event: event-id, start: 12, end: 28, quote: "the failing test" }
body: "This is the retry path, not the initial call."
```

The event id is the stable anchor. The text range is a hint. If the event body
is edited, the view tries the range, then the quote, then marks the anchor
`stale`. A stale anchor stays visible; it does not silently attach to a
different sentence.

Comments and reactions are transcript ops, so they merge with the transcript
instead of living in a separate mutable side table.

## VFS layout

The exact layout is still open, but it must satisfy the VFS model:

```text
sessions/<session>/transcripts/
  ops/
    <actor>/<lamport>-<op-id>.json
  snapshots/
    <generation>.json
  head.json                 derived pointer/cache, not authority
  comments/                 optional materialized view, not authority
```

- Ops are content-addressed VFS entries.
- `head.json` is a cache of the latest known snapshot/generation. Losing it is
  safe; it is rebuilt by scanning ops.
- Snapshots are derived, immutable, and content-addressed. They are for faster
  opening, not for merge.
- A reader with a stale snapshot can still merge new ops.
- Records can index ops (`kind`, `session`, `actor`, `target`) for search, but
  the op log remains the source of truth.

## View and comments

A Soil surface or shell view reads the CRDT, not a private database:

- page the conversation by causal order;
- show tool calls and results as one unit;
- show comments anchored to the event/span they name;
- let an authorized actor append a comment or reaction;
- show unresolved edit conflicts and stale anchors explicitly.

The existing `soil-agent-records` reader becomes an adapter:

```text
recorded book  -> transcript ops -> CRDT view
```

It should not become a second source of truth. The harness in
`~/src/fungOS/soil` (`soil-agent-records`, `soil-agentsd`,
`soil-conversation`, `soil-serve`) is the first producer to bridge.

## Non-goals

- No inline media bytes in a transcript body.
- No central mutable document that two writers race on.
- No wall-clock ordering across devices.
- No silent anchor migration after an edit.
- No requirement that a live harness be present to read a transcript.
