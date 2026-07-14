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

### Steer Delivery

- [ ] `AcpChildRuntime::start_child` receives the child's `AgentInputQueue` so
  parent steer messages (`steer_child_agent` → `SupervisorPayload::SteerChild`)
  can reach the ACP session; the port has no default that silently drops the
  queue.
- [ ] `AgentInputQueue` exposes a public async `recv()` returning
  `Option<String>`: queued messages in enqueue order, `None` once every
  `ChildInputHandle` has dropped.
- [ ] The ACP factory branch passes the supervisor-registered input queue —
  the same queue whose `ChildInputHandle` the supervisor holds in
  `child_inputs` — to the injected ACP runtime, so a steer against a live ACP
  child feeds the runtime rather than an undrained queue.
- [ ] Native child steering is unchanged: `AgentLoop` still drains the queue
  between model turns.
- [ ] Delivery timing, retry, and readiness policy are the embedding's
  responsibility; Simulacra guarantees only that the queue handed to the port
  is the live steer source for that child.
- [ ] Cancel wins: the supervisor may still accept steers for a child whose
  cancellation has begun (the input sender lives until the child task ends);
  ACP runtime implementations must stop consuming the queue once they observe
  cancellation or produce a terminal result, discarding undelivered messages.

### VFS Independence

- [ ] ACP child execution does not require `VirtualFs`.
- [ ] ACP child execution does not call native child environment construction.
- [ ] ACP child execution does not construct an `AgentCell`.
- [ ] ACP child execution does not register native Simulacra tools for the child.
- [ ] ACP child execution does not require local sandbox inspection APIs.
- [ ] Simulacra must not require local filesystem mediation for ACP children.

### Known limitation (pre-existing, out of scope here)

- The supervisor retry path (`run_task_with_retries`, strategies `RetryOnce` /
  `RetryTwiceThenFail`) rebuilds the child via `TaskFactory::create_task`,
  which mints a fresh input queue (handle dropped) and a fresh cancellation
  token that is never registered — a retried child (native or ACP) is
  therefore neither steerable nor cancellable through the supervisor's
  retained handles. Unreachable from the `spawn_agent` tool path, which
  hard-codes `RestartStrategy::LetCrash`. Fixing retry re-wiring (queue +
  token swap per attempt) is its own follow-up task.

## Non-Goals

- No built-in stdio, HTTP, or process ACP transport in v1.
- No ACP profile registry in Simulacra config beyond the opaque `acp_profile`
  string on agent types.
- No ACP-native file diffs, artifacts, or sandbox logs unless the ACP session
  reports them as protocol-visible terminal data.
- No changes to native child agent behavior.
