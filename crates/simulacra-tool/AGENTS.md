# simulacra-tool

Registry for tools that can be offered to an LLM and invoked when the
model returns a tool-use response.

## Key types

- **ToolRegistry** — holds registered `Tool` implementations, provides
  `definitions()` for the provider and `call()` for execution.

## Capability checking

Before calling a tool the registry verifies the `CapabilityToken` allows
it. Currently this is a pass-through; real checks will be added once
the capability model is finalized.

## Dependencies

- `simulacra-types` — `Tool` trait, `ToolDefinition`, `ToolError`, `CapabilityToken`
- `serde_json` — tool argument values

## How to test

```bash
cargo test -p simulacra-tool
cargo clippy -p simulacra-tool -- -D warnings
```
