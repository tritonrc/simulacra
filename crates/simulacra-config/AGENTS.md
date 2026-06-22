# simulacra-config

Configuration loading and types for Simulacra projects.

## Responsibility

Deserialize `simulacra.toml` files into typed Rust structs. This crate owns the
schema for project metadata, agent type definitions, MCP server lists,
capability grants, and task configuration.

## Key Types

- `SimulacraConfig` -- top-level config, entry point via `from_file()`
- `AgentTypeConfig` -- per-agent-type settings (model, capabilities, limits)
- `CapabilitiesConfig` -- granted capabilities (network, mcp, shell, etc.)
- `McpServerConfig` -- MCP transport endpoints
- `TaskConfig` -- what to run and how

## Constraints

- No runtime logic; this crate is pure data + deserialization.
- All structs derive `Serialize, Deserialize, Debug, Clone`.
- Errors use `ConfigError` (thiserror) -- no panics on bad input.
