# S061 — Path-Shaped Child Ids from `task_name`

**Status:** Active — implemented
**Crates involved:** `simulacra-runtime`

## Dependencies

- **ARCHITECTURE.md** — supervisor ownership, opaque child identity crossing the
  tool/supervisor boundary
- **S009** — supervisor ownership; accepted child ids are never reused
- **S018** — supervised child handles and the spawn acknowledgement contract
- **S054** — child status, wait, join, and list result shapes
- **S060** — the current placement/instructions/task spawn contract this spec
  extends

## Why this spec exists

`spawn_agent` currently mints child ids as `child-{nanos:016x}{counter:016x}`
(`next_child_id` in `spawn_tool/tool.rs`). These ids are opaque to the model
and to every human reading a journal, roster, or observability line: a
coordinator that spawns "explore the codebase" reports success for
`child-000123abb04d2f9c0000000000000001`, which says nothing about the work.

Codex's multi-agent naming demonstrates the alternative: the model supplies a
snake_case `task_name`, and the child's canonical identity is the hierarchical
path composed from it (`/root/explore_codebase`). The id becomes a readable
summary of the delegation tree.

This spec adopts that model with a DevForge-neutral root segment (`forge`,
not `root`), an automatic slug fallback when the model omits the name, and
collision semantics that reuse the supervisor's existing accepted-id
rejection rather than adding a new registry at the supervisor boundary.

## Terminology

- **Root caller** — the agent whose `parent_id` is not itself a `/forge/...`
  path (the host-bound root of the supervision tree).
- **Descendant caller** — an agent whose own `AgentId` is a `/forge/...` path
  (any accepted child). Its spawns compose deeper paths.
- **Segment** — one `/`-delimited component of a path: the `task_name` value.
- **Auto-slug** — a segment derived from the `task` text when `task_name` is
  omitted.
- **Legacy fallback id** — the existing `child-{nanos:016x}{counter:016x}`
  mint, used only when an auto-slug cannot be derived.

## Model-facing contract

`spawn_agent` gains one optional argument:

- `task_name` — `string`. "Short snake_case name for this child, derived from
  the task; use lowercase letters, digits, and underscores (for example
  `explore_codebase`). The child's id becomes `/forge/<task_name>`."

The `required` array stays `["placement", "task", "budget"]`. Unknown keys
remain rejected, so `task_name` joins the accepted top-level key set.

## Path composition

- Root caller: child id = `/forge/<segment>`.
- Descendant caller: child id = `<parent-id>/<segment>` (the parent id is
  already a `/forge/...` path; the segment is appended with a single `/`).

The composed path is the `SpawnConfig.agent_id` and flows unchanged through
the acknowledgement (`child_id`), journal (`SubAgentSpawned`/`SubAgentCompleted`),
activity events, roster, and every child-control tool. Nothing downstream
parses the id; it remains opaque outside this spec's composition rule.

## Segment validation

A model-supplied `task_name` is **validated, never normalized**:

- must be a string; non-string values fail as `InvalidArguments`;
- must be non-blank;
- must contain only ASCII lowercase letters, digits, and `_`;
- must not contain `/`;
- must not equal the reserved segments `forge`, `.`, or `..`;
- must be at most 64 characters.

Every failure names `task_name` in the error so the model can retry.

## Auto-slug derivation

When `task_name` is omitted, a segment is derived from the `task` text:

1. lowercase (ASCII only; non-ASCII characters — including non-ASCII
   whitespace such as NBSP — are dropped);
2. every character outside `[a-z0-9_]` is dropped;
3. each run of ASCII whitespace becomes a single `_`;
4. leading and trailing `_` are trimmed;
5. the result is truncated to 32 characters (before any uniqueness suffix).

If the derived slug is empty, the spawn does not fail: the child receives a
legacy fallback id in the existing `child-{nanos:016x}{counter:016x}` format.

## Uniqueness and collisions

- **Model-supplied names are never auto-suffixed.** A duplicate path is
  rejected by the supervisor's existing accepted-id check
  (`ensure_child_id_is_new`); the tool propagates that error to the model,
  naming the colliding path. No new supervisor state is introduced.
- **Auto-slugs dedupe locally per parent.** A process-global registry keyed
  by `parent_id` records attempted auto-derived segments — including spawns
  the supervisor later rejects; a consumed local segment is never reused,
  which is harmless because rejected spawns mint no child and legacy ids
  remain available. A repeated auto-slug appends `_2`, `_3`, … (after
  truncation) before submission. Entries are never freed, matching the
  supervisor's never-reuse accepted-id semantics.
- If suffix probing is exhausted (`_2` through `_100`, 99 suffixed
  candidates after the base), the spawn falls back to the legacy id rather
  than looping.
- Different parents may independently derive the same auto-slug; cross-parent
  path collisions are the supervisor's rejection, not local suffixing.

## Non-goals

- No global (cross-conversation) name coordination.
- No name freeing or reuse after terminal settlement or close.
- No nickname pool ("Euclid") — only the path identity.
- No downstream consumer changes: journal, roster, activity events, and
  child-control tools treat the id as opaque.
- No normalization of model-supplied names (uppercase is rejected, not folded).

## Assertions

### Schema

- [x] The `spawn_agent` schema contains an optional `task_name` string
  property whose description directs lowercase letters, digits, and
  underscores; `required` remains exactly `["placement", "task", "budget"]`.
- [x] `spawn_agent` calls containing `task_name` pass shape validation;
  unknown top-level keys are still rejected.

### Model-supplied validation

- [x] A non-string `task_name` fails as `InvalidArguments` naming `task_name`.
- [x] Empty, whitespace-only, and blank `task_name` values fail naming
  `task_name`.
- [x] `task_name` values containing `/`, uppercase letters, hyphens, spaces,
  or any character outside `[a-z0-9_]` fail naming `task_name`.
- [x] `task_name` values `forge`, `.`, and `..` fail as reserved.
- [x] A 64-character `task_name` is accepted; a 65-character value fails.

### Path composition

- [x] A model-supplied `task_name` from a root caller produces
  `SpawnConfig.agent_id == "/forge/<task_name>"`, and the acknowledgement
  echoes that path as `child_id`.
- [x] A model-supplied `task_name` from a descendant caller whose own id is
  `/forge/first_phase` produces `"/forge/first_phase/<task_name>"`.
- [x] A valid `task_name` flows into journal, activity, and roster payloads
  unchanged (asserted through the existing spawn acknowledgement and captured
  `SpawnConfig`, with no re-minting of the id).

### Auto-slug

- [x] With `task_name` omitted, `"Explore the Codebase for DF-123"` derives
  the segment `explore_the_codebase_for_df123` and child id
  `/forge/explore_the_codebase_for_df123` for a root caller.
- [x] Whitespace runs collapse to a single `_`; non-`[a-z0-9_]` characters
  (including punctuation and non-ASCII) are dropped; leading/trailing `_`
  are trimmed; the slug is truncated to 32 characters before suffixing.
- [x] A `task` with no slug-able characters (for example `"???"`) yields a
  legacy fallback id matching `child-[0-9a-f]{32}`.
- [x] Two no-slug spawns under one parent produce two distinct legacy ids.

### Uniqueness

- [x] The same omitted-`task_name` slug spawned twice under one parent yields
  `/forge/<slug>` then `/forge/<slug>_2`; a third yields `_3`. Model-supplied
  names are never suffixed.
- [x] Two tools with different `parent_id`s deriving the same slug both
  submit the unsuffixed path (cross-parent collision handling belongs to the
  supervisor).
- [x] A model-supplied duplicate path surfaces the supervisor's
  already-accepted rejection to the caller as an `ExecutionFailed` error
  containing the duplicate path.

### Regression

- [x] Spawn calls without `task_name` under a unique parent id still succeed
  end-to-end through the fake-supervisor seam with a path-shaped or legacy
  id, and existing S060 spawn-contract behavior (placement validation, budget
  validation order before id minting) is unchanged.
