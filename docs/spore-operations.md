# Declarative Spore operations

A Spore can declare one typed operation contract and project it to both the CLI
and MCP. The contract is `[[operations]]` in `spore.toml` (and the equivalent
form for `spore.soil` is still to be added).

```toml
[[operations]]
name = "marbles.ready"          # stable public name
summary = "List ready issues"   # CLI help and MCP description
input = "marbles/ready@1"       # schema id, not a free-form argument blob
output = "marbles/issues@1"     # optional
effect = "read"                 # read | write | external
risk = "low"                    # low | medium | high
requires = ["soil:records"]     # must already be in [grants]
resource = "records:marbles"    # bounded selector, never a host path
expose = ["cli", "mcp"]         # the two projections
idempotent = true
command = "mb"                  # a declared [[commands]] implementation
action = "marbles.ready"        # a declared [[actions]] implementation
```

## Rules

- **One contract, two projections.** `command` is the CLI implementation and
  `action` is the MCP/action-bus implementation. They share name, summary,
  input schema, effect, risk, capabilities and resource selector.
- **No self-grant.** Every `requires` entry must already appear in `[grants]`.
  Declaring an operation cannot widen authority.
- **No host paths.** `resource` is a bounded selector. Absolute paths, `..`
  and whitespace are refused.
- **Fail closed.** `expose = ["cli"]` requires a declared command; `expose =
  ["mcp"]` requires a declared action. A missing implementation is a manifest
  error, not a half-exported tool.
- **Stable names.** Operation names are unique. The name is the public identity;
  command/action names are implementation references.
- **Labels are not authority.** `effect` and `risk` drive discovery and policy
  display; the host still decides approval from the grant and the action.

## What is implemented

- `Operation`, `OperationEffect`, `OperationRisk` in
  `crates/loam/src/spore/mod.rs`.
- Parsing/validation of `[[operations]]` with the rules above.
- Six tests in `operation_manifest_tests`: CLI+MCP projection, self-grant
  refusal, host-path resource refusal, MCP-without-action, CLI-without-command,
  duplicate names.

## What is not implemented yet

- Registry binding of an operation to an installed revision/digest and explicit
  activation. `InstallRecord` already carries `package_digest`, `current`,
  `composition_key` and `dev`; the operation projection must be gated on the
  active signed generation, not a manifest path.
- Collision/version selection across installed Spores. Command names are unique
  within one package; cross-package precedence and version pinning need a
  registry rule.
- CLI dispatch from `operation.name` (the command registry still dispatches
  `[[commands]]`).
- MCP projection through the existing Mycelium gateway. `loam-mcp` and
  `soil:mcp` exist, but the operation catalog is not yet wired into their
  tool listing.
- `spore.soil` parsing for `operations`; Marbles is a source Spore and needs
  this parser plus an action/tool implementation before it can be the first real
  dual-surface Spore. Do not add a fake action just to claim coverage.
