# S024 — MCP Streamable HTTP Transport

**Status:** Active
**Crate:** `simulacra-mcp`

## Dependencies

- **S008** — MCP integration (capability gating, journal entries, tool schema bridging, OTel — all unchanged)
- **S010** — Observability conventions

## Scope

Replace the MCP transport layer with the 2025-03-26 streamable HTTP protocol. Auto-detect server capability with fallback to legacy SSE. Public API unchanged.

**In scope:**
- Streamable HTTP handshake (POST-based, single endpoint, protocol version `2025-03-26`)
- Session management via `Mcp-Session-Id` header
- Content-type negotiation on tool call responses (`application/json` vs `text/event-stream`)
- Auto-detect: try streamable HTTP first, fall back to legacy SSE on 4xx
- SSE response stream parsing (buffer progress notifications, extract final JSON-RPC result)
- `transport` config field becomes optional (omit for auto-detect)

**Out of scope:**
- GET stream for server-initiated messages (future — `TransportMode` enum accommodates it)
- JSON-RPC batching (one request per POST)
- Resumability / `Last-Event-ID`
- Session termination via DELETE (connection drop suffices)
- Changes to capability gating, journal, or OTel semantics (S008 governs those)

## Context

S008 implemented MCP with two transport modes: plain HTTP (request/response) and SSE (endpoint discovery via server-sent events, persistent connection). The MCP specification revision 2025-03-26 replaces both with a single "streamable HTTP" transport that uses one endpoint for all communication: POST for client-to-server messages, with the server choosing between JSON and SSE for responses.

The key improvement is simplicity: one endpoint instead of two, explicit session management via headers instead of implicit connection-based sessions, and SSE is optional (simple servers just return JSON).

Simulacra adopts this with auto-detection: try the new protocol first, fall back to legacy SSE for older servers. Callers of `McpManager` never know which transport is in use.

## Design

### Transport mode enum

`McpConnection` replaces its flat `sse_handle` and `post_endpoint` fields with:

```rust
enum TransportMode {
    /// 2025-03-26 streamable HTTP — single endpoint, session ID
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

The enum is set during handshake and does not change for the lifetime of the connection (reconnection re-handshakes with the same mode).

### Auto-detection handshake

On first tool access (lazy, per S008):

1. **Try streamable HTTP** — POST `InitializeRequest` to configured URL with `Accept: application/json, text/event-stream`.
   - Success (2xx with valid JSON-RPC response) → store `Mcp-Session-Id` from response headers, send `notifications/initialized`, send `tools/list`, mark `TransportMode::StreamableHttp`.
   - 404 or 405 → proceed to step 2 (server doesn't support streamable HTTP).
   - Other 4xx (401, 403) → `McpError::ConnectionFailed` (auth/permission issue, not a transport mismatch — don't fall back).
   - 5xx or network error → `McpError::ConnectionFailed` (server issue, not a transport mismatch).

2. **Fall back to legacy SSE** — GET configured URL with `Accept: text/event-stream`, parse `event: endpoint` / `data: /path`, handshake via discovered POST endpoint, mark `TransportMode::LegacySse`.

3. **Both fail** → `McpError::ConnectionFailed` with context about what was tried.

### Tool call dispatch

On `call_tool`:

**StreamableHttp mode:**
- POST `tools/call` to server URL with `Accept: application/json, text/event-stream` and `Mcp-Session-Id` (if present).
- Read response `Content-Type`:
  - `application/json` → parse JSON-RPC response, return result.
  - `text/event-stream` → buffer SSE events. Progress notifications are logged via `tracing::debug!`. Final JSON-RPC response is extracted and returned. Stream closes without response → `McpError::ProtocolError`.

**LegacySse mode:**
- POST `tools/call` to `post_endpoint` (existing S008 behavior, unchanged).

### Session expiry

If any POST in StreamableHttp mode returns **404** and a `session_id` is present:
- Clear `session_id` and `handshake_done`.
- Re-run streamable HTTP handshake (no backoff — this is protocol-level, not transport-level).
- Retry the original request once.
- If retry fails → return the error.

This is distinct from reconnection (which handles transport failures with exponential backoff).

### Reconnection

Existing S008 reconnection logic applies unchanged, but is transport-mode-aware:

- Reconnection re-handshakes using the current `TransportMode` (streamable HTTP retries as streamable HTTP, legacy SSE retries as legacy SSE).
- Transport mode does not change during reconnection. A server doesn't change protocol versions between retries.
- Backoff timing: 1s, 2s, 4s — 3 attempts max. Only for previously-connected servers.

### Config changes

`McpServerConfig.transport` becomes optional:

```toml
# Auto-detect (recommended):
[[mcp.servers]]
name = "github"
url = "https://mcp.github.com/mcp"

# Explicit auto-detect (equivalent to omitting transport):
[[mcp.servers]]
name = "github-auto"
transport = "auto"
url = "https://mcp.github.com/mcp"

# Force legacy SSE:
[[mcp.servers]]
name = "legacy"
transport = "sse"
url = "https://old-server.example.com/sse"

# Force streamable HTTP (no fallback):
[[mcp.servers]]
name = "new-server"
transport = "http"
url = "https://new-server.example.com/mcp"
```

- `transport` absent → auto-detect (try streamable HTTP, fall back to legacy SSE)
- `transport = "auto"` → auto-detect (same behavior as absent)
- `transport = "sse"` → legacy SSE only, skip auto-detect
- `transport = "http"` → streamable HTTP only, no fallback

### Timeouts

- Handshake requests: 5s (unchanged)
- Tool call requests: 30s (unchanged)
- SSE stream idle timeout: 60s (new — no events for 60s during a streaming response → transport error)

## Behavior

### Auto-detection

1. When `transport` config is absent, the first handshake attempt uses streamable HTTP (POST `InitializeRequest`).
2. If the streamable HTTP handshake receives HTTP 404 or 405, the system falls back to legacy SSE handshake. Other 4xx (401, 403) and 5xx are not fallback-eligible — they indicate auth/server issues, not transport mismatch.
3. If `transport = "sse"`, only legacy SSE handshake is attempted.
4. If `transport = "http"`, only streamable HTTP handshake is attempted (no fallback).
5. If both streamable HTTP and legacy SSE fail during auto-detect, `McpError::ConnectionFailed` is returned with details of both attempts.

### Streamable HTTP handshake

6. The `InitializeRequest` POST includes `Accept: application/json, text/event-stream` header.
7. The protocol version in `InitializeRequest` is `2025-03-26`.
8. If the server returns `Mcp-Session-Id` in the response headers, it is stored on the connection.
9. All subsequent requests to that server include `Mcp-Session-Id` header (if stored).
10. After successful `InitializeResult`, `notifications/initialized` is sent as a POST.
11. After initialized notification, `tools/list` is sent as a POST.

### Session management

12. If a POST returns HTTP 404 and a session ID is present, the session is considered expired.
13. On session expiry, the session ID and handshake state are cleared.
14. A fresh streamable HTTP handshake is attempted immediately (no backoff).
15. The original request is retried once after successful re-handshake.
16. If re-handshake or retry fails, the error is returned to the caller.

### Tool call responses

17. Tool call POST includes `Accept: application/json, text/event-stream`.
18. If the response `Content-Type` is `application/json`, it is parsed as a single JSON-RPC response.
19. If the response `Content-Type` is `text/event-stream`, SSE events are buffered.
20. Progress notifications in SSE streams are logged via `tracing::debug!` but not surfaced to callers.
21. The final JSON-RPC response is extracted from the SSE stream and returned.
22. If the SSE stream closes without delivering a JSON-RPC response, `McpError::ProtocolError` is returned.
23. If no SSE events arrive for 60 seconds during a streaming response, it is treated as a transport error.

### Reconnection

24. Reconnection uses the connection's current transport mode (does not switch modes).
25. Reconnection re-runs the full handshake for the current mode.
26. Session expiry (404) triggers immediate re-handshake, not exponential backoff.
27. Exponential backoff (1s, 2s, 4s) applies to transport-level failures only.

### Config

28. `McpServerConfig.transport` is optional. Omitting it enables auto-detection.
29. `transport = "auto"` enables auto-detection with the same behavior as omitting the field.
30. Existing configs with `transport = "sse"` continue to work without changes.
31. `transport = "http"` forces streamable HTTP with no legacy fallback.

### What doesn't change

32. `McpManager` public API (`connect`, `list_tools`, `call_tool`) is unchanged.
33. Capability gating (S008 assertions 3-4) is unchanged.
34. Journal entries (S008 assertion 14) are unchanged.
35. Tool schema bridging (S008 assertion 2) is unchanged.
36. Lazy connection semantics (S008 assertion 6) are unchanged.
37. Typed error handling (S008 assertions 5-8) is unchanged.
38. OTel counters and histograms for tool calls (S008 observability) are unchanged.

## Assertions

### Auto-detection

- [x] Auto-detect tries streamable HTTP POST first when `transport` is absent.
- [x] Auto-detect falls back to legacy SSE when streamable HTTP returns 404 or 405.
- [x] Auto-detect does NOT fall back on 401, 403, or 5xx — these are `ConnectionFailed` errors.
- [x] `transport = "sse"` skips auto-detect and uses legacy SSE directly.
- [x] `transport = "http"` skips auto-detect and uses streamable HTTP directly (no fallback).
- [x] Both transports failing during auto-detect returns `McpError::ConnectionFailed` with details.

### Streamable HTTP handshake

- [x] `InitializeRequest` POST includes `Accept: application/json, text/event-stream`.
- [x] Protocol version in `InitializeRequest` is `2025-03-26`.
- [x] `Mcp-Session-Id` from response headers is stored on the connection.
- [x] All subsequent requests include stored `Mcp-Session-Id` header.
- [x] `notifications/initialized` is sent after successful `InitializeResult`.
- [x] `tools/list` is sent after initialized notification.

### Session management

- [x] HTTP 404 with a stored session ID triggers session expiry handling.
- [x] Session expiry clears session ID and handshake state.
- [x] Re-handshake after session expiry has no backoff delay.
- [x] Original request is retried once after successful re-handshake.
- [x] Failed re-handshake returns error to caller.

### Tool call responses

- [x] Tool call POST includes `Accept: application/json, text/event-stream`.
- [x] `application/json` response is parsed as JSON-RPC response.
- [x] `text/event-stream` response is parsed as SSE, progress logged, final result extracted.
- [x] SSE stream closing without a JSON-RPC response returns `McpError::ProtocolError`.
- [x] 60s idle timeout on SSE stream triggers transport error.

### Reconnection

- [x] Reconnection uses current transport mode, does not switch.
- [x] Reconnection re-runs full handshake for current mode.
- [x] Session expiry triggers immediate re-handshake (no backoff).
- [x] Transport failures use exponential backoff (existing S008 behavior).

### Config

- [x] `transport` field is optional in `McpServerConfig`.
- [x] Omitting `transport` enables auto-detection.
- [x] `transport = "auto"` enables auto-detection with the same behavior as omitting the field.
- [x] `transport = "sse"` forces legacy SSE.
- [x] `transport = "http"` forces streamable HTTP.
- [x] Existing S008 configs with `transport = "sse"` work unchanged.

## Observability (see S010)

- [x] `simulacra_mcp_handshake` span includes `simulacra.mcp.transport_mode` (`"streamable_http"` or `"legacy_sse"`).
- [x] `simulacra_mcp_handshake` span includes `simulacra.mcp.protocol_version` (`"2025-03-26"` or `"2024-11-05"`).
- [x] `simulacra_mcp_handshake` span includes `simulacra.mcp.session_id` (if present).
- [x] `simulacra_mcp_tool_call` span includes `simulacra.mcp.response_type` (`"json"` or `"sse_stream"`).
- [x] `simulacra.mcp.session_expired` counter incremented on session expiry, with `server` label.
- [x] `tracing::info!` emitted when auto-detect falls back from streamable HTTP to legacy SSE.
- [x] `tracing::debug!` emitted for each progress notification in SSE streaming responses.
- [x] `tracing::info!` emitted when session expiry is detected and re-handshake begins.
