# S029 — Agent Procfs (Virtual Process Filesystem)

**Status:** Active
**Crates involved:** `simulacra-vfs` (ProcFs layer), `simulacra-sandbox` (wiring), `simulacra-types` (VirtualFs trait)

## Dependencies

- **S001** — Virtual filesystem (VirtualFs trait, OverlayFs, path resolution)
- **S004** — Capability tokens (paths_read gates /proc access)
- **S005** — Journal (procfs reads journaled like any file read)
- **S006** — Resource budgets (budget values exposed via /proc/budget/)
- **S012** — Built-in tools (file_read, list_dir are the only access mechanism)

## Scope

Mount a virtual `/proc` directory in the agent's VFS that exposes runtime state as readable files. Writable `/proc/mailbox/` for agent-produced artifacts. No new tools. No new protocols. Just files.

Full spec: `specs/S029-agent-procfs.md`

## The Key Insight

The VFS is the universal interface for ALL agent state. Instead of building special tools or MCP servers for introspection, we expose runtime state as virtual files. Agents are already excellent at reading files. We already have capability gating and hooks on file I/O. Everything composes naturally.

Linux's `/proc` filesystem proved this at the OS level. `cat /proc/cpuinfo` — no special API, no privileged syscalls. Any tool that reads files can inspect system state. The pattern composes because it reuses the existing interface.

Simulacra applies the same pattern. An agent that wants its remaining budget reads `/proc/budget/remaining_tokens`. An agent that wants to discover tools does `list_dir("/proc/tools/")`. Governance hooks see the reads. Capability tokens gate access. The journal records everything. Zero new mechanisms.

## Design

### ProcFs Layer

`ProcFs` is a `VirtualFs` implementation that wraps an inner VFS and intercepts `/proc/**` paths:

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

Three routing rules:
1. `/proc/mailbox/**` — delegate to inner VFS (read + write)
2. `/proc/**` (all other) — procfs handlers (read-only, writes return PermissionDenied)
3. Everything else — delegate to inner VFS unchanged

### VFS Composition Stack

```
  OverlayFs (upper: scratch, lower: host mounts)
       │
       ▼
  ProcFs (intercepts /proc/**, delegates rest)
       │
       ▼
  AgentCell sees unified VFS
```

ProcFs is the outermost layer. It wraps OverlayFs. AgentCell's proxy layer handles capability checks and journaling before any VFS call reaches ProcFs.

### Virtual File Tree

```
/proc/
  agent/      id, name, model, turn, parent_id
  budget/     max_tokens, used_tokens, remaining_tokens, max_turns, ...
  capabilities/  shell, javascript, python, network, mcp_tools, paths_read, paths_write
  tools/      <tool_name> → JSON {name, description, input_schema}
  session/    id, uptime_ms, journal_entries
  hooks/      tool_call, llm, spawn, http_request → newline-separated names
  mailbox/    writable directory for agent artifacts
```

### Encoding

- **Scalars:** Plain UTF-8 text, no trailing newline
- **Booleans:** `"true"` / `"false"`
- **Numbers:** Decimal string. Costs use two decimal places
- **Lists:** Newline-separated. Empty list = empty string
- **Structured:** Compact JSON (tool definitions only)

### Dynamic Values

Every read computes the value at read time. No caching. Budget values reflect current state. Turn count reflects current turn. Tool list reflects current registry. Agents always see accurate, live state.

### Read-Only Enforcement

Writes to `/proc/**` except `/proc/mailbox/**` return `VfsError::PermissionDenied`. This is enforced at the ProcFs layer itself, independent of capability tokens. Even `paths_write: ["/**"]` cannot write to `/proc/agent/id`.

### Capability Gating

`/proc` access is gated by `paths_read` exactly like any other VFS path. No special capability — just path patterns:

- `paths_read: ["/workspace/**"]` — no `/proc` access
- `paths_read: ["/proc/budget/**"]` — budget only
- `paths_read: ["/proc/**"]` — full procfs access
- `paths_read: ["/**"]` — everything including procfs

Default tokens should include `/proc/**` in paths_read. Operators attenuate to restrict.

### Mailbox

`/proc/mailbox/` is writable. Writes delegate to the inner VFS. The host/operator reads mailbox contents after agent completion. The VFS IS the artifact system:

- Capability-gated (paths_write must include `/proc/mailbox/**`)
- Journaled (normal VFS write journal entries)
- Hook-visible (governance hooks see mailbox writes)
- Snapshot-safe (included in VFS snapshots)

### Crate Position

```
simulacra-types (VirtualFs trait, CapabilityToken, ResourceBudget)
  └→ simulacra-vfs (MemoryFs, OverlayFs, ProcFs)
       └→ simulacra-sandbox (AgentCell composes ProcFs + OverlayFs)
```

`ProcFs` lives in `simulacra-vfs`. It depends on types from `simulacra-types` (VirtualFs, CapabilityToken, ResourceBudget). It does NOT depend on simulacra-sandbox, simulacra-runtime, or simulacra-tool directly — it receives trait objects for ToolRegistry and HookRegistry.

### What This Replaces

Without procfs, agents would need:
- A `get_budget` tool → now: `read_file("/proc/budget/remaining_tokens")`
- A `list_tools` tool → now: `list_dir("/proc/tools/")`
- A `get_capabilities` tool → now: `read_file("/proc/capabilities/shell")`
- An artifact upload mechanism → now: `write_file("/proc/mailbox/report.md")`

Each of those would need its own capability check, journal entry, hook integration, and observability. Procfs gets all of that for free because it reuses VFS infrastructure.

### Future Extensions

- `/proc/signals/` — agent writes to trigger actions (checkpoint, pause, yield)
- `/proc/peers/` — read-only view of sibling agents (names, status)
- `/proc/metrics/` — agent-defined custom counters/gauges
- `/proc/env/` — read-only environment variables visible to the agent

All follow the same pattern: virtual files, read through existing tools, capability-gated, journaled, hook-visible.
