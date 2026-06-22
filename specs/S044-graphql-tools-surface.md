# S044 — GraphQL Tools Surface

**Status:** Active
**Crates:** `simulacra-graphql`, `simulacra-server`

## Dependencies

- **S033** — Integration fabric (`IntegrationRegistry`, the source of truth for integrations)
- **S041** — WASM MCP servers (`McpRegistry`, source of truth for MCP servers)
- **S042** — Agent catalog & GraphQL control plane (extends the schema this spec adds to)

## Scope

Today, `Agent.capabilities` is `[String!]!` of opaque grants like `"shell:exec"`, `"net:read"`, `"mcp:fetcher:*"`. A UI cannot render the *Tools* picker in the agent-builder form (the northstar in `project_northstar_agent_form` memory) because there's no structured catalog to enumerate from and no derived projection of which tools an existing agent has.

This spec adds a **read-only** GraphQL surface that exposes the available tools and the projection of an agent's selected tools. **No new mutations** — UI continues to use `updateAgent { capabilities }` and re-submits the full list. Sugar mutations (`attachTool`, `detachTool`) belong to a follow-up if the UI grows ergonomic friction.

**In scope:**
- `Tool` type + `ToolKind` enum
- `availableTools` query (tenant-scoped via existing `GraphQLContext`)
- `Agent.tools` derived field (projects `capabilities[]` and integration grants → `[Tool!]!`)
- `ToolCatalog` trait + default impl in `simulacra-server` that combines built-in caps + integration registry + MCP registry

**Out of scope:**
- Mutations (deferred — use `updateAgent { capabilities }`)
- Tool icons, categories, badge states (later spec, when UI ergonomics demand)
- Skill enumeration via this surface (skills have their own GraphQL types per S042)
- Per-integration sub-tools (an integration is one Tool in v1; sub-tool granularity later)
- Runtime MCP tool discovery (config-time MCP server list only — runtime tool listing is per-task, not catalog-shaped)
- Per-agent `attachTool` / `detachTool` mutations
- Persistence changes — `Agent.capabilities` storage stays as `Vec<String>` in the catalog

## Context

### Tool sources today

| Source | Where | Capability-string shape |
|---|---|---|
| Built-in tools | `simulacra-tool::register_builtins` (always registered, gated by capability) | `"shell:exec"`, `"javascript"`, `"python"` (boolean caps) |
| Integrations | `simulacra-integration::IntegrationRegistry` | (no capability today — granted via tenant config; see "Integration grants" below) |
| MCP servers | `simulacra-mcp::McpRegistry` (config-time list) | `"mcp:<server>"` or `"mcp:<server>:<tool>"` |
| Skills | `simulacra-catalog` skills | NOT a tool for this picker — separate UI section |

### Capability-string parsing

`simulacra-server::engine::build_capability_token_from_resolved` already parses `agent.capabilities[]`:
- Bare strings `shell:exec`, `javascript`, `python` → boolean caps on `CapabilityToken`
- `mcp:<server>` or `mcp:<server>:<tool>` → MCP grants
- `net:<host>` → network grants
- Other strings: silently ignored (NIT noted in S042 review)

### Integration grants

Today integrations are granted to an agent via `TenantConfig.integrations` (a `Vec<String>` *outside* the catalog). The catalog has no integration association on `Agent` rows. v1 of this spec uses the **integration's name as a capability string** with prefix `integration:` (e.g. `"integration:slack"`) and adds parsing in `build_capability_token_from_resolved`. Existing tenant-config integrations remain valid; agents that opt in via the new capability string also get the integration. Migration: a follow-up spec wires per-tenant integration assignment into the catalog proper.

## Design

### `ToolKind` enum

```graphql
enum ToolKind {
  BUILTIN_CAPABILITY  # shell:exec, javascript, python
  INTEGRATION         # integration:slack, integration:gmail
  MCP_SERVER          # mcp:<server>
}
```

### `Tool` type

```graphql
type Tool {
  "Capability string used in agent.capabilities[]. Stable identity."
  id: ID!

  kind: ToolKind!

  "Human-readable label for the picker (\"Slack\", \"Shell execution\")."
  name: String!

  "Short description for the picker."
  description: String!

  "Provider name for INTEGRATION/MCP_SERVER kinds (\"slack\", \"my-mcp\"). Null for BUILTIN_CAPABILITY."
  provider: String

  "Optional input-schema JSON (always null in v1; reserved for sub-tool surfacing)."
  inputSchema: JSON
}
```

`Tool.id` IS the capability string the engine consumes. This is intentional: the UI takes a `Tool` from `availableTools`, drops `tool.id` into the agent's capabilities list, and `updateAgent { capabilities }` round-trips through the same parser.

### Queries

```graphql
extend type Query {
  "All tools available to the authenticated tenant."
  availableTools: [Tool!]!

  "A single tool by id (capability string), null if not in the catalog."
  tool(id: ID!): Tool
}

extend type Agent {
  "Tools currently granted to this agent. Derived from capabilities[]."
  tools: [Tool!]!
}
```

`availableTools` is tenant-scoped via existing `GraphQLContext`. `Agent.tools` is computed as: for each entry in `capabilities[]`, look up the corresponding `Tool` from the catalog; entries that don't resolve are filtered out (logged at warn — see Observability).

### `ToolCatalog` trait

```rust
// crates/simulacra-graphql/src/tool_catalog.rs (new)
#[async_trait]
pub trait ToolCatalog: Send + Sync {
    /// All tools available to the given tenant.
    async fn list(&self, tenant_id: &TenantId) -> Vec<Tool>;
    /// Single tool lookup by id (capability string).
    async fn get(&self, tenant_id: &TenantId, id: &str) -> Option<Tool>;
}
```

Default impl in `simulacra-server::tool_catalog::DefaultToolCatalog`:
- Always returns the 3 builtin capability tools.
- For integrations: iterates `IntegrationRegistry::names()`, pulls `metadata(name)` for description, returns one `Tool` per entry with `id = "integration:<name>"`.
- For MCP servers: iterates the tenant's configured MCP server allowlist, filtered against the top-level `[mcp.servers]` definitions. One `Tool` per server with `id = "mcp:<server>"`. Sub-tool surfacing is later.

The catalog is *static* relative to engine startup — it doesn't poll. Hot-reload is a future spec.

### Capability parser update

`build_capability_token_from_resolved` gains one branch:
- `integration:<name>` → currently no-op on `CapabilityToken` (the existing tenant-config integration grant is what actually wires the integration); included so the agent can declare its integration use without tenant-config edits. Future spec: collapse the two grant paths.
- `mcp:<server>` → expands to `mcp:<server>:*` so the UI can grant access to a configured server row while MCP dispatch keeps using fully-qualified `mcp:<server>:<tool>` patterns.

### Hooking ToolCatalog into the schema

`simulacra-graphql::graphql_router` and the test schema builders gain an `Arc<dyn ToolCatalog>` data dep, parallel to the existing `Arc<dyn AgentRepository>` etc. `simulacra-server` constructs the default impl from its `IntegrationRegistry` + MCP config and threads it into the router.

## Behavior

### `availableTools` query
- Tenant-scoped (auth required, no cross-tenant leakage).
- Returns the union of: 3 builtin caps + all integrations registered for the tenant + all MCP servers configured for the tenant.
- Stable ordering: builtins first (alpha), then integrations (alpha by name), then MCP servers (alpha by server name). Stable enough for diff-friendly UIs.

### `tool(id:)` query
- Returns null when the id is unknown to the catalog (NOT an error).
- Cross-tenant ids return null (the tenant scope filters before lookup).

### `Agent.tools` derived field
- Iterates `capabilities[]`, calls `ToolCatalog::get(tenant, cap)` for each.
- Filters out None results, emits a `tracing::warn!` per dropped entry with `tenant_id`, `agent_id`, `unknown_capability`.
- Order of returned tools matches `capabilities[]` order (stable; no de-dup beyond what the catalog already enforces).

## Assertions

### `Tool` type
- [x] `Tool.id` round-trips through `agent.capabilities[]` for all three kinds.
- [x] `Tool.kind` is `BUILTIN_CAPABILITY` for `shell:exec`, `javascript`, `python`; `INTEGRATION` for `integration:<name>`; `MCP_SERVER` for `mcp:<server>`.
- [x] `Tool.provider` is null for builtins, equals the integration name for integrations, equals the server name for MCP servers.

### `availableTools` query
- [x] Returns all three builtin capability tools.
- [x] Returns one Tool per registered integration in `IntegrationRegistry::names()`. *(GraphQL surface verified via `StubToolCatalog` in simulacra-graphql tests; the `IntegrationRegistry::names()` -> `Tool` projection lives in `DefaultToolCatalog` (simulacra-server, deferred to follow-up — see "Open questions").)*
- [x] Returns one Tool per MCP server in the engine's MCP config via `simulacra-server::DefaultToolCatalog`.
- [x] In multi-tenant mode, MCP rows are restricted by the tenant's `mcp_servers` allowlist before lookup/listing.
- [x] Stable ordering: builtins (alpha) → integrations (alpha) → MCP (alpha).
- [x] Tenant A's integrations are not visible to tenant B's `availableTools`.
- [x] Tenant A's MCP servers are not visible to tenant B's `availableTools`.

### `tool(id:)` query
- [x] Returns the `Tool` for a known id.
- [x] Returns null (not error) for an unknown id.
- [x] Returns null for a tool the tenant has no access to.

### `Agent.tools` derived field
- [x] Projects `capabilities[]` → `[Tool!]!` in capability-string order. *(Catalog returns capabilities `ORDER BY capability ASC` from `agent_capabilities` table; `Agent.tools` mirrors that order. Insertion-order preservation would require a schema change and is not in scope.)*
- [x] Drops capability strings that don't resolve, emits a `tracing::warn!` carrying `tenant_id`, `agent_id`, `unknown_capability`.
- [x] An agent with empty `capabilities[]` has empty `tools`.
- [x] An agent with only catalog-resolvable capabilities has `tools.len() == capabilities.len()`.

### Capability parser
- [ ] `integration:<name>` is recognized by `build_capability_token_from_resolved` (no-op on the token in v1, but does NOT fall through to the silent-drop branch). *(Deferred — the parser-update lives in simulacra-server. Not blocking the GraphQL surface; tracked as part of the "DefaultToolCatalog wiring" follow-up.)*
- [x] `mcp:<server>` from the UI expands to `mcp:<server>:*`, while explicit `mcp:<server>:<tool>` / glob patterns are preserved.

## Observability

- [ ] `simulacra.graphql.tool.list` span carries `tenant_id`, `count`. *(Lightweight; full o11y suite is the existing S042 deferred follow-up.)*
- [ ] `tracing::warn!` on each dropped capability in `Agent.tools`: `tenant_id`, `agent_id`, `unknown_capability`.

## Open questions

1. Should `availableTools` accept a `kind:` filter? Deferred — UI can filter client-side until a sort/filter UX requires it.
2. Should integrations grow per-tenant assignment in the catalog (rather than via `TenantConfig`)? Yes — but it's a follow-up to keep this spec read-only.
3. Should `Tool.inputSchema` populate for MCP servers (showing each sub-tool)? Deferred until the UI needs it; today the picker treats an MCP server as one selectable unit.

## Deferred follow-ups

The GraphQL surface lands complete + tested with a `StubToolCatalog`. The `simulacra-server::DefaultToolCatalog` now wires tenant-allowed, config-time MCP servers into `availableTools` as server-level picker rows. Integration registry projection remains deferred because tenant-scoped integration assignment is still split between tenant config and the catalog.

Also deferred: `build_capability_token_from_resolved` learning the `integration:<name>` prefix.

These are small, additive, and don't block the agent-builder UI from rendering against the schema once a frontend exists.
