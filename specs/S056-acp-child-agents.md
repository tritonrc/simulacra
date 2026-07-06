# S056 - ACP Child Agents

**Status:** Active
**Crates involved:** `simulacra-config`, `simulacra-runtime`, `simulacra-cli`, `simulacra-server`

## Dependencies

- **ARCHITECTURE.md** - supervision model, Golden Rule boundary, ACP exception to local runtime assumptions
- **S006** - Resource budgets
- **S009** - Agent supervisor lifecycle and cancellation
- **S018** - parent-facing child spawn, join, cancel, and result flow
- **S019** - activity events
- **S054** - child status, bounded wait, and close tools

## Scope

Add ACP-backed child agents as an alternate child runtime behind the existing
`spawn_agent` / `create_agent` supervisor contract.

Simulacra treats ACP as the child-agent runtime boundary. For ACP children,
Simulacra does not inspect or mediate the child's execution location, sandbox,
tools, or filesystem. The embedding system supplies the ACP transport/session
implementation selected by `acp_profile`.

## Behavior

### Configuration

- [ ] Agent type config accepts an optional `backend` field.
- [ ] Omitted `backend` means the existing native Simulacra child runtime.
- [ ] `backend = "native"` explicitly selects the existing native Simulacra child runtime.
- [ ] `backend = "acp"` selects the ACP child runtime.
- [ ] Native agent types require a non-empty `model`.
- [ ] ACP agent types do not require `model`.
- [ ] ACP agent types require a non-empty `acp_profile`.
- [ ] Unknown `backend` values are rejected with an actionable config error.
- [ ] Existing native agent configs parse unchanged.

### Supervisor Contract

- [ ] ACP-backed children preserve the S018/S054 parent-facing tool contract:
  `spawn_agent`, `child_status`, `wait_child_agent`, `join_child_agent`,
  `cancel_child_agent`, and `close_child_agent` keep their names, schemas, and
  result shapes.
- [ ] The supervisor still owns accepted-child handles, metadata, budget
  reservation, journaling, status/wait/close behavior, cancellation handles,
  and terminal result caching.
- [ ] ACP child internals are not appended to the parent conversation except
  through the normal terminal summary returned by `join_child_agent` or terminal
  `wait_child_agent`.
- [ ] If an ACP child is spawned without an injected ACP runtime, spawn fails
  with an actionable error before any native child environment is built.
- [ ] ACP-backed spawns still run Simulacra spawn before/after hooks on
  Simulacra-owned metadata such as agent type, configured prompt metadata, and
  requested budget; hook execution must not inspect ACP child internals.

### ACP Runtime Port

- [ ] `simulacra-runtime` exposes an object-safe `AcpChildRuntime` port.
- [ ] The port starts an ACP child session from `child_id`, `parent_id`,
  `agent_type`, `acp_profile`, delegated `task`, requested `budget`, and
  effective `capability`.
- [ ] The ACP runtime receives the existing `CancellationToken`.
- [ ] ACP runtime implementations are responsible for observing cancellation
  and returning a cancelled terminal result when the ACP session is cancelled.
- [ ] The ACP runtime receives an `ActivitySink` that can forward
  protocol-visible ACP activity into the existing activity stream.
- [ ] The ACP runtime returns an `AgentLoopOutput`-compatible terminal result.
- [ ] ACP result usage is recorded only when the ACP runtime reports usage.
- [ ] ACP result tool counts are derived from the returned terminal output or
  activity-derived counts, not from prose parsing.

### VFS Independence

- [ ] ACP child execution does not require `VirtualFs`.
- [ ] ACP child execution does not call native child environment construction.
- [ ] ACP child execution does not construct an `AgentCell`.
- [ ] ACP child execution does not register native Simulacra tools for the child.
- [ ] ACP child execution does not require local sandbox inspection APIs.
- [ ] Simulacra must not require local filesystem mediation for ACP children.

## Non-Goals

- No built-in stdio, HTTP, or process ACP transport in v1.
- No ACP profile registry in Simulacra config beyond the opaque `acp_profile`
  string on agent types.
- No ACP-native file diffs, artifacts, or sandbox logs unless the ACP session
  reports them as protocol-visible terminal data.
- No changes to native child agent behavior.
