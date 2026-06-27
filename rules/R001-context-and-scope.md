# R001 — Context and Scope

## Rule

One task per context window. One concern per change. One sub-agent per phase.

### One Task Per Context

A context window is a fixed-size allocation. Once mixed, it cannot be cleanly recovered.

1. One task per session. If you switch tasks, restart with a fresh context.
2. Re-load the stack on restart: `AGENTS.md` → `ARCHITECTURE.md` → spec → rules.
3. If outputs degrade (autoregressive drift), stop and restart with a smaller context.

### What Counts As A Task

A task has a coherent success condition tied to ONE spec or ONE bug or ONE feature.

Good tasks:
- "Implement S005 journal replay"
- "Fix budget check where 0 should mean unlimited"
- "Add OTel spans to simulacra-provider [S010]"

Not tasks (too large, must be split):
- "Implement S005 and S010" — two specs, two tasks
- "Make Simulacra production-ready" — unbounded scope

### One Sub-Agent Per Phase

Sub-agents are scoped to a single phase of the protocol (see AGENTS.md § The Protocol):

| Phase | Sub-agent scope |
|---|---|
| Red | NOT a sub-agent — shell to `codex exec --model gpt-5.5 --full-auto --cd . "..." < /dev/null` |
| Green | One Claude sub-agent per spec. Receives failing tests + spec. Writes implementation only. |
| Review | NOT a sub-agent — shell to `codex exec --model gpt-5.5 --cd . "..." < /dev/null` |

> `codex exec` hangs unless stdin is closed (`< /dev/null`); macOS has no `timeout`.

A sub-agent that writes tests AND implementation violates the adversarial boundary. The whole point of the protocol is that the test author and the implementer are different: Codex (gpt-5.5) writes tests and reviews, Claude implements, and the human remains the architectural authority.

### Stay In Your Crate

- If the task targets `simulacra-vfs`, do not modify `simulacra-shell` or `simulacra-runtime`.
- If you need a type from another crate, import it — do not duplicate it.
- If the task requires cross-crate changes, list every crate before starting.

### One Concern Per Change

- A commit addresses one spec, one bug, or one feature.
- Do not refactor adjacent code while fixing a bug.
- Do not add features while refactoring.

### When Scope Is Unclear

- Check the relevant spec. If the spec covers it, follow the spec.
- If the spec doesn't cover it, update the spec first — then implement.
- Do not invent behavior.

## Why

Context windows are finite. Loading irrelevant files wastes tokens, increases unintended changes, and makes review harder. Scoped changes are easier to test, review, and revert. Separating phases across Codex (gpt-5.5), Claude sub-agents, and human architectural review preserves the adversarial diversity that makes the protocol work.
