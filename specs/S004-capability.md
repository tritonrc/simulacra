# S004 — Capability Tokens

**Status:** Active
**Crate:** `simulacra-sandbox`, `simulacra-runtime`

## Behavior

1. Every `AgentCell` has exactly one `CapabilityToken` assigned at creation.
2. The capability token is checked **at the proxy layer** before any side-effecting operation.
3. A denied operation returns an error to the agent, not a silent no-op. The agent sees the denial reason.
4. **Attenuation:** When a parent spawns a child, the child's token must be a subset of the parent's. The supervisor enforces this — it is not convention.
5. A parent with `["net:*.github.com"]` can grant `["net:api.github.com"]` but never `["net:*.stripe.com"]`.
6. A parent with `shell: true` can grant `shell: true` or `shell: false` but a parent with `shell: false` cannot grant `shell: true`.
7. Capability violations are logged to the journal AND emitted as OTel events.

## Token Structure

```rust
struct CapabilityToken {
    network: Vec<NetworkPermission>,   // URL patterns with wildcards
    mcp_tools: Vec<String>,            // "mcp:server:tool" glob patterns
    shell: bool,
    javascript: bool,
    python: bool,
    paths_write: Vec<PathPattern>,
    paths_read: Vec<PathPattern>,
    spawn_types: Vec<String>,          // which agent types this agent can spawn
}
```

## Assertions

- [x] Token with `shell: false` → shell command returns capability error. **Tested in simulacra-sandbox.**
- [x] Token with `net:api.github.com` → fetch to `api.github.com` succeeds. **Tested in simulacra-types capability module.**
- [x] Token with `net:api.github.com` → fetch to `api.stripe.com` returns capability error. **Tested in simulacra-types capability module.**
- [x] Attenuation: child token is validated as subset of parent at spawn time. **Tested via is_subset_of and supervisor spawn.**
- [x] Attenuation: attempt to grant wider capability than parent → spawn error. **Tested in simulacra-types (wildcard/boolean tests) and supervisor.**
- [x] Capability denial writes a `JournalEntry` with the denied operation details. **Tested in agent_loop `capability_denial_is_journaled_with_operation_details`.**
- [x] Capability check for `javascript: false` prevents JS execution and returns error with reason. **Tested in `execute_js_with_javascript_false_surfaces_operation_and_reason_to_agent`.**
- [x] Capability check for `paths_read` / `paths_write` restricts VFS operations. **Implemented in simulacra-sandbox; path checks enforce CapabilityToken patterns on VFS operations.**
- [x] Capability denial returns the denial reason to the agent (not just an error type). **Tested for shell, JS, network, and path operations in simulacra-sandbox: `shell_denial_surfaces_operation_and_reason_to_agent`, `execute_js_with_javascript_false_surfaces_operation_and_reason_to_agent`, `read_file_with_denied_paths_read_surfaces_operation_and_reason_to_agent`, `fetch_http_with_denied_network_capability_surfaces_operation_and_reason_to_agent`.**
- [x] Network capability wildcard matching: `*.example.com` covers `sub.example.com` but not `sub.sub.example.com`. **Tested in `wildcard_network_permission_does_not_cover_multi_level_subdomains`.**
- [x] MCP tool capability check uses glob matching (not just string equality). **Implemented in simulacra-mcp `glob_match`; tested in simulacra-mcp unit tests.**

## Observability (see S010 for conventions)

- [x] Capability denials emit a `WARN`-level event on the current span with `simulacra.capability.operation` and `simulacra.capability.reason`. **Tested in agent_loop o11y tests.**
- [x] `simulacra.capability.denials` counter is incremented on each denial. **Implemented as tracing info event with `simulacra.capability.denials = 1` and `operation` label at every denial point in simulacra-sandbox. Tested in `capability_denials_increment_counter_with_operation_labels_for_each_denial_type`.**
- [x] Capability denials are logged at `WARN` with operation, reason, and agent name. **Tested in `capability_denial_warn_event_includes_agent_name_for_attribution`.**
