# soil-ui-proto

A working vertical slice of the Lisp → HTML/CSS UI path described in
[`docs/soil-lisp-roadmap.md`](../../docs/soil-lisp-roadmap.md). It is a
**prototype host**, not the final `soil:ui` implementation.

The point is to answer one question with running code:

> If Lisp emits UI forms, stylesheets, and state transitions, what is left for
> Rust/WIT, and what is left for JavaScript?

## Run it

```sh
cargo run -p soil-ui-proto
```

Open <http://127.0.0.1:7878/>.

The left side is the Soil app. The right side is the `.soil` source. Edit it and
press **Apply** (or `Cmd/Ctrl + Enter`); the server re-evaluates the file and the
page patches the DOM in place.

Options:

```sh
cargo run -p soil-ui-proto -- --source path/to/view.soil --port 9000
cargo run -p soil-ui-proto -- --render --state 5 --action inc
```

`--render` prints the same JSON the browser receives. It is the cheap way to
check a form without starting a server.

## The contract

A Soil UI file exports four functions:

```lisp
(initial-state)          ; -> value
(update state action)    ; -> value
(view state)             ; -> UI form
(styles)                 ; -> CSS text
```

A UI form is a record tree:

```lisp
((:tag "div")
 (:attrs ((:class "app")))
 (:children (((:text "hello")))))
```

The host encodes that as JSON:

```json
{
  "tag": "div",
  "attrs": { "class": "app" },
  "children": [{ "text": "hello" }]
}
```

Rules the prototype already relies on:

- **Lisp never touches the DOM.** It returns values.
- **Attributes are keyword/value pairs.** They become HTML attributes.
- **Text is a `{text}` node.** It cannot be injected as markup.
- **CSS is one string per render.** The host owns the `<style>` element.
- **Actions are opaque names.** `update` decides what they mean.

`counter.soil` is the reference app: a counter with increment/decrement/reset,
real CSS, and a source panel that re-evaluates the file.

## What this experiment says belongs where

| Layer | Prototype | Final home |
|---|---|---|
| App markup, CSS, state transitions | `counter.soil` | Soil spore |
| Reader, evaluator, value encoding | `soil-lang` | Rust core + eventual Soil bootstrap |
| Calling `view` / `update` / `styles` | `soil-ui-proto` | `tend run` + `soil:ui` host |
| Turning forms into DOM, patching, CSS injection | `web/ui.js` | JS implementation of `soil:ui` (WIT) |
| Event routing, focus, measurement | `web/ui.js` + `data-action` | `soil:ui` contract |
| Dev server, source reload, HTTP | `soil-ui-proto` | `tend` dev command only |

The stable boundary is **forms in, events out**. Everything else — DOM nodes,
CSSOM, the reconciler, style caching, event delegation — is host implementation.
That is why the same Lisp should run against a second `soil:ui` host (native or
a GPU scene) without changing `counter.soil`.

### Shape of the WIT this implies

The prototype does not define WIT yet, but the call shape is already visible:

```wit
interface ui {
  mount: func(form: string, styles: string) -> surface-id;
  patch: func(surface: surface-id, form: string, styles: string) -> result<_, string>;
  unmount: func(surface: surface-id);
  event: func(surface: surface-id, name: string, payload: string);
}
```

The guest emits `form` and `styles`; the host owns `surface-id`, input, and
layout. `event` is how a click becomes `action = "inc"`; the guest's `update`
turns that into a new state and a new form. A real contract should use the
canonical S-expression encoding (`soil:sexp`) rather than a JSON string, and
should carry a source span so errors point at the form that failed.

## What is deliberately missing

This is a first slice, not the roadmap's Phase 4:

- No signal graph. State is one integer and one `update` call per action.
- No keyed-list demo yet; the reconciler supports `data-key` but nothing uses it.
- No grants, capabilities, or sandbox. The dev server can write the source file.
- No Wasm guest. The Lisp runs in the host process.
- No native `soil:ui` implementation. Browser only.
- No source spans or comment preservation in the emitted form.

Each of those is a reason to keep the contract narrow now: the host can grow
without touching the `.soil` files.

## Tests

```sh
cargo test -p soil-ui-proto
```

The tests pin the four-function contract, the state transitions, and the fact
that `(view 42)` actually contains `42`.
