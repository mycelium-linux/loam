# Soil ↔ WIT: audit, the one gap, and what is deferred

Status: bindings derive from WIT today and need no per-provider code. Execution
stops at one missing host implementation. Nothing here proposes a new registry,
RPC mechanism or provider-specific extension.

## What already exists

| mechanism | where | what it does |
|---|---|---|
| `witgen` | `soil-lang/src/witgen.rs` | WIT → Soil: `define-record`, type defs, capability metadata, and one `defun` per function calling `(host-call interface function args)` |
| `host-call` | `soil-lang/src/eval.rs` | the single host boundary in the language |
| `Host` trait | `soil-lang/src/eval.rs` | `granted(interface, function)` + `call(interface, function, args) -> Result<Value, String>` |
| `HostChain` | `soil-lang/src/eval.rs` | dispatches to the first host that grants |
| grants | spore manifest | the authorization half, already enforced by loam |

Existing `Host` implementations: `NativeHost`, `ProcessHost`, `SessionHost`,
`SoilRecordsHost`, `SystemTimeHost`, `HostChain`.

## Evidence

```
$ soil-witgen wit --out bindings.soil
wrote fpl:transmog@0.1.0 (8 capabilities, 6 types)

(define-record fpl:transmog/engineering.request
  (source (list u8))
(defun fpl:transmog/engineering.analyze (req process)
  (host-call "fpl:transmog" "analyze" (list req process)))
```

A Soil program calling a generated defun:

```
$ soil run id2.soil
host error: unknown interface `fpl:transmog`
```

That error is the whole gap: naming, resolution and marshalling of the *name*
already work. The host chain simply has no provider host.

## The gap: one `Host` implementation, generic

It belongs in loam, because loam owns the registry and the grants.

```
impl Host for Providers {
    fn granted(&self, interface, function) ->
        registry.get(interface).is_some() && manifest_grants.contains(interface)
    fn call(&mut self, interface, function, args) -> Result<Value, String> {
        // 1. resolve the provider through PluginRegistry (same path as run)
        // 2. instantiate the provider component (reuse plugin.rs)
        // 3. convert Value -> WIT values for the exported function
        // 4. call the export
        // 5. convert WIT results -> Value
    }
}
```

Nothing in it names a provider. `fpl:transmog`, `fpl:kicad` and `fpl:vision`
work through the same implementation or not at all.

## Marshalling the existing providers actually need

| WIT | Soil | note |
|---|---|---|
| `list<u8>` | `(list u8…)` or a bytes value | 361 KB STL: needs an efficient representation, not a 361 000-element list |
| `string` | `String` | |
| `f64`, `u64`, `u32` | `Float`, `Int` | `u32` needs a range check, `u64` may exceed `i64` |
| `bool` | `Bool` | |
| `record` | `(list (pair :field value)…)` | witgen already emits `define-record` for this shape |
| `enum` | `Symbol` or `Keyword` | pick one and enforce it |
| `option<T>` | `nil` or `T` | |
| `result<T, E>` | `(ok T)` / `(err E)` | must not collapse to a single error value |
| `variant` (provider errors) | `(err (pair :code 'engine-unavailable) (pair :message "…"))` | keeps domain outcome and infrastructure cause distinguishable |
| `resource` | not supported | `fpl:kernel`'s `solid` would need a handle table; out of scope this milestone |

The `list<u8>` row is the one real design decision: a naive list is unusable at
mesh sizes. A distinct bytes value (or a host-provided opaque blob handle) is
required before any measurement is meaningful.

## Errors and authorization

- **authorization**: `granted()` reads the spore manifest's grants. A denied
  call must be an explicit error carrying the interface and the missing grant —
  never a silent no-grant that surfaces as a missing host.
- **infrastructure vs domain**: a provider that cannot start, a sidecar that is
  missing, and a DFM verdict of "blocked" are three different outcomes. The
  first two are `err` with an infrastructure code; the third is a successful
  call whose payload says blocked. Conflating them is how `engine-unavailable`
  already hid six distinct defects.

## Not done, and why

Items 7–10 (a real `fpl:transmog` call from Soil, live edit, Soil-exported WIT
consumed as a component, editor metadata) all wait on the host above plus the
`list<u8>` decision. Measuring interpreter and marshalling overhead before that
would measure the error path.

Item 9's remaining gap, stated plainly: Soil can *export* an interface only as
a `(defun …)` over `(host-call …)`. To be substitutable for a compiled
component, a Soil Spore needs to expose its defuns through the component model —
either by compiling to a component whose exports call back into the interpreter,
or by a host-mediated export in loam. Neither exists.

## Deferred, recorded here because `mb` is currently broken

**P0.3 — registry refusal diagnostics.** Scan-time refusals print on every
command invocation. Fix: route them through a `RefusalLog` that prints once per
package per process, gated on `LOAM_DEBUG`, with `loam plugin verify` remaining
the health surface.

**P0.5 — preserve infrastructure causes behind domain errors.** `loam:plugin/ipc`'s
error is a four-variant enum, so a sidecar's reason is discarded. Fix now:
sidecars always reply `payload.error` + `payload.detail`; adapters map to the
domain error and keep `detail` in the report. Later: add `message(string)` to the
IPC error variant when that contract is next revised.
