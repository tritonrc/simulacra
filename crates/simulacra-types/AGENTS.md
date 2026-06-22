# simulacra-types

Core types and traits — the leaf crate of the dependency graph.

## Responsibilities
- Define all cross-boundary types: Message, ToolDefinition, ToolCall, ToolResult, TokenUsage
- Define all protocol traits: Provider, Tool, ContextStrategy, VirtualFs, JournalStorage
- Define capability and budget types: CapabilityToken, ResourceBudget

## Dependencies
- External only: serde, serde_json, schemars, rust_decimal, thiserror, tokio
- Zero internal dependencies

## Testing
```bash
cargo test -p simulacra-types
```
