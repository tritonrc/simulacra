# AGENTS.md — Simulacra

`CLAUDE.md` is a symlink to this file. Both point to the same content.

## Two Kinds of Truth

| Kind | What | Where | Governs |
|---|---|---|---|
| **Process** | How we code/work | `AGENTS.md`, `rules/` | Workflow, methodology, development standards |
| **System** | What gets built | `ARCHITECTURE.md`, `specs/` | Invariants, features, acceptance criteria |

Process docs tell you *how* to work. System docs tell you *what* to build.
Never derive process from specs. Never derive system behavior from rules.

**Authority:** `ARCHITECTURE.md` > `specs/` > `rules/` > agent judgment.
If specs are ambiguous, update the specs (do not invent behavior).

## The Protocol

Every implementation task follows this protocol. No exceptions. No shortcuts.

### Phase 0: Scope

1. One task = one spec (or one bug, one feature). Never combine.
2. Load this file → `ARCHITECTURE.md` → the specific spec → relevant `rules/`.
3. If the task touches multiple crates, list them explicitly before starting.
4. Sub-agents get ONE task each. A sub-agent that writes tests must not write implementation.

### Phase 1: Red (Tests)

Three steps, each in a **sub-agent** to keep the top-level context clean.

**Step 1a — Draft tests.** `codex exec --model gpt-5.5` writes the first pass.

```bash
codex exec --model gpt-5.5 --full-auto --cd . "Write failing tests for spec S00N. \
  Read specs/S00N-*.md for assertions. \
  Read rules/R004-test-against-fakes.md for test patterns. \
  Read the existing code in crates/simulacra-<crate>/src/. \
  Every '- [ ]' assertion in the spec MUST have a corresponding #[test]. \
  Tests must compile but FAIL. Output only Rust code." < /dev/null
```
> `codex exec` hangs unless stdin is closed (`< /dev/null`). macOS has no `timeout`; for long runs write output to a file, not a tail pipe.

**Step 1b — Review tests.** A Claude **sub-agent** reviews the draft tests with heavy emphasis on:
- Behavioral coverage (every spec assertion has a test that exercises real code paths)
- Edge cases (empty inputs, boundary values, error paths, concurrent access)
- No source-scanning tests (see `rules/R004-test-against-fakes.md`)
- Test isolation (no shared mutable state between tests)
- Correct use of fakes (`MemoryFs`, `InMemoryJournalStorage`, recorded fixtures)

**Step 1c — Reconcile.** A Claude **sub-agent** reconciles the review feedback:
- Adds missing edge-case tests identified in 1b
- Fixes any test-quality issues
- Removes any tests that verify comments/strings in source rather than behavior
- Ensures every test compiles and fails for the right reason

**Gate:** Tests compile. Tests fail. If tests pass without implementation, the tests are wrong.

### Phase 2: Green (Implementation)

**Who:** Claude Code **sub-agents**, one per task.

Break the failing tests into logical task groups (by module, by feature area, or by crate).
Each sub-agent receives:
- The spec
- Its subset of failing tests
- The relevant rules

Each sub-agent implements until its tests pass. It does NOT write new tests — it makes the red tests green.

After all sub-agents complete, verify the full test suite passes together. Fix any integration conflicts between sub-agent outputs.

After tests pass, the sub-agent queries the local Aniani instance to verify o11y assertions from the spec (see `rules/R010-observability-validation.md`). Traces, metrics, and logs are validated via TraceQL, PromQL, and LogQL — not just unit tests.

**Gate:** `cargo test -p simulacra-<crate>` — all tests pass. Aniani queries confirm o11y assertions.

### Phase 3: Integration & Mechanical Checks

Two steps:

**Step 3a — E2E smoke tests.** A Claude **sub-agent** verifies the implementation works end-to-end, not just unit-by-unit. The weakness in the current system is that individual units are coherent but not always strung together correctly. The sub-agent should:
- Write or run integration tests that exercise the full bootstrap → agent loop → tool call path
- Verify config parsing → VFS setup → capability enforcement → tool execution chain
- Check that cross-crate boundaries work (e.g., config types flow through CLI into runtime)
- Run `cargo test --workspace` and investigate any failures

**Step 3b — Mechanical gate (non-negotiable):**
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four must pass. The compiler + clippy are the first reviewers.

### Phase 4: Review

Three independent reviewers, **all run as sub-agents or background tasks**:

**4a — Codex (gpt-5.5), spec-compliance lens:**
```bash
codex exec --model gpt-5.5 --cd . "Review the changes in \
  crates/simulacra-<crate>/. Read the spec (specs/S00N-*.md), ARCHITECTURE.md, and rules/. \
  Classify issues as BLOCKER (must fix) or WARNING (should fix) or NIT (optional). \
  Focus on: spec compliance, journal completeness, capability enforcement, \
  no invented behavior, test coverage of every spec assertion." < /dev/null
```

**4b — Codex (gpt-5.5), edge-case/security lens:**
```bash
codex exec --model gpt-5.5 --cd . "Review the changes in \
  crates/simulacra-<crate>/. Read the spec (specs/S00N-*.md), ARCHITECTURE.md, and rules/. \
  Classify issues as BLOCKER (must fix) or WARNING (should fix) or NIT (optional). \
  Focus on: edge cases, error handling, security (capability bypasses, path traversal), \
  and whether the tests actually verify what they claim to verify." < /dev/null
```

**4c — Claude sub-agent:** Reviews the overall work holistically:
- Do the pieces fit together?
- Are there gaps between what the spec says and what was built?
- Is the implementation minimal and focused, or was anything over-engineered?
- Would a new contributor understand this code?

**Gate:** Zero BLOCKERs across all three reviews. Fix BLOCKERs and re-run Phase 3 + Phase 4. WARNINGs are fix-or-justify.

### Phase 5: Commit

Commit message format: `feat(<crate>): <what> [S00N]`

Only after all gates pass.

## Enforcement

These rules are not guidelines. They are the protocol.

- If Claude writes tests instead of shelling to `codex exec --model gpt-5.5` for the initial draft, the tests are invalid. Delete and redo Phase 1a.
- If a sub-agent handles both tests and implementation, the adversarial boundary is violated. Restart.
- If review is skipped or done by the implementing agent, the review is invalid. Redo Phase 4.
- If any of the 4 mechanical checks fail, do not proceed to review.
- Use sub-agents aggressively. The top-level context is for orchestration, not for doing the work. Sub-agents keep context clean and enable parallelism.

## Where Truth Lives

| What | Where |
|---|---|
| System invariants + architectural positions | `ARCHITECTURE.md` |
| Full design + diagrams | `docs/simulacra-design.md` |
| Feature specs + acceptance criteria | `specs/` (indexed by `SPECS.md`) |
| Process rules (TDD, review, scope) | `rules/` |
| O11y validation | Local Aniani instance (PromQL, LogQL, TraceQL) |

## When You Get Stuck

If a task is ambiguous or a spec doesn't cover the case:

1. Do not invent behavior.
2. Check `ARCHITECTURE.md` for the governing principle.
3. Check `specs/` for the closest behavioral spec.
4. If still ambiguous, **update the spec or add a rule** — the repo is the system of record, not conversation history.
5. If a decision defines a hard constraint on how we code, add a `rules/RNNN-*.md` rule.
