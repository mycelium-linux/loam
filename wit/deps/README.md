# Vendored WIT dependencies

These packages are upstream, not loam contracts. They are vendored so a
contract in this tree resolves as one WIT graph.

| Package | Why it is here |
|---|---|
| `wasi:webgpu@0.3.0-rc.2` | dependency of the `soil:surface` surface contract |

Keep the vendored package byte-identical to the upstream release named in the
path. A host that consumes a different version updates the copy and the contract
in the same change.
