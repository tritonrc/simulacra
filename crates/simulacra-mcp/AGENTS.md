# simulacra-mcp

Manages connections to external MCP (Model Context Protocol) servers
and exposes their tools as `ToolDefinition` values.

## Key types

- **McpManager** — connect to servers via SSE or HTTP, list tools.
- **McpError** — error enum for connection/protocol failures.

## Constraints

Per R002 this crate MUST NOT use `std::process::Command` or
`tokio::process`. All communication happens over network transports.

## Dependencies

- `simulacra-types` — `ToolDefinition`
- `reqwest` — HTTP/SSE transport
- `thiserror` — error derivation

## How to test

```bash
cargo test -p simulacra-mcp
cargo clippy -p simulacra-mcp -- -D warnings
```

`connect_sse` and `connect_http` are currently `todo!()` stubs.
