# S010 — Observability Conventions (Reference)

**Status:** Reference (not a feature spec — o11y ships with each feature)
**Applies to:** All crates with side effects

This document defines the naming conventions, attribute schemas, and metric formats used across all Simulacra specs. Individual specs reference this document for their o11y assertions. S010 itself has no assertions — the assertions live in the specs where the side effects occur.

## Principle

Observability is not a feature. It ships with the feature, like error handling. Every side-effecting operation must produce spans, metrics, and logs as defined here and asserted in its own spec.

## Span Conventions

### LLM Calls (GenAI Semantic Conventions v1.37+)

| Attribute | Value | Required |
|---|---|---|
| `gen_ai.operation.name` | `chat` | Yes |
| `gen_ai.request.model` | Exact model string | Yes |
| `gen_ai.provider.name` | `anthropic` \| `openai` \| `ollama` | Yes |
| `gen_ai.usage.input_tokens` | Integer | Yes (on response) |
| `gen_ai.usage.output_tokens` | Integer | Yes (on response) |
| `gen_ai.request.temperature` | Float | If set |
| `gen_ai.request.max_tokens` | Integer | If set |
| `gen_ai.response.id` | String | Yes (on response) |
| `gen_ai.response.finish_reasons` | JSON array | Yes (on response) |
| `server.address` | Hostname | Yes |
| `server.port` | Integer | Yes |

Span name format: `chat {gen_ai.request.model}`

### Agent Invocation

| Attribute | Value |
|---|---|
| `gen_ai.operation.name` | `invoke_agent` |
| `gen_ai.agent.name` | Agent ID string |

Span kind: `INTERNAL`

### Sub-Agent Creation

| Attribute | Value |
|---|---|
| `gen_ai.operation.name` | `create_agent` |
| `gen_ai.agent.name` | Child agent ID |

### Tool Calls

| Attribute | Value |
|---|---|
| `gen_ai.operation.name` | `execute_tool` |
| `simulacra.tool.name` | Tool name |
| `simulacra.tool.source` | `builtin` \| `mcp:{server}` |

Events: `gen_ai.tool.message` with tool input/output.

### Shell Commands

| Attribute | Value |
|---|---|
| `simulacra.operation.name` | `shell_command` |
| `simulacra.shell.command` | Command string (sanitized) |
| `simulacra.shell.exit_code` | Integer |

### JS Execution

| Attribute | Value |
|---|---|
| `simulacra.operation.name` | `js_execute` |
| `simulacra.js.module` | Module path |

### File Operations (VFS)

| Attribute | Value |
|---|---|
| `simulacra.operation.name` | `vfs_{read,write,delete,list}` |
| `simulacra.vfs.path` | Virtual path |

### Journal Operations

| Attribute | Value |
|---|---|
| `simulacra.operation.name` | `journal_{append,checkpoint,replay,fork}` |
| `simulacra.journal.entry_kind` | Entry type name |
| `simulacra.journal.mode` | `live` \| `replayed` |

### Capability Checks

Denials are logged as events on the current span:

| Attribute | Value |
|---|---|
| `simulacra.capability.operation` | What was denied |
| `simulacra.capability.reason` | Denial reason |

Severity: `WARN`

## Metric Conventions

| Metric | Type | Labels | Owner |
|---|---|---|---|
| `gen_ai.client.token.usage` | Histogram | `operation`, `model` | S007 |
| `gen_ai.client.operation.duration` | Histogram | `operation`, `model` | S007 |
| `simulacra.agent.turns` | Counter | `agent_name` | S009 |
| `simulacra.agent.budget.remaining` | Gauge | `agent_name`, `resource` | S006 |
| `simulacra.journal.entries` | Counter | `entry_kind` | S005 |
| `simulacra.journal.replay.ratio` | Gauge | `agent_name` | S005 |
| `simulacra.tool.calls` | Counter | `tool_name`, `source` | S007 |
| `simulacra.shell.commands` | Counter | `command`, `exit_code` | S002 |
| `simulacra.capability.denials` | Counter | `operation` | S004 |
| `simulacra.mcp.calls` | Counter | `server`, `tool` | S008 |

## Log Conventions

| Event | Level | Content |
|---|---|---|
| Budget exhausted | `WARN` | Resource name, used, limit |
| Capability denied | `WARN` | Operation, reason, agent |
| Agent spawned | `INFO` | Agent name, parent, capabilities |
| Agent completed | `INFO` | Agent name, exit reason, token total |
| Agent restarted | `WARN` | Agent name, restart strategy, failure reason |
| Replay divergence | `ERROR` | Expected entry kind, actual entry kind, journal position |
| MCP connection failed | `WARN` | Server name, error |
| Schema version mismatch | `ERROR` | Expected version, found version |

## Local Validation via Obsidian

[Obsidian](https://github.com/tritonrc/obsidian) is a single-binary o11y backend that accepts OTLP and exposes PromQL, LogQL, and TraceQL. Every dev session runs a local instance. Coding agents query Obsidian to validate o11y assertions — see `rules/R010-observability-validation.md` for the process.

| Signal | Ingest | Query |
|---|---|---|
| Traces | OTLP protobuf | TraceQL via `/api/search` |
| Metrics | OTLP protobuf | PromQL via `/api/v1/query` |
| Logs | Loki JSON/protobuf | LogQL via `/loki/api/v1/query` |

OTLP endpoint: `http://localhost:${OBSIDIAN_PORT:-4320}`

## Namespace Rule

- `gen_ai.*` — OTel GenAI Semantic Conventions only. Never use for Simulacra-specific concerns.
- `simulacra.*` — Simulacra-specific spans, metrics, and attributes.
- `server.*` — Standard OTel server attributes (address, port).
