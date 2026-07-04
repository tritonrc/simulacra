# S052 — Workflow Runtime

**Status:** Active
**Crates involved:** `simulacra-types`, `simulacra-runtime`, `simulacra-workflow`, `simulacra-server`

## Context

S049-S051 make one `AgentLoop` run cancellable, streamable, and resumable
around HITL waits. S052 adds a separate orchestration layer above individual
agent runs. A workflow is a persisted, restricted ESM script that coordinates
many agent workers through phases, fan-out, joins, progress, and cached resume.
Workflows execute on Simulacra's shared QuickJS runtime with a restricted host
API profile; S052 does not introduce a separate workflow parser or interpreter.
S053 owns the shared async QuickJS substrate.

The workflow runtime coordinates only. It does not gain direct filesystem,
shell, fetch, process, or tool powers. All real side effects still happen inside
normal agents through `AgentLoop`, `AgentCell`, capabilities, budgets, journals,
HITL, and observability.

## Behavior

1. Simulacra provides a new `simulacra-workflow` crate for workflow
   orchestration. `simulacra-runtime` remains the owner of a single agent run.
2. Workflow scripts are ESM text and must export `meta` with non-empty `name`
   and `description` strings.
3. Workflow scripts execute in QuickJS using a workflow host API profile that
   exposes only orchestration helpers:
   `agent()`, `parallel()`, `pipeline()`, `phase()`, and `progress()`.
4. Workflow scripts cannot directly access filesystem, shell, fetch, normal
   tools, host environment, process APIs, or the regular `simulacra:*` QuickJS
   modules.
5. Workflow scripts cannot use nondeterministic time/random APIs:
   `Date`, `Date.now`, `new Date`, `Math.random`, or `performance.now`.
6. Inline workflow scripts are persisted through the configured VFS before
   execution under `/var/workflows/runs/<run_id>/workflow.mjs`.
7. Workflow run state is persisted through VFS under
   `/var/workflows/runs/<run_id>/state.json`.
8. Per-agent workflow transcripts/results are persisted through VFS under
   `/var/workflows/runs/<run_id>/agents/<label>.json`.
9. Saved reusable workflows resolve from `/workflows/<name>.mjs`.
10. Workflow script paths reject traversal, host-absolute paths, NUL bytes, and
    non-`.mjs` paths. Valid paths are rooted in `/workflows/` or
    `/var/workflows/runs/`.
11. Workflow workers execute through a `WorkflowWorker` abstraction that invokes
    normal `AgentLoop` behavior in production and fakes in tests.
12. Worker calls are described by `WorkflowAgentCall` and produce
    `WorkflowAgentResult`.
13. `parallel()` honors a bounded concurrency limit and returns results in input
    order.
14. `phase()` emits phase start/finish events. `progress()` emits progress
    events without changing workflow result semantics.
15. Workflow cancellation marks the run cancelled and signals active workers
    through existing cancellation mechanisms.
16. A resumed workflow may reuse completed worker calls from a previous run when
    the stable call key matches. Changed, missing, failed, or cancelled calls
    rerun.
17. Workflow resume is orchestration-level caching. It does not replace
    `AgentLoop` journal replay.
18. `ActivityEvent` includes workflow lifecycle events for start, phase
    start/finish, agent start/finish, progress, cancellation, failure, and
    completion.
19. Server SSE maps workflow lifecycle events to `workflow.*` JSON events with
    `run_id`, sequence number, status, phase, and agent label where applicable.
20. The server exposes workflow start, status, events, cancel, and resume routes
    under `/api/v1/workflows`.
21. Simulacra exposes a model-visible `Workflow` tool when workflow capability
    is available.
22. `Workflow` tool input accepts `script`, `name`, `script_path`, `args`, and
    `resume_from_run_id`. At least one of `script`, `name`, or `script_path` is
    required. `script_path` takes precedence over inline `script`.
23. The `Workflow` tool starts the workflow and returns promptly with `run_id`,
    `status`, `script_path`, and `transcript_dir`. It does not block the agent
    turn until workflow completion.
24. HITL waits from worker agents surface through workflow status/events and are
    resumed through the existing S051 input/approval channels.

## Assertions

- [ ] `simulacra-workflow` exists as a separate crate and does not move
  single-agent turn behavior out of `simulacra-runtime`.
- [ ] Workflow scripts require `export const meta` or `export let meta` with
  non-empty `name` and `description`.
- [ ] Scripts execute as QuickJS ESM and can run simple `agent()`,
  `parallel()`, `pipeline()`, `phase()`, and `progress()` orchestration without
  direct side-effect APIs.
- [ ] The workflow surface is enforced by configurable QuickJS host API
  exposure, not by a separate parser/interpreter for workflow JavaScript.
- [ ] Restricted filesystem, shell, fetch, process, `simulacra:*`, time, and
  random APIs fail before worker execution.
- [ ] Inline scripts persist to `/var/workflows/runs/<run_id>/workflow.mjs`.
- [ ] Run state persists to `/var/workflows/runs/<run_id>/state.json`.
- [ ] Worker results persist to
  `/var/workflows/runs/<run_id>/agents/<label>.json`.
- [ ] `script_path` validation accepts VFS workflow paths and rejects traversal,
  host-absolute, NUL, and non-`.mjs` paths.
- [ ] `parallel()` respects the configured concurrency limit and returns results
  in input order.
- [ ] Workflow cancellation cancels active workers and records a cancelled run
  state without fabricating successful worker results.
- [ ] Resume reuses matching completed worker results and reruns changed,
  missing, failed, or cancelled calls.
- [ ] Workflow activity events serialize and server SSE maps them to
  `workflow.*` events.
- [ ] Workflow start/status/events/cancel/resume server routes enforce existing
  tenant ownership and event replay behavior.
- [ ] `Workflow` tool is model-visible when registered, validates input
  precedence, starts a run, and returns promptly with run metadata.
- [ ] Worker execution uses the existing `AgentLoop`/worker boundary so side
  effects still journal through the normal runtime path.

## Out of Scope

- Frontend workflow visualization beyond receiving server events.
- Remote workflow registries, marketplace distribution, hosted provider tools,
  or host process execution.
- Durable process-restart recovery beyond VFS run-state persistence.
- A general-purpose JavaScript runtime for workflows. The v1 script surface is
  restricted to workflow orchestration.
- A custom restricted parser/interpreter for workflow JavaScript.
