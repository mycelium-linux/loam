# Soil structural constrained decoding

Status: **design + implementation prompt, not implemented.** This maps the
auto-close/skeleton idea onto the existing reader and the UMIE constrained
decoding seam. It does not claim token savings until Layer A/B/C below are
measured.

## Implementation prompt (paste this into the Phase 5 task)

> Add optional structural actions to Soil constrained decoding. A model may
> open a form without emitting its closer, fill skeleton holes, move between
> holes, and close one or many frames with a single action. The renderer owns
> the delimiters; the model owns only atoms, strings, and hole choices. Keep a
> structural stack so every rendered prefix plus deferred closers is a valid
> Soil form. Mask illegal actions before sampling, reject illegal external
> actions loudly, preserve comments/spans, and canonicalize through the same
> reader used by the editor. Keep the human editor and the model decoder on the
> same `Action` enum. Use the same path for JSON/XML closers when a spore opts
> in. Do not require retraining for the first safe slice; gate the
> control-token layer behind a model capability flag.

## Why the naive version does not save what it looks like

A text model emits tokens; an editor can move a cursor. If a model emits `(`
and the runtime knows the matching `)` will be needed, that does **not** by
itself save a token: the model still has to signal when the form is finished.
Two distinct mechanisms are often mixed together:

1. **Forced closer elision (no retraining).** When the grammar says only a
   closer is legal, the decoder appends it without a model forward pass. This
   saves decode steps and latency, but if the response is billed or counted on
   rendered tokens, the closer is still in the text.
2. **Structural actions (real sampled-token savings).** The model chooses an
   action such as `CloseTo(depth)` or `Commit`. One action can close several
   frames, or all of them, replacing many closer tokens. Skeletons replace
   boilerplate keywords and delimiters with one action.

So implement both, measure `sampled_tokens` and `rendered_tokens` separately,
and do not report them as the same number.

## Three layers

### Layer A — forced closer elision

- `StructuralStack` knows the required closer for every open frame.
- When `legal_actions()` has no non-closing action, the runtime appends the
  closer directly and repeats until a choice exists.
- This is a safe optimization for today's byte models. It changes no grammar,
  no tokenizer, and no prompt.
- Savings: decode steps and wall time, not necessarily rendered text or billing.

### Layer B — control actions (requires a capable/trained model)

Add a small action vocabulary above the byte grammar:

```text
Open(kind)          ; open a form, do not emit its closer
Skeleton(kind)      ; emit a template with named holes
Atom(bytes)         ; emit an atom/symbol/keyword
String(bytes)       ; emit a quoted string (escaped by the renderer)
Hole(index)         ; move to the next unfilled hole
CloseCurrent        ; close one frame
CloseTo(depth)      ; close all frames down to depth
Commit              ; close all frames and finish
Comment(bytes)      ; attach trivia to the current frame
```

The action tokens may be special tokens in the tokenizer or a prefix-mapped
subset of existing tokens. Do not add them to a base model that was not trained
to select them; a control token the model never chooses is not a feature.

### Layer C — primed skeletons (works before Layer B)

For current models, the host primes the prompt with the skeleton and lets the
model continue at the first hole:

```text
<|im_start|>assistant
```soil
(def |
```

The model fills `|`; the renderer supplies the rest at `Commit`. This is the
same idea as `--prime-fence` in UMIE. It costs no new control vocabulary and
already removes template tokens from the sampled output.

## Grammar fragments

Structural legality is a function of the stack, not a flat regex:

```text
Action      := Open(kind) | Skeleton(kind) | Atom | String | Hole
             | CloseCurrent | CloseTo(depth) | Commit | Comment

Open(kind)  := legal when kind may appear in the current hole/position
Skeleton(k) := legal when k's required position matches the current hole
Hole        := legal only when the current frame has an unfilled hole
CloseCurrent:= legal when stack.depth > 0
CloseTo(d)  := legal when 0 <= d < stack.depth
Commit      := legal when every required hole is filled and the root closes
Atom/String := legal when the current position accepts a value
```

The byte grammar still runs underneath. An action expands to bytes, then the
existing `Lisp`/`Json` recognizer must accept those bytes. This preserves the
current safety property: the final rendered text parses through the normal
reader.

## Structural stack invariant

```text
rendered_prefix + deferred_closers(stack)  is a valid prefix of a Soil form
```

Each frame carries:

- `kind` (list, vector, string, comment, skeleton instance),
- `closer` (`)`, `]`, `"`, etc.),
- `holes` and which are filled,
- `explicit_close` (did the model choose a close action?),
- `span_start`/`span_end` for the reader.

`Commit` drains the stack and emits closers in reverse order. `CloseTo(depth)`
does the same down to `depth`, then leaves the cursor after the closed forms.
No action may leave the stack in a state whose deferred suffix is not a valid
prefix.

## Skeleton rules

Skeletons are registered assets, not hard-coded magic in the decoder. Start
with the forms Soil actually uses most:

```text
def        (def <name> <args> <body>)
fn         (fn <args> <body>)
let        (let <bindings> <body>)
if         (if <test> <then> <else>)
signal     (signal <name> <initial>)
ui         (ui <layout>)
panel      (panel <title> <children>...)
row/col    (row <children>...) / (col <children>...)
defspore   (defspore <name> <metadata> <body>)
```

Each rule declares required and optional holes. The decoder offers only
skeletons whose position matches the current hole. The model fills the holes;
the renderer supplies the fixed keywords and closing delimiters. Unknown or
malformed skeletons fail at load time with a loud error, never at generation
time.

## Worked example: function definition

Naive sampled text:

```text
(defn square (x) (* x x))
```

The model samples `(`, `defn`, `square`, `(`, `x`, `)`, `(`, `*`, `x`, `x`,
`)`, `)` — including every closer.

Layer B:

```text
Skeleton(defn)        -> renders "(defn "
Atom(square)          -> "square"
Hole(0)               -> enters the argument list
Atom(x)               -> "x"
Hole(1)               -> enters the body
Atom(*), Atom(x), Atom(x)
Commit                -> closes args and body
```

The model emits no `(`, `)`, `defn`, or closing-paren tokens. The final text is
canonicalized by the reader and compared to the structural tree before it is
committed.

## Worked example: UI panel

Naive:

```lisp
(ui (panel :title "Status"
      (col (text "CPU") (signal cpu))
      (col (text "Disk") (signal disk))))
```

Layer B/C outline:

```text
Skeleton(ui)                 -> "(ui "
Skeleton(panel)              -> "(panel "
Atom(:title), String(Status)
Skeleton(col)                -> resources
  Skeleton(text), String(CPU)
  Skeleton(signal), Atom(cpu)
Skeleton(col)
  Skeleton(text), String(Disk)
  Skeleton(signal), Atom(disk)
Commit
```

The model chooses the values and which children exist. It does not spend tokens
on `ui`, `panel`, `col`, `text`, `signal` boilerplate or delimiters.

## Repair and validation

- Illegal sampled action: do not apply it. Return a typed error with the
  current path and expected actions; if the caller allows retry, rewind to the
  last committed frame/hole and mask the illegal action on the retry.
- Illegal bytes inside `Atom`/`String`: the underlying reader rejects them; the
  action is not committed.
- Comments and spans: store comments as trivia attached to the frame/hole they
  precede; keep byte spans in the original rendered order. Canonical printing
  may normalize whitespace, but it must not lose comment content or relative
  order without an explicit `canonicalized` flag.
- Final validation: render, read with the normal Soil reader, compare the
  structural tree (and optionally a CID) to the action tree. Mismatch is a
  hard failure, never a silent auto-repair.
- No semantic guessing: the decoder may insert delimiters and template
  keywords; it may not invent missing expressions.

## Savings bounds

Track separately:

- `sampled_tokens` — tokens chosen by the model.
- `rendered_tokens` — tokens in the final text.
- `decode_steps` — forward passes, including forced suffixes.

Measured with `umie-bench structural-savings` and Ornith-1.5-9B's tokenizer
(2026-11-21):

| Corpus | Naive | Auto-close, 1 action/run | Zero-cost-close bound | + Skeletons |
|---|---:|---:|---:|---:|
| `jmc.lisp` | 738 | 0.7% saved | 14.1% | 0.7% |
| `bel.bel` (15.4k tokens) | 15,400 | 0.2% saved | 8.2% | 1.0% |
| 100 Soil UI panels | 6,200 | **−1.6%** (worse) | 8.1% | 3.2% |
| 100 JSON objects | 3,303 | **−3.0%** (worse) | 3.1% | n/a |

The earlier 8–15% auto-close estimate was wrong for byte-level BPE. BPE already
compresses closers; charging one action token per close run can increase sampled
tokens. The zero-cost-close column is the optimistic bound if closing needs no
token at all.

The real wins are fewer malformed forms, fewer repair loops, and forced closers
without forward passes. Skeleton expansion is the only consistent sampled-token
win measured so far: ~1% on `bel.bel`, 3.2% on repetitive UI forms. Measure a
real Soil corpus before claiming more than that.

## Integration points

### Soil reader/editor

- Owns `Action`, `StructuralStack`, skeleton registry, reader, spans, comments,
  canonical printer, and final validation.
- Exposes the same operations to the human editor and the model decoder.
- Skeleton assets are signed/validated like other Soil assets; a bad skeleton is
  a load error.

### UMIE decoder

- Reuse the existing `Constraint`/`RegionGate`/`ConstrainedSelector` seam.
- Add a `SoilStructural` constraint that wraps `Lisp` (and a JSON variant).
- Add an action layer above token selection:
  - Layer A: forced suffix insertion inside the generation loop.
  - Layer B: mask action tokens by structural legality.
  - Layer C: prompt priming; no new tokenizer surface.
- Cache identity must include grammar, skeleton registry digest, action
  vocabulary, and model capability flag. A skeleton change changes semantics.

### WIT / host boundary

- No WIT change for Layer A or C.
- Layer B needs either a small action enum or opaque tagged action bytes at the
  inference boundary. Keep tokenizer ids and logits in the host; keep reader
  legality and rendering in Soil.
- The host owns the model/tokenizer/KV/mask machinery. Soil owns what a legal
  Soil action is. Do not put tokenizer details in the guest, and do not put
  reader semantics in the host.

## Acceptance criteria

1. Rendered output from every successful structural run parses under the normal
   Soil reader.
2. `Commit` closes all frames and the canonical text equals the action tree.
3. `CloseTo(depth)` closes exactly the requested frames; no extra closers leak.
4. Illegal action is never applied; retry masks it and reports the path.
5. Comments and spans survive a round trip; canonicalization is explicit.
6. Editor and decoder exercise the same action enum and skeleton registry.
7. Measurements report sampled tokens, rendered tokens, and decode steps.
8. Base-model forced elision works without new tokens; Layer B is opt-in by
   model capability.
9. JSON/XML opt-in uses the same stack and action machinery, not a fork.

## Deliberate non-goals

- No general Lisp/Python/Rust parser in the decoder.
- No semantic completion or synthesis of missing expressions.
- No silent repair of a model's malformed action stream.
- No claim that rendered text is shorter than naive text; the win is sampled
  tokens and decode steps unless the provider bills rendered tokens.
