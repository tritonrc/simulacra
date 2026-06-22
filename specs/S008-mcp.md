# S008 — MCP Integration

**Status:** Active
**Crate:** `simulacra-mcp`

## Behavior

1. MCP servers are accessed via HTTP/SSE only. No stdio. No child processes.
2. `McpManager` maintains connections to configured MCP servers.
3. Tool schemas from MCP servers are bridged to Simulacra's `ToolDefinition` format.
4. Tool calls to MCP servers go through the capability proxy (checked against `CapabilityToken`).
5. MCP server connections are lazy — established on first tool call, not at startup.
6. Connection failures produce typed errors, not panics.
7. SSE transport discovers the POST endpoint from server-sent `event: endpoint` events.
8. SSE transport sends `notifications/initialized` after receiving `initialize` response.
9. SSE transport maintains a persistent keepalive connection for server-pushed events.
10. If a previously-connected server becomes unreachable during a tool call, reconnection is attempted with exponential backoff (1s, 2s, 4s — 3 attempts max). After max retries, the transport error is returned.
11. Reconnection is not attempted for servers that have never successfully connected.

## Assertions

- [x] No use of `std::process::Command` or `tokio::process::Command` in simulacra-mcp. **Behavioral test in `simulacra_mcp_connects_via_http_not_child_process` — verifies MCP connections use HTTP transport; no stdio/spawn API exists in McpManager's public interface. Architectural constraint enforced by code review.**
- [x] Tool schema bridging produces valid `ToolDefinition`. **Behavioral test in `list_tools_bridges_name_description_and_input_schema_from_mcp_server` — starts a real HTTP MCP server, calls list_tools, asserts name/description/inputSchema are bridged.**
- [x] Capability check happens before MCP tool call. **Behavioral test in `call_tool_with_tool_outside_capability_returns_capability_denied` — attempts a call with insufficient capabilities, asserts CapabilityDenied.**
- [x] Glob capability pattern allows matching tools. **Behavioral test in `call_tool_with_glob_mcp_capability_pattern_is_allowed_to_dispatch` — uses `mcp:*:*` pattern, asserts tool call succeeds.**
- [x] Connection failures produce typed errors. **Behavioral tests in `invalid_mcp_url_returns_typed_error` and `call_tool_to_unconnected_server_returns_typed_error`.**
- [x] Lazy connection: no HTTP request on McpManager construction or connect call. **Behavioral test in `connect_http_is_lazy_and_does_not_open_a_socket_during_connect` — binds a port, connects, verifies no accept before first use.**
- [x] `call_tool` with a tool not in `mcp_tools` capability returns `CapabilityDenied`. **Behavioral test in `call_tool_with_tool_outside_capability_returns_capability_denied`.**
- [x] `call_tool` to a server that is not connected returns a typed error. **Behavioral test in `call_tool_to_unconnected_server_returns_typed_error`.**
- [x] Actual MCP protocol handshake (initialize, tools/list) is implemented. **Behavioral test in `connect_http_performs_initialize_then_tools_list_handshake` — starts a fake MCP server, verifies JSON-RPC initialize and tools/list sequence.**
- [x] SSE transport discovers POST endpoint from server-sent events. **Behavioral test in `connect_sse_discovers_post_endpoint_from_sse_events` — starts a fake SSE server, verifies endpoint URL is parsed from `event: endpoint` / `data: /path`.**
- [x] SSE transport performs handshake via discovered endpoint. **Behavioral test in `connect_sse_performs_handshake_via_discovered_endpoint` — verifies initialize and tools/list go through the discovered POST URL.**
- [x] Tool calls via SSE transport use the discovered endpoint. **Behavioral test in `call_tool_via_sse_transport_uses_discovered_endpoint` — dispatches a tool call, verifies it hits the SSE-discovered POST URL.**
- [x] SSE transport maintains a persistent connection for server-pushed events. **Behavioral test in `connect_sse_keeps_connection_alive` — verifies the SSE connection stays open after handshake.**
- [x] MCP tool call writes a journal entry before returning. **Behavioral test in `call_tool_records_a_tool_call_journal_entry` — calls a tool, asserts journal contains the entry.**
- [x] Transient transport failure on a previously-connected server triggers automatic reconnection with exponential backoff. **Behavioral test in `reconnect_after_transient_failure_succeeds_on_retry` — takes a server down, brings it back, verifies the call succeeds after retry.**
- [x] After 3 reconnection failures, the transport error propagates to the caller. **Behavioral test in `reconnect_exhausts_retries_and_returns_error` — takes a server down permanently, verifies error after max retries.**
- [x] No reconnection is attempted for servers that never successfully connected. **Behavioral test in `no_reconnect_for_never_connected_server` — connects to an unreachable server, verifies fast failure without retry backoff.**

## Observability (see S010 for conventions)

- [x] MCP tool calls produce a span with `gen_ai.operation.name` = `execute_tool`, `simulacra.tool.name`, and `simulacra.tool.source` = `mcp:{server}`. **Behavioral test in `mcp_tool_calls_emit_execute_tool_span_with_mcp_source_attributes` — captures tracing spans and asserts attributes.**
- [x] `simulacra.mcp.calls` counter is incremented per call with `server` and `tool` labels. **Behavioral test in `mcp_tool_calls_increment_counter_with_server_and_tool_labels` — captures tracing events and asserts counter fields.**
- [x] MCP connection failures are logged at `WARN` with server name and error. **Behavioral test in `mcp_connection_failures_are_logged_at_warn_with_server_and_error` — triggers a connection failure and captures the WARN event.**
- [x] `gen_ai.tool.message` events are emitted with tool input/output. **Behavioral test in `mcp_tool_calls_emit_gen_ai_tool_message_events_for_input_and_output` — captures tracing events and asserts input/output fields.**
