# S031 — Simulacra API Server

**Status:** Active
**Crates:** `simulacra-server` (new), `simulacra-types`, `simulacra-runtime`, `simulacra-config`

## Dependencies

- **S006** — Resource budgets (tenant budget pools, per-task metering)
- **S009** — Agent supervisor (task lifecycle, concurrent agent management)
- **S010** — Observability conventions
- **S012** — Built-in tools (tool registry integration)
- **S026** — Governance hooks (hook pipeline for API-triggered tasks)
- **S030** — Agent payments (budget pool enforcement, payment flow)

## Scope

Native API server wrapping the Simulacra engine with WebSocket bidirectional transport, REST+SSE fallback, protocol adapters (A2A, AG-UI), OAuth2/OIDC + API key auth, namespace-based multi-tenancy, and agent-consumable endpoints.

**In scope:**
- New `simulacra-server` crate — HTTP server (axum), WebSocket transport, REST+SSE fallback
- Native API protocol: client commands + server events over WebSocket or REST+SSE
- `ProtocolAdapter` trait — pluggable protocol translation (A2A, AG-UI)
- `AuthProvider` trait — `OidcAuthProvider` (OAuth2/OIDC), `ApiKeyAuthProvider`
- `TenantResolver` — maps authenticated identity to `TenantConfig` (namespace, agent type, VFS root, hooks, budget)
- `SimulacraEngine` wrapper — bridges API layer to existing engine crates
- `TaskManager` — task lifecycle (create, cancel, pause, resume), multiple concurrent tasks per connection
- Agent lifecycle state machine: pending → running → streaming/waiting_input/waiting_approval/paused → completed/failed/killed/cancelled
- Event stream: task state, agent messages, tool calls/results, approvals, artifacts, payments, hooks, budget warnings, errors
- `[server]` and `[tenants.*]` config sections in `simulacra.toml`
- API-as-MCP-server: every endpoint discoverable as MCP tools
- SaaS + managed BYOC deployment modes

**Out of scope:**
- Specific protocol adapter implementations (A2A, AG-UI — follow-up specs define behavior)
- Slack/Teams bot integration (those are protocol adapters that consume the API)
- Admin dashboard UI
- Workflow hardening / curator agent
- Agent personality config (uses existing VFS + system prompt mechanisms)
- MicroVM / fork isolation backends (deployment choice, not API concern)
- WebSocket reconnection / session resume (future spec)

## Context

Simulacra today runs as a CLI. The API server is the first step toward making Simulacra a multi-user, multi-tenant platform. The API is the product surface — every interaction flows through it. A human in Slack, a cron job, a webhook from Salesforce, and another agent all create tasks through the same API.

The native API uses WebSocket for bidirectional streaming. REST+SSE is a fallback for environments that can't maintain WebSocket connections. Protocol adapters translate between the native protocol and external standards (Google's A2A for agent-as-service, AG-UI for chat UI embedding).

Multi-tenancy is namespace-based: each tenant gets an isolated VFS root, budget pool, hook configuration, and agent type. A single Simulacra instance serves accounting agents and customer success agents with completely separate configurations. The tenant is resolved from the authenticated identity (OIDC claim, API key metadata).

The API is agent-consumable by design. Every endpoint returns tool-friendly JSON. The entire API can be exposed as an MCP server, so agents can manage other agents, query task status, and interact with the platform programmatically.

## Design

### Transport layers

```
┌─────────────────────────────────────────────┐
│  Protocol Adapters (A2A, AG-UI)             │
│  Translate external protocols → native API  │
├─────────────────────────────────────────────┤
│  Native API                                 │
│  WebSocket (primary) · REST+SSE (fallback)  │
├─────────────────────────────────────────────┤
│  Auth Layer                                 │
│  OIDC · API Keys · Tenant Resolution        │
├─────────────────────────────────────────────┤
│  TaskManager + SimulacraEngine                  │
│  Task lifecycle · Agent execution · Events  │
└─────────────────────────────────────────────┘
```

WebSocket connections carry both client commands and server events on a single channel. REST+SSE uses POST for commands and a per-task SSE stream for events. The `TaskManager` doesn't know which transport delivered the command.

### Agent lifecycle

```
                    ┌──────────┐
                    │ pending  │
                    └────┬─────┘
                         │ task.create
                         ▼
                    ┌──────────┐
              ┌─────│ running  │─────┐
              │     └────┬─────┘     │
              │          │           │
              ▼          ▼           ▼
        ┌───────────┐ ┌──────────────────┐ ┌────────┐
        │ streaming │ │ waiting_input    │ │ paused │
        └───────────┘ │ waiting_approval │ └────────┘
                      └──────────────────┘
              │          │           │
              │          │           │
              ▼          ▼           ▼
        ┌───────────────────────────────────┐
        │ completed · failed · killed ·     │
        │ cancelled                         │
        └───────────────────────────────────┘
```

- `pending` — task created, queued for execution
- `running` — agent loop active
- `streaming` — agent producing output (sub-state of running)
- `waiting_input` — agent requested user input via `input.required` event
- `waiting_approval` — tool call requires human approval via `tool.approval_required` event
- `paused` — explicitly paused by client via `task.pause`
- `completed` — agent finished successfully
- `failed` — agent encountered unrecoverable error
- `killed` — terminated by budget exhaustion or system
- `cancelled` — cancelled by client via `task.cancel`

Terminal states (`completed`, `failed`, `killed`, `cancelled`) are final. No transitions out.

### Client → Server commands

| Command | Payload | Description |
|---|---|---|
| `task.create` | `{ tenant?, task, agent_type?, metadata? }` | Create and start a new task |
| `task.cancel` | `{ task_id }` | Cancel a running task |
| `task.pause` | `{ task_id }` | Pause a running task |
| `task.resume` | `{ task_id }` | Resume a paused task |
| `input.response` | `{ task_id, content }` | Respond to an `input.required` event |
| `approval.respond` | `{ task_id, tool_call_id, approved, reason? }` | Respond to a `tool.approval_required` event |

### Server → Client events

| Event | Payload | Description |
|---|---|---|
| `task.state_changed` | `{ task_id, from, to, reason? }` | Task lifecycle transition |
| `agent.thinking` | `{ task_id, content? }` | Agent reasoning (streaming) |
| `agent.message` | `{ task_id, content, role }` | Agent text output |
| `tool.called` | `{ task_id, tool_call_id, tool_name, arguments }` | Tool invocation started |
| `tool.call_delta` | `{ task_id, index, tool_call_id?, tool_name?, arguments_delta }` | Tool-call input streamed before invocation starts |
| `tool.result` | `{ task_id, tool_call_id, result, duration_ms }` | Tool invocation completed |
| `tool.approval_required` | `{ task_id, tool_call_id, tool_name, arguments, reason }` | Tool needs human approval |
| `input.required` | `{ task_id, prompt, schema? }` | Agent needs user input |
| `artifact.created` | `{ task_id, artifact_id, name, mime_type, size }` | Agent produced an artifact |
| `payment.required` | `{ task_id, amount, currency, vendor, reason }` | Payment needs approval (S030) |
| `hook.fired` | `{ task_id, hook_name, operation, verdict }` | Governance hook executed (S026) |
| `budget.warning` | `{ task_id, budget_type, used, limit, pct }` | Budget threshold crossed |
| `error` | `{ task_id?, code, message }` | Error (task-scoped or connection-scoped) |

### ProtocolAdapter trait

```rust
/// Translates between an external protocol and the native Simulacra API.
/// Implementations: A2A (agent-as-service), AG-UI (chat UI embedding).
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Protocol identifier (e.g., "a2a", "ag-ui").
    fn protocol_id(&self) -> &str;

    /// Mount routes on the given axum Router.
    fn routes(&self, engine: Arc<SimulacraEngine>) -> axum::Router;

    /// Translate an inbound protocol message to a native command.
    async fn translate_inbound(
        &self,
        request: ProtocolRequest,
    ) -> Result<NativeCommand, ProtocolError>;

    /// Translate a native event to the protocol's outbound format.
    async fn translate_outbound(
        &self,
        event: NativeEvent,
    ) -> Result<Option<ProtocolResponse>, ProtocolError>;
}
```

Protocol adapters are registered at startup. Each mounts its own routes (e.g., `/a2a/...`, `/ag-ui/...`). The native API is always available at `/api/v1/...`.

### AuthProvider trait

```rust
/// Authentication provider for the API server.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Validate credentials and return an authenticated identity.
    async fn authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError>;
}

pub struct Identity {
    pub subject: String,           // User or service principal
    pub tenant_hint: Option<String>, // From OIDC claim or API key metadata
    pub scopes: Vec<String>,       // Granted scopes
    pub metadata: serde_json::Value,
}

pub enum Credentials {
    Bearer(String),    // OIDC token
    ApiKey(String),    // Service-to-service key
}
```

`OidcAuthProvider` validates JWT tokens against the configured issuer. `ApiKeyAuthProvider` looks up keys from environment or config. Both return an `Identity` that the `TenantResolver` maps to a `TenantConfig`.

### TenantResolver

```rust
pub struct TenantConfig {
    pub namespace: String,
    pub agent_type: String,
    pub vfs_root: PathBuf,
    pub budget_pool: BudgetPoolConfig,
    pub hooks: Vec<HookConfig>,
}

pub struct TenantResolver {
    tenants: HashMap<String, TenantConfig>,
}

impl TenantResolver {
    /// Resolve an identity to a tenant config.
    /// Uses identity.tenant_hint, falls back to default tenant.
    pub fn resolve(&self, identity: &Identity) -> Result<&TenantConfig, TenantError>;
}
```

### Config

```toml
[server]
host = "0.0.0.0"
port = 8080

[auth.oidc]
issuer = "https://company.okta.com"
audience = "simulacra-api"
tenant_claim = "org.department"

[auth.api_keys]
keys_env = "SIMULACRA_API_KEYS"

[tenants.accounting]
agent_type = "accounting-agent"
vfs_root = "/data/accounting/"
budget_pool = { max_tokens = 1000000, max_cost = "500.00" }

[tenants.csm]
agent_type = "csm-agent"
vfs_root = "/data/csm/"
budget_pool = { max_tokens = 500000, max_cost = "250.00" }
```

### Crate position in dependency graph

```
simulacra-types (leaf)
  ├→ simulacra-runtime (engine crates)
  └→ simulacra-server (axum, tokio-tungstenite, simulacra-runtime, simulacra-types, simulacra-config)
       └→ simulacra-cli (optional: simulacra-server for `simulacra serve` subcommand)
```

`simulacra-server` depends on `simulacra-runtime` (the full engine) and `simulacra-config` (for server/tenant configuration). It does not depend on `simulacra-cli`.

## Behavior

### Server startup

1. `simulacra-server` reads `[server]`, `[auth]`, and `[tenants]` config sections.
2. Auth providers are initialized: OIDC provider validates issuer reachability, API key provider loads keys from environment.
3. Tenant configs are validated: each tenant must have a valid `vfs_root` and `agent_type`.
4. Protocol adapters are registered and mount their routes.
5. The HTTP server binds to `host:port` and begins accepting connections.
6. If OIDC issuer is unreachable at startup, the server logs a warning and starts (OIDC validation will fail until issuer is reachable).

### Authentication

7. Every request (HTTP or WebSocket upgrade) passes through the auth middleware.
8. Bearer tokens are validated by `OidcAuthProvider` (JWT signature, issuer, audience, expiry).
9. API keys are validated by `ApiKeyAuthProvider` (lookup in configured key set).
10. Invalid or missing credentials return HTTP 401.
11. The authenticated `Identity` is attached to the request context.

### Tenant resolution

12. `TenantResolver` maps `Identity` to `TenantConfig` using `tenant_hint` (from OIDC claim or API key metadata).
13. If no `tenant_hint` is present and a default tenant is configured, the default is used.
14. If no tenant can be resolved, the request returns HTTP 403.
15. The resolved `TenantConfig` determines VFS root, agent type, budget pool, and hooks for all tasks on this connection.

### WebSocket transport

16. WebSocket connections are established at `/api/v1/ws`.
17. Client sends JSON-encoded commands; server sends JSON-encoded events.
18. Multiple concurrent tasks are multiplexed on a single WebSocket connection (each event carries `task_id`).
19. WebSocket close triggers cancellation of all active tasks on that connection.
20. Malformed messages receive an `error` event with `code: "invalid_message"`.

### REST+SSE transport

21. REST commands are sent via POST to `/api/v1/tasks/{action}` (e.g., `/api/v1/tasks/create`).
22. SSE event streams are opened via GET `/api/v1/tasks/{task_id}/events`.
23. Each REST command returns a synchronous acknowledgment (task_id, initial state).
24. Events for the task are delivered on the SSE stream.
25. SSE stream closes when the task reaches a terminal state.

### Task lifecycle

26. `task.create` validates the request, resolves the tenant, creates a `ResourceBudget` from the tenant's budget pool, and spawns the agent.
27. The task enters `pending` state, then transitions to `running` when the agent loop starts.
28. `task.cancel` sends a cancellation signal. The agent completes its current operation, then transitions to `cancelled`.
29. `task.pause` suspends the agent loop. The task transitions to `paused`. The agent does not consume LLM tokens while paused.
30. `task.resume` resumes a paused task. The task transitions back to `running`.
31. When the agent requests user input, the task transitions to `waiting_input`. `input.response` provides the input and transitions back to `running`.
32. When a tool call requires approval, the task transitions to `waiting_approval`. `approval.respond` provides the decision and transitions back to `running`.
33. Terminal states (`completed`, `failed`, `killed`, `cancelled`) emit a final `task.state_changed` event.

### Event stream

34. All task activity is emitted as events on the connection's event stream.
35. Events are ordered per-task (monotonic sequence number within each task).
36. `agent.thinking` events are streamed as the LLM produces reasoning tokens.
37. `tool.called` is emitted when a tool invocation begins; `tool.result` when it completes.
38. `budget.warning` is emitted when budget usage crosses 80% and 95% thresholds.
39. `error` events with a `task_id` are task-scoped. `error` events without `task_id` are connection-scoped.

### Agent-consumable API

40. Every endpoint returns JSON with consistent envelope: `{ "ok": true, "data": ... }` or `{ "ok": false, "error": { "code": "...", "message": "..." } }`.
41. The API is self-describing: GET `/api/v1/schema` returns the full command/event schema.
42. The API can be exposed as an MCP server: each command maps to an MCP tool, events map to MCP notifications.

### Concurrent tasks

43. A single connection can have multiple active tasks.
44. Each task has independent state, budget tracking, and event stream.
45. Task events are interleaved on the connection's event stream, distinguished by `task_id`.
46. Connection-level rate limiting prevents a single client from overwhelming the server.

## Assertions

### Server startup

- [x] Server reads `[server]` config and binds to the configured host:port.
- [x] Server initializes OIDC auth provider from `[auth.oidc]` config.
- [x] Server initializes API key auth provider from `[auth.api_keys]` config.
- [x] Server validates tenant configs at startup (invalid VFS root or missing agent type is an error).
- [x] Server starts even if OIDC issuer is unreachable (logs warning).
- [x] Protocol adapters mount their routes at startup.

### Authentication

- [x] Valid OIDC Bearer token returns an `Identity` with subject and tenant_hint from configured claim.
- [x] Expired OIDC token returns HTTP 401.
- [x] Invalid OIDC signature returns HTTP 401.
- [x] Valid API key returns an `Identity` with subject and tenant_hint from key metadata.
- [x] Unknown API key returns HTTP 401.
- [x] Missing credentials return HTTP 401.

### Tenant resolution

- [x] `TenantResolver` maps identity with `tenant_hint = "accounting"` to the accounting `TenantConfig`.
- [x] Identity with no `tenant_hint` and no default tenant returns HTTP 403.
- [x] Identity with no `tenant_hint` and a default tenant resolves to the default.
- [x] Identity with `tenant_hint` for a nonexistent tenant returns HTTP 403.

### WebSocket transport

- [x] WebSocket connection at `/api/v1/ws` succeeds with valid auth.
- [x] Client can send `task.create` command over WebSocket and receive events.
- [x] Multiple concurrent tasks on a single WebSocket connection receive interleaved events with correct `task_id`.
- [x] WebSocket close cancels all active tasks on that connection.
- [x] Malformed WebSocket message returns `error` event with `code: "invalid_message"`.

### REST+SSE transport

- [x] POST `/api/v1/tasks/create` with valid auth creates a task and returns acknowledgment.
- [x] GET `/api/v1/tasks/{task_id}/events` opens an SSE stream with task events.
- [x] SSE stream closes when task reaches terminal state.
- [x] REST command without SSE stream still executes (fire-and-forget).

### Task lifecycle

- [x] `task.create` transitions task from `pending` to `running`.
- [x] `task.cancel` transitions task to `cancelled` after current operation completes.
- [x] `task.pause` transitions running task to `paused`.
- [x] `task.resume` transitions paused task back to `running`.
- [x] `input.response` transitions `waiting_input` task back to `running`.
- [x] `approval.respond` with `approved: true` transitions `waiting_approval` task back to `running`.
- [x] `approval.respond` with `approved: false` returns tool error to agent.
- [x] Terminal states are final — commands on terminal tasks return error.
- [x] Task budget is created from tenant's budget pool config.

### Event stream

- [x] Events are ordered per-task with monotonic sequence numbers.
- [x] `agent.thinking` events stream as LLM produces tokens.
- [x] `tool.called` event emitted when tool invocation begins.
- [x] `tool.result` event emitted when tool invocation completes.
- [x] `budget.warning` emitted at 80% and 95% budget thresholds.
- [x] `error` events carry `task_id` when task-scoped, omit it when connection-scoped.

### Agent-consumable API

- [x] All endpoints return consistent JSON envelope (`ok`, `data`/`error`).
- [x] GET `/api/v1/schema` returns command/event schema.
- [x] API endpoints are expressible as MCP tools.

### Protocol adapters

- [x] `ProtocolAdapter` trait allows mounting custom routes.
- [x] Adapters translate inbound requests to native commands.
- [x] Adapters translate native events to protocol-specific responses.
- [x] Native API is always available at `/api/v1/...` regardless of adapters.

## Observability (see S010)

- [x] `simulacra_server_request` span wraps each HTTP/WebSocket request with `simulacra.server.method`, `simulacra.server.path`, `simulacra.server.tenant`.
- [x] `simulacra_server_task` span wraps each task lifecycle with `simulacra.server.task_id`, `simulacra.server.agent_type`, `simulacra.server.tenant`.
- [x] `simulacra.server.active_tasks` gauge tracks concurrent active tasks with `tenant` label.
- [x] `simulacra.server.active_connections` gauge tracks concurrent connections with `transport` label (ws/sse).
- [x] `simulacra.server.task_duration` histogram records task duration with `tenant`, `agent_type`, `terminal_state` labels.
- [x] `simulacra.server.events_emitted` counter tracks events emitted with `event_type` and `tenant` labels.
- [x] `simulacra.server.auth_failures` counter tracks auth failures with `provider` and `reason` labels.
- [x] `tracing::info!` on server startup with bind address, tenant count, adapter count.
- [x] `tracing::info!` on task creation with task_id, tenant, agent_type.
- [x] `tracing::warn!` on auth failure, tenant resolution failure, budget warning.
- [x] `tracing::error!` on task failure, unhandled server error.
