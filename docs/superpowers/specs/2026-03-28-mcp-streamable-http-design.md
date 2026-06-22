# S024 — MCP Streamable HTTP Transport

**Status:** Active
**Crates involved:** `simulacra-mcp`, `simulacra-config`

## Dependencies

- **S008** — MCP integration (capability gating, journal, tool bridging, OTel — unchanged)
- **S010** — Observability conventions

## Scope

Replace the MCP transport layer with the 2025-03-26 streamable HTTP protocol. Auto-detect server capability with fallback to legacy SSE. Public API unchanged.

**In scope:**
- Streamable HTTP handshake (POST-based, single endpoint)
- Session management via `Mcp-Session-Id` header
- Content-type negotiation (`application/json` vs `text/event-stream`)
- Auto-detect with legacy SSE fallback
- SSE response stream parsing (buffer progress, return final result)
- `transport` config field becomes optional

**Out of scope:**
- GET stream for server-initiated messages (future — enum accommodates it)
- JSON-RPC batching
- Resumability / `Last-Event-ID`
- Session termination via DELETE
- Changes to capability gating, journal, or OTel semantics (S008)

Full spec: `specs/S024-mcp-streamable-http.md`

## Design

### TransportMode enum

`McpConnection` replaces its flat `sse_handle` and `post_endpoint` fields with a discriminated enum:

```rust
enum TransportMode {
    /// 2025-03-26 streamable HTTP — single endpoint, optional session ID
    StreamableHttp {
        session_id: Option<String>,
    },
    /// 2024-11-05 legacy SSE — discovered POST endpoint, persistent stream
    LegacySse {
        post_endpoint: String,
        sse_handle: JoinHandle<()>,
    },
}
```

The mode is determined during handshake and fixed for the connection's lifetime. Reconnection re-handshakes with the same mode — servers don't change protocol versions between retries.

### McpConnection changes

```rust
struct McpConnection {
    server_name: String,
    url: String,
    tools: Vec<McpToolSchema>,
    handshake_done: bool,
    was_connected: bool,
    transport_mode: Option<TransportMode>,  // None before handshake
}
```

The `sse_handle: Option<JoinHandle<()>>` and `post_endpoint: Option<String>` fields are removed — they live inside `TransportMode::LegacySse`.

### Auto-detection flow

```
connect(url) called (lazy — no I/O)
    │
    ▼
first tool access triggers ensure_connected()
    │
    ├─ transport config = "sse"  ──────► legacy SSE handshake
    ├─ transport config = "http" ──────► streamable HTTP handshake
    └─ transport config absent   ──────► try streamable HTTP
                                             │
                                    ┌────────┴────────┐
                                    │ success          │ 404/405
                                    ▼                  ▼
                              StreamableHttp     try legacy SSE
                                                       │
                                              ┌────────┴────────┐
                                              │ success          │ fail
                                              ▼                  ▼
                                          LegacySse     ConnectionFailed
```

### Streamable HTTP handshake

1. POST `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}` to configured URL
   - Headers: `Content-Type: application/json`, `Accept: application/json, text/event-stream`
   - Protocol version: `2025-03-26`
2. Parse response — extract `Mcp-Session-Id` header if present
3. POST `{"jsonrpc":"2.0","method":"notifications/initialized"}` (with session ID header)
4. POST `{"jsonrpc":"2.0","id":2,"method":"tools/list"}` (with session ID header)
5. Parse tools, set `TransportMode::StreamableHttp { session_id }`

### Tool call dispatch

**StreamableHttp:**
- POST `tools/call` with `Accept: application/json, text/event-stream` + `Mcp-Session-Id`
- Branch on response `Content-Type`:
  - `application/json` → parse as JSON-RPC, return result
  - `text/event-stream` → buffer events, log progress via `tracing::debug!`, extract final response
  - 60s idle timeout on SSE stream

**LegacySse:**
- POST `tools/call` to discovered `post_endpoint` (existing S008 path, unchanged)

### Session expiry

- POST returns 404 + session ID present → session expired
- Clear session ID and `handshake_done`
- Immediate re-handshake (no backoff — this is protocol-level)
- Retry original request once
- If retry fails → return error

Distinct from reconnection (transport failures, exponential backoff).

### Reconnection

- Existing S008 logic unchanged
- Reconnection stays in current transport mode
- Session expiry is NOT reconnection — no backoff
- Transport failures get backoff: 1s, 2s, 4s — 3 attempts

### Config changes

`McpServerConfig.transport` becomes optional:

- Absent → auto-detect
- `"sse"` → legacy SSE only
- `"http"` → streamable HTTP only

Existing configs work unchanged.

### Observability

New span attributes:
- `simulacra.mcp.transport_mode`: `"streamable_http"` or `"legacy_sse"`
- `simulacra.mcp.protocol_version`: `"2025-03-26"` or `"2024-11-05"`
- `simulacra.mcp.session_id`: session ID if present
- `simulacra.mcp.response_type`: `"json"` or `"sse_stream"` on tool call spans

New counter:
- `simulacra.mcp.session_expired` with `server` label

Tracing events:
- `info!` on auto-detect fallback
- `debug!` per SSE progress notification
- `info!` on session expiry + re-handshake

All existing S008 observability (tool call spans, counters, journal) unchanged.
