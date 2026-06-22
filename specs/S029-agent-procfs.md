# S029 — Agent Procfs (Virtual Process Filesystem)

**Status:** Active
**Crate:** `simulacra-vfs` (ProcFs layer), `simulacra-sandbox` (wiring)

## Dependencies

- **S001** — Virtual filesystem (VirtualFs trait, path resolution, OverlayFs)
- **S004** — Capability tokens (paths_read gates /proc access)
- **S005** — Journal (reads through VFS are journaled)
- **S006** — Resource budgets (budget values exposed via /proc/budget/)
- **S012** — Built-in tools (file_read, list_dir are the access mechanism)

## Scope

Mount a virtual `/proc` directory in the agent's VFS that exposes runtime state as readable files. Agents introspect their own identity, budget, capabilities, tools, session, and hooks by reading files through existing `file_read` and `list_dir` operations. A writable `/proc/mailbox/` directory lets agents produce artifacts. No new tools. No new protocols. Just files.

**In scope:**
- `ProcFs` — a `VirtualFs` layer that intercepts reads to `/proc/**` and returns dynamic values
- `/proc/agent/` — identity files (id, name, model, turn, parent_id)
- `/proc/budget/` — resource budget counters (max/used/remaining for tokens, turns, fuel, cost)
- `/proc/capabilities/` — current capability token values
- `/proc/tools/` — registered tool definitions as JSON
- `/proc/session/` — session metadata (id, uptime_ms, journal_entries)
- `/proc/hooks/` — registered hook names per event type
- `/proc/mailbox/` — writable directory for agent-produced artifacts (delegated to inner VFS)
- Integration into `AgentCell` composition (ProcFs wraps the existing VFS stack)

**Out of scope:**
- `/proc/signals/` — writable control signals (checkpoint, pause) — future spec
- `/proc/peers/` — cross-agent visibility — future spec
- `/proc/metrics/` — custom agent-defined metrics — future spec
- MCP server exposing /proc — not needed, VFS IS the interface
- Special tools for introspection — the whole point is no special tools
- Binary or structured encoding beyond JSON for tool definitions

## Context

The key insight: the VFS is the universal interface for ALL agent state. Instead of building special tools or MCP servers for introspection, we expose runtime state as virtual files. Agents are already excellent at reading files. We already have capability gating and hooks on file I/O. Everything composes naturally.

Linux's `/proc` filesystem exposes kernel and process state as readable files. No special API — just `cat /proc/cpuinfo`. Any tool that can read files can inspect system state. Simulacra adopts the same pattern: a virtual `/proc` directory mounted in the agent's VFS exposes agent runtime state through the same `read_file` and `list_dir` operations the agent already uses.

This eliminates the need for dedicated introspection tools. An agent that wants to know its remaining budget reads `/proc/budget/remaining_tokens`. An agent that wants to discover available tools reads `list_dir("/proc/tools/")`. Governance hooks see these reads. Capability tokens gate access. The journal records what the agent introspected and when. No new mechanisms — just files.

The mailbox pattern falls out naturally: `/proc/mailbox/` is a writable directory within the proc mount. Agents write artifacts there as normal VFS files. The host/operator reads them. No special collection mechanism — the VFS IS the artifact system. Writes to `/proc/mailbox/**` go through the normal VFS write path: capability-gated, journaled, hook-visible.

## Design

### ProcFs layer

`ProcFs` implements `VirtualFs` and wraps an inner VFS (the agent's real filesystem). It is the outermost layer in the VFS composition:

```rust
pub struct ProcFs<V: VirtualFs> {
    inner: V,
    state: Arc<ProcState>,
}

pub struct ProcState {
    agent_id: String,
    agent_name: String,
    model: String,
    parent_id: Option<String>,
    budget: Arc<ResourceBudget>,
    capabilities: CapabilityToken,
    tools: Arc<dyn ToolRegistry>,
    session: Arc<SessionState>,
    hooks: Arc<dyn HookRegistry>,
    turn: Arc<AtomicU64>,
}
```

Path routing:
- `/proc/mailbox/**` — delegated to inner VFS (read + write)
- `/proc/**` (all other) — intercepted by ProcFs handlers (read-only)
- Everything else — delegated to inner VFS unchanged

### Composition in AgentCell

```
  OverlayFs (upper: scratch, lower: host mounts)
       │
       ▼
  ProcFs (intercepts /proc/**, delegates rest to inner)
       │
       ▼
  AgentCell sees unified VFS with /proc + workspace + host mounts
```

ProcFs wraps the OverlayFs. AgentCell's `read_file` and `list_dir` go through ProcFs, which either handles `/proc/**` paths or delegates to the inner OverlayFs.

### Virtual file tree

```
/proc/
  agent/
    id                  → agent ID string
    name                → agent type name ("default", "researcher", etc.)
    model               → model string ("claude-sonnet-4-6")
    turn                → current turn number (e.g., "3")
    parent_id           → parent agent ID (empty string for root agent)

  budget/
    max_tokens          → "100000" (0 = unlimited)
    used_tokens         → "4521"
    remaining_tokens    → "95479"
    max_turns           → "10"
    used_turns          → "3"
    remaining_turns     → "7"
    max_fuel            → "0"
    used_fuel           → "1273"
    max_cost            → "0.00"
    used_cost           → "0.12"

  capabilities/
    shell               → "true" or "false"
    javascript          → "true" or "false"
    python              → "true" or "false"
    network             → newline-separated patterns ("*\n*.github.com")
    mcp_tools           → newline-separated patterns ("mcp:*:*")
    paths_read          → newline-separated patterns ("/**")
    paths_write         → newline-separated patterns ("/**")

  tools/
    <tool_name>         → JSON: {"name": "...", "description": "...", "input_schema": {...}}
    (one virtual file per registered tool)

  session/
    id                  → session ID string
    uptime_ms           → milliseconds since agent started (e.g., "12345")
    journal_entries     → count of journal entries (e.g., "42")

  hooks/
    tool_call           → newline-separated hook names in chain order
    llm                 → newline-separated hook names
    spawn               → newline-separated hook names
    http_request        → newline-separated hook names

  mailbox/              → writable directory (normal VFS files)
    report.md
    analysis.json
    chart.svg
```

### Encoding rules

| Data type | Encoding | Example |
|---|---|---|
| Scalar string | UTF-8 text, no trailing newline | `"agent-abc123"` |
| Scalar number | Decimal string, no trailing newline | `"42371"` |
| Scalar boolean | `"true"` or `"false"` (lowercase) | `"true"` |
| Decimal (money) | String with two decimal places | `"1.50"` |
| List | Newline-separated values | `"*.github.com\napi.stripe.com"` |
| Structured | Compact JSON object | `{"name":"file_read",...}` |
| Empty/absent | Empty string | `""` |

### State references

The procfs handler needs access to live runtime state. At the `AgentCell` / `AgentLoop` level, the following are available:

| `/proc` subtree | State source |
|---|---|
| `/proc/agent/` | `AgentId`, agent config (name, model, parent) |
| `/proc/budget/` | `ResourceBudget` (live counters) |
| `/proc/capabilities/` | `CapabilityToken` |
| `/proc/tools/` | `ToolRegistry` |
| `/proc/session/` | Session ID, start time, journal entry count |
| `/proc/hooks/` | `HookPipeline` (registered hook names per operation type) |

### Dynamic values

Every read to a `/proc` file computes the value at read time. There is no caching. Budget values reflect current state. Turn count reflects current turn. Tool list reflects current registry contents. This ensures agents always see accurate state.

### Read-only enforcement

Writes to `/proc/**` (except `/proc/mailbox/`) return `VfsError::PermissionDenied`. This is enforced at the ProcFs layer, independent of capability tokens. Even an agent with `paths_write: ["/**"]` cannot write to `/proc/agent/id`.

### Capability gating

`/proc` access is gated by `paths_read` on the agent's capability token, same as any other VFS path. An agent with `paths_read: ["/workspace/**"]` cannot read `/proc/agent/id`. An agent with `paths_read: ["/**"]` or `paths_read: ["/proc/**"]` can.

Default capability tokens should include `/proc/**` in paths_read so agents can introspect by default. Operators can restrict this by attenuating paths_read.

### Mailbox semantics

`/proc/mailbox/` is a writable directory within the proc mount, but writes are delegated to the inner VFS. Agents write artifacts here:

```
/proc/mailbox/
  report.md
  analysis.json
  chart.svg
```

The host/operator reads mailbox contents after agent completion. No special collection mechanism — the VFS IS the artifact system. Mailbox files are included in VFS snapshots and journal checkpoints.

Writes to `/proc/mailbox/**` follow normal VFS write paths: capability check (paths_write must include `/proc/mailbox/**`), journal entry, hook visibility. Reads from `/proc/mailbox/**` are also delegated to the inner VFS.

## Behavior

### Virtual mount recognition

1. The VFS recognizes `/proc/` as a virtual mount prefix. Reads to any path under `/proc/` (except `/proc/mailbox/`) are dispatched to the procfs handler instead of the inner VFS.
2. `/proc/mailbox/**` reads and writes are delegated to the inner VFS. The procfs handler never sees mailbox operations.
3. All non-`/proc` paths are delegated to the inner VFS unchanged.

### Agent identity

4. `read_file("/proc/agent/id")` returns the agent's unique identifier string.
5. `read_file("/proc/agent/name")` returns the agent's configured type name.
6. `read_file("/proc/agent/model")` returns the LLM model identifier string.
7. `read_file("/proc/agent/turn")` returns the current turn number as a decimal string.
8. `read_file("/proc/agent/parent_id")` returns the parent agent's ID, or empty string for root agent.

### Budget exposure

9. `read_file("/proc/budget/max_tokens")` returns the token budget limit. `"0"` means unlimited.
10. `read_file("/proc/budget/used_tokens")` returns tokens consumed so far.
11. `read_file("/proc/budget/remaining_tokens")` returns `max - used`, or `"0"` if unlimited (max = 0).
12. `read_file("/proc/budget/max_turns")` returns the turn limit.
13. `read_file("/proc/budget/used_turns")` returns turns consumed.
14. `read_file("/proc/budget/remaining_turns")` returns `max - used`, or `"0"` if unlimited.
15. `read_file("/proc/budget/max_fuel")` returns the fuel limit.
16. `read_file("/proc/budget/used_fuel")` returns fuel consumed.
17. `read_file("/proc/budget/max_cost")` returns the cost limit as a decimal string with two places.
18. `read_file("/proc/budget/used_cost")` returns cost consumed as a decimal string with two places.
19. Budget values are computed at read time — never stale. Two consecutive reads of `/proc/budget/used_tokens` may return different values if a tool call occurred between them.

### Capability exposure

20. `read_file("/proc/capabilities/shell")` returns `"true"` if shell capability is granted, `"false"` otherwise.
21. `read_file("/proc/capabilities/javascript")` returns `"true"` or `"false"`.
22. `read_file("/proc/capabilities/python")` returns `"true"` or `"false"`.
23. `read_file("/proc/capabilities/network")` returns newline-separated URL patterns, or empty string if no network access.
24. `read_file("/proc/capabilities/mcp_tools")` returns newline-separated MCP tool glob patterns.
25. `read_file("/proc/capabilities/paths_read")` returns newline-separated read path patterns.
26. `read_file("/proc/capabilities/paths_write")` returns newline-separated write path patterns.

### Tool exposure

27. `list_dir("/proc/tools/")` returns the name of every registered tool, sorted.
28. `read_file("/proc/tools/<name>")` returns a compact JSON object with `name`, `description`, and `input_schema` fields.
29. `read_file("/proc/tools/<nonexistent>")` returns a not-found error.

### Session exposure

30. `read_file("/proc/session/id")` returns the session identifier.
31. `read_file("/proc/session/uptime_ms")` returns milliseconds since agent start as a decimal string.
32. `read_file("/proc/session/journal_entries")` returns the count of journal entries as a decimal string.

### Hook exposure

33. `read_file("/proc/hooks/tool_call")` returns newline-separated names of hooks registered for the `tool_call` event, in execution order.
34. `read_file("/proc/hooks/llm")` returns newline-separated names of hooks registered for the `llm` event.
35. `read_file("/proc/hooks/spawn")` returns newline-separated names of hooks registered for the `spawn` event.
36. `read_file("/proc/hooks/http_request")` returns newline-separated names of hooks registered for the `http_request` event.
37. Reading a hook event type with no registered hooks returns an empty string.

### Directory listing

38. `list_dir("/proc/")` returns `["agent", "budget", "capabilities", "hooks", "mailbox", "session", "tools"]` (sorted).
39. `list_dir("/proc/agent/")` returns `["id", "model", "name", "parent_id", "turn"]` (sorted).
40. `list_dir("/proc/budget/")` returns all budget file names (sorted).
41. `list_dir("/proc/tools/")` returns the name of every registered tool (sorted).
42. `list_dir("/proc/capabilities/")` returns `["javascript", "mcp_tools", "network", "paths_read", "paths_write", "python", "shell"]` (sorted).
43. `list_dir("/proc/hooks/")` returns `["http_request", "llm", "spawn", "tool_call"]` (sorted).
44. `list_dir("/proc/session/")` returns `["id", "journal_entries", "uptime_ms"]` (sorted).
45. `list_dir("/proc/mailbox/")` returns mailbox file names from the inner VFS.

### Dynamic values

46. Budget values reflect the current state at the time of read, not a cached value.
47. `list_dir("/proc/tools/")` reflects the current tool registry. If a tool is registered or removed, the listing changes.
48. `/proc/agent/turn` increments as the agent loop progresses.
49. `/proc/session/uptime_ms` increases monotonically with each read.
50. `/proc/session/journal_entries` reflects the current journal count.

### Write protection

51. `write("/proc/agent/id", ...)` returns `VfsError::PermissionDenied`.
52. `write("/proc/budget/max_tokens", ...)` returns `VfsError::PermissionDenied`.
53. `remove("/proc/tools/file_read")` returns `VfsError::PermissionDenied`.
54. `mkdir("/proc/custom")` returns `VfsError::PermissionDenied`.
55. All write, remove, and mkdir operations on `/proc/**` (excluding `/proc/mailbox/`) are rejected with `VfsError::PermissionDenied`.

### Mailbox

56. Writing to `/proc/mailbox/<file>` succeeds (delegated to inner VFS, subject to capability check).
57. Reading from `/proc/mailbox/<file>` returns the written content.
58. `list_dir("/proc/mailbox/")` returns mailbox file names.
59. Mailbox files are included in VFS snapshots and survive snapshot/restore.
60. Mailbox writes go through the normal VFS write path (capability check, journal, hooks).

### Capability gating

61. An agent with `paths_read = ["/workspace/**"]` cannot read any `/proc` path. The read returns a capability error.
62. An agent with `paths_read = ["/workspace/**", "/proc/budget/**"]` can read `/proc/budget/remaining_tokens` but not `/proc/capabilities/shell`.
63. An agent with `paths_read = ["/proc/**"]` can read all procfs paths.
64. An agent with `paths_read = ["/**"]` can read all procfs paths (wildcard includes `/proc`).
65. Capability checks happen at the AgentCell proxy layer, before the procfs handler is invoked. A denied read never reaches the handler.

### Metadata and existence

66. `exists("/proc/agent/id")` returns true.
67. `exists("/proc/nonexistent")` returns false.
68. `metadata("/proc")` returns directory metadata.
69. `metadata("/proc/agent")` returns directory metadata.
70. `metadata("/proc/agent/id")` returns file metadata with size equal to the current value's byte length.

### Journaling and hooks

71. Every `/proc` read produces a journal entry, same as any other VFS read. The journal entry includes the path read.
72. `list_dir` on `/proc` subtrees produces a journal entry.
73. Denied reads (capability violation) produce a journal entry recording the denial.
74. Governance hooks (S026) see `/proc` reads — a hook can observe that an agent read its budget or probed its capabilities.
75. No special journal entry type for `/proc` reads — they use the same entry type as regular file reads.

### Unknown paths

76. `read_file("/proc/nonexistent")` returns `VfsError::NotFound`.
77. `read_file("/proc/agent/nonexistent")` returns `VfsError::NotFound`.
78. `list_dir("/proc/nonexistent/")` returns `VfsError::NotFound`.

## Assertions

### Agent identity

- [x] `read_file("/proc/agent/id")` returns the agent's configured ID.
- [x] `read_file("/proc/agent/name")` returns the agent type name.
- [x] `read_file("/proc/agent/model")` returns the model string.
- [x] `read_file("/proc/agent/turn")` returns the current turn number as a string.
- [x] `read_file("/proc/agent/parent_id")` returns empty string for root agent.
- [x] `read_file("/proc/agent/parent_id")` returns parent ID for a child agent.

### Budget

- [x] `read_file("/proc/budget/max_tokens")` returns the configured token limit.
- [x] `read_file("/proc/budget/used_tokens")` returns current token usage.
- [x] `read_file("/proc/budget/remaining_tokens")` returns `max - used`.
- [x] `read_file("/proc/budget/remaining_tokens")` returns `"0"` when `max_tokens = 0` (unlimited).
- [x] `read_file("/proc/budget/max_turns")` returns the configured turn limit.
- [x] `read_file("/proc/budget/remaining_turns")` returns `max - used`.
- [x] `read_file("/proc/budget/used_cost")` returns cost with two decimal places.
- [x] Budget values are dynamic: a read after consuming tokens returns updated values.

### Capabilities

- [x] `read_file("/proc/capabilities/shell")` returns `"true"` when shell is granted.
- [x] `read_file("/proc/capabilities/shell")` returns `"false"` when shell is not granted.
- [x] `read_file("/proc/capabilities/network")` returns newline-separated patterns.
- [x] `read_file("/proc/capabilities/network")` returns empty string when no network access.
- [x] `read_file("/proc/capabilities/paths_read")` returns newline-separated path patterns.
- [x] `read_file("/proc/capabilities/mcp_tools")` returns newline-separated MCP tool patterns.
- [x] `read_file("/proc/capabilities/paths_write")` returns newline-separated write path patterns.

### Tools

- [x] `list_dir("/proc/tools/")` returns names of all registered tools, sorted.
- [x] `read_file("/proc/tools/<name>")` returns JSON with `name`, `description`, and `input_schema`.
- [x] `read_file("/proc/tools/<nonexistent>")` returns not-found error.
- [x] Tool listing reflects dynamic registry changes (tool added/removed between reads).

### Session

- [x] `read_file("/proc/session/id")` returns the session ID.
- [x] `read_file("/proc/session/uptime_ms")` returns a numeric string that increases over time.
- [x] `read_file("/proc/session/journal_entries")` returns current journal entry count.

### Hooks

- [x] `read_file("/proc/hooks/tool_call")` returns newline-separated hook names.
- [x] `read_file("/proc/hooks/tool_call")` returns empty string when no hooks registered.
- [x] `list_dir("/proc/hooks/")` returns all four operation types.

### Directory listing

- [x] `list_dir("/proc/")` returns `["agent", "budget", "capabilities", "hooks", "mailbox", "session", "tools"]`.
- [x] `list_dir("/proc/agent/")` returns all agent file names sorted.
- [x] `list_dir("/proc/tools/")` returns one entry per registered tool.
- [x] `list_dir("/proc/budget/")` returns all budget file names sorted.
- [x] `list_dir("/proc/mailbox/")` returns mailbox file names from inner VFS.

### Write protection

- [x] `write("/proc/agent/id", data)` returns `VfsError::PermissionDenied`.
- [x] `write("/proc/budget/max_tokens", data)` returns `VfsError::PermissionDenied`.
- [x] `remove("/proc/tools/file_read")` returns `VfsError::PermissionDenied`.
- [x] `mkdir("/proc/custom")` returns `VfsError::PermissionDenied`.
- [x] All write operations on `/proc/**` (except `/proc/mailbox/`) are rejected.

### Mailbox

- [x] Write to `/proc/mailbox/report.md` succeeds.
- [x] Read from `/proc/mailbox/report.md` returns written content.
- [x] `list_dir("/proc/mailbox/")` shows written files.
- [x] Mailbox files survive VFS snapshot and restore.
- [x] Mailbox writes are subject to `paths_write` capability check.

### Capability gating

- [x] Agent with `paths_read = ["/workspace/**"]` gets capability error on `/proc` reads.
- [x] Agent with `paths_read = ["/workspace/**", "/proc/budget/**"]` can read budget but not capabilities.
- [x] Agent with `paths_read = ["/**"]` can read all `/proc` paths.
- [x] Capability check happens before procfs handler dispatch.

### Journaling

- [x] `/proc` read produces a VFS journal entry with path.
- [x] `/proc` list_dir produces a VFS journal entry.
- [x] Denied `/proc` read produces a journal entry recording the denial.

### Metadata and existence

- [x] `exists("/proc/agent/id")` returns true.
- [x] `exists("/proc/nonexistent")` returns false.
- [x] `metadata("/proc/agent")` returns directory metadata.
- [x] `metadata("/proc/agent/id")` returns file metadata with correct size.

### Unknown paths

- [x] `read_file("/proc/nonexistent")` returns `VfsError::NotFound`.
- [x] `read_file("/proc/agent/nonexistent")` returns `VfsError::NotFound`.
- [x] `list_dir("/proc/nonexistent/")` returns `VfsError::NotFound`.

## Observability (see S010)

- [x] `simulacra_procfs_read` span wraps each procfs read with `simulacra.procfs.path` and `simulacra.procfs.category` (e.g., `agent`, `budget`).
- [x] `simulacra_procfs_list_dir` span wraps each procfs list_dir with `simulacra.procfs.path`.
- [x] `simulacra.procfs.reads` counter incremented per read, with `category` label.
- [x] `tracing::debug!` on each procfs read with path and value length (high frequency, debug not info).
- [x] `tracing::warn!` on write attempt to read-only procfs path.
- [x] `tracing::warn!` on capability-denied procfs access.
- [x] Mailbox writes use standard VFS write observability (no special procfs spans).
