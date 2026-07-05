# Parent/Child CLI Playground

This demo makes the parent behave like a small code-agent lead: immediately
delegate independent source and test inspection to two configured children,
keep doing parent-side spec analysis while they run, then join and reconcile
the findings into a concrete patch-plan artifact.

Run from the repo root:

```bash
target/debug/simulacra --config demos/parent-child/simulacra.toml --mode interactive --no-catalog
```

Headless smoke run with local OTLP:

```bash
target/debug/simulacra --config demos/parent-child/simulacra.toml --mode headless --no-catalog --otlp-endpoint http://localhost:4320
```

Structured headless smoke run:

```bash
target/debug/simulacra --config demos/parent-child/simulacra.toml --mode headless --no-catalog --otlp-endpoint http://localhost:4320 --output-format jsonl
```

The headless task is embedded in `simulacra.toml` and should exercise:
parallel `spawn_agent`, parent-side file inspection, `child_status`,
wait-any polling via `wait_child_agent`, `join_child_agent`, handle cleanup via
`close_child_agent`, and a write to `/workspace/tmp/parent-child-code-agent-plan.md`.
The demo agents use built-in file and child-control tools, with shell and
JavaScript enabled for small targeted searches and lightweight local analysis.
Python and network access are disabled.

Useful smoke-test prompt:

```text
Act like a code-agent lead validating the child-agent orchestration implementation. Spawn two children in parallel: a researcher to inspect the runtime/CLI source paths for how spawn_agent, child_status, wait_child_agent, join_child_agent, steer_child_agent, and close_child_agent are wired, and a reviewer to inspect the mounted subagent_spawn tests for missing behavioral coverage or brittle assertions. After both spawns return handles, do your own parent-side scan of /workspace/specs/S054-child-agent-orchestration.md and /workspace/specs/S018-interactive-subagents.md while the children run. Use child_status on both handles, then wait_child_agent with child_ids and timeout_ms 0 to poll once without consuming results. Join both children, compare their findings with your parent-side spec scan, close both child handles, and write /workspace/tmp/parent-child-code-agent-plan.md with a concise patch plan: confirmed behavior, suspected gaps, exact files to edit, and tests to run.
```
