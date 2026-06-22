# S031 — Simulacra API Server

**Status:** Active
**Crates involved:** `simulacra-server` (new), `simulacra-types`, `simulacra-runtime`, `simulacra-config`

## Dependencies

- **S006** — Resource budgets (tenant budget pools, per-task metering)
- **S009** — Agent supervisor (task lifecycle, concurrent agent management)
- **S010** — Observability conventions
- **S012** — Built-in tools (tool registry integration)
- **S026** — Governance hooks (hook pipeline for API-triggered tasks)
- **S030** — Agent payments (budget pool enforcement, payment flow)

## Scope

Native API server wrapping the Simulacra engine with WebSocket bidirectional transport, REST+SSE fallback, protocol adapters (A2A, AG-UI), OAuth2/OIDC + API key auth, namespace-based multi-tenancy, and agent-consumable endpoints.

Full spec: `specs/S031-api-server.md`

## Design

### Why a Native API First

The API is the product surface — for humans AND machines. A human in Slack, a cron job, a webhook, and another agent all create tasks through the same API. Protocol adapters (A2A, AG-UI, Slack) translate their native formats into the Simulacra native protocol. This means:

1. One event model to implement, test, and audit
2. Protocol adapters are thin translation layers, not separate code paths
3. The API itself is agent-consumable — tool-friendly JSON, discoverable as MCP tools
4. Every admin action (cancel task, check budget, query audit log) is available via API

### Transport Architecture

```
┌─────────────────────────────────────────────┐
│  Protocol Adapters                          │
│  A2A (agent-as-service)                     │
│  AG-UI (chat UI embedding)                  │
│  Slack / Teams (future)                     │
├─────────────────────────────────────────────┤
│  Native API                                 │
│  WebSocket (bidirectional, primary)         │
│  REST+SSE (fallback)                        │
├─────────────────────────────────────────────┤
│  Auth + Tenancy                             │
│  OIDC · API Keys · Tenant Resolution        │
├─────────────────────────────────────────────┤
│  TaskManager                                │
│  Task lifecycle · Concurrent tasks          │
│  Budget enforcement · Event emission        │
├─────────────────────────────────────────────┤
│  SimulacraEngine                                │
│  Agent loop · Providers · Tools · Hooks     │
│  VFS · Shell · JS · WASM · Journal · OTel   │
└─────────────────────────────────────────────┘
```

WebSocket is the primary transport — bidirectional, multiplexed, low-latency. REST+SSE is the fallback for environments that can't hold WebSocket connections (corporate proxies, serverless). The `TaskManager` doesn't know which transport delivered the command.

### Multi-Tenancy Model

Namespace-based, not database-based. Each tenant is a configuration namespace:

```toml
[tenants.accounting]
agent_type = "accounting-agent"
vfs_root = "/data/accounting/"
budget_pool = { max_tokens = 1000000, max_cost = "500.00" }
```

The `TenantResolver` maps an authenticated identity to a tenant config. The mapping comes from OIDC claims (e.g., `org.department = "accounting"`) or API key metadata. Once resolved, the tenant config determines everything: VFS root, agent type, budget pool, governance hooks.

This is simple, explicit, and auditable. No shared state between tenants. No cross-tenant data access. The VFS root is the isolation boundary.

### Agent Lifecycle State Machine

```
pending → running → { streaming, waiting_input, waiting_approval, paused } → { completed, failed, killed, cancelled }
```

Key design decisions:
- `streaming`, `waiting_input`, `waiting_approval` are sub-states of `running` — the agent is alive but blocked on something
- `paused` is explicit suspension — the agent is not consuming resources
- Terminal states are final — no resurrection
- Every transition emits a `task.state_changed` event with `from`, `to`, and optional `reason`

### Event Model

Events are the primary communication channel from server to client. They cover every observable thing that happens during task execution:

| Category | Events |
|---|---|
| Lifecycle | `task.state_changed` |
| Agent output | `agent.thinking`, `agent.message` |
| Tools | `tool.called`, `tool.result`, `tool.approval_required` |
| Interaction | `input.required` |
| Artifacts | `artifact.created` |
| Governance | `hook.fired`, `payment.required`, `budget.warning` |
| System | `error` |

Events are ordered per-task (monotonic sequence number). This enables clients to detect gaps and request replay.

### Auth Model

Two providers, same interface:

- **OIDC** — for humans. JWT validation against configured issuer. Tenant extracted from a configurable claim.
- **API Keys** — for services. Looked up from environment. Tenant embedded in key metadata.

Both return an `Identity` that flows through tenant resolution into task creation. The agent never sees auth details.

### API-as-MCP-Server

Every command maps to an MCP tool. `task.create` → MCP tool `simulacra_task_create`. Events map to MCP notifications. This means an agent running somewhere else can manage Simulacra tasks through standard MCP, without custom integration code.
