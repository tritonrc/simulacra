# S009 — Agent Supervisor

**Status:** Active
**Crate:** `simulacra-runtime`

## Behavior

1. Supervisor manages agent lifecycle: spawn, cancel, restart.
2. Message priority: signals > supervision > commands > work.
3. Restart strategies: `retry_once`, `retry_twice_then_fail`, `snapshot_and_fail`, `let_crash`.
4. Capability attenuation is enforced at spawn time by the supervisor.
5. Budget allocation for child agents is deducted from parent's budget.
6. Agent cancellation is cooperative — the agent receives a cancellation signal and has a grace period.

## Assertions

- [x] Supervisor enforces capability attenuation on spawn. **Behavioral test in `supervisor_enforces_capability_attenuation_on_spawn` — calls spawn_agent with wider-than-parent capabilities, asserts RuntimeError::CapabilityViolation.**
- [x] Restart strategy is applied on agent failure. **Behavioral test in `restart_strategy_is_applied_on_agent_failure` and `supervisor_restarts_failed_agent_via_actor_loop` — verifies handle_failure return values AND actual re-spawn through actor loop.**
- [x] Cancelled agent receives cancellation signal. **Behavioral test in `cancelled_agent_receives_cancellation_signal` — spawns a real tokio task, cancels it, verifies it observes the signal.**
- [x] Child budget does not exceed parent budget. **Behavioral test in `child_budget_does_not_exceed_parent_budget` — calls spawn_agent with child budget exceeding parent's remaining tokens, asserts RuntimeError::BudgetExhausted.**
- [x] Message priority ordering (signals > supervision > commands > work) is enforced. **Behavioral test in `supervisor_actor_loop_processes_messages_by_priority` — sends Signal, Command, Work simultaneously, verifies dispatch order.**
- [x] `retry_once` strategy restarts the agent exactly once then fails. **Tested in `retry_once_restarts_exactly_once_then_fails`.**
- [x] `retry_twice_then_fail` strategy restarts the agent at most twice. **Tested in `retry_twice_then_fail_restarts_at_most_twice`.**
- [x] `snapshot_and_fail` saves journal snapshot before propagating failure. **Calls save_checkpoint_for_snapshot before returning. Tested in `snapshot_and_fail_saves_journal_snapshot_before_propagating_failure`.**
- [x] `let_crash` does not restart the agent. **Tested in `let_crash_does_not_restart`.**
- [x] Agent cancellation has a grace period before forceful termination. **Implemented with tokio::time::timeout and AbortHandle. Tested in `cancellation_has_a_grace_period_before_forceful_termination`.**
- [x] Supervisor can manage multiple concurrent agents. **Behavioral test in `supervisor_manages_multiple_concurrent_agents` — spawns 3 agents, cancels 1, verifies the other 2 continue.**
- [x] Supervisor is actor-style on raw tokio (no framework dependency) per ARCHITECTURE.md. **run_actor_loop with tokio::select! and mpsc. Tested via `supervisor_actor_loop_processes_messages_by_priority`.**
- [x] Child agents are `tokio::spawn` tasks communicating via `mpsc`. **Behavioral test in `child_agents_communicate_via_mpsc` — child completion flows back and rolls up budget.**

## Observability (see S010 for conventions)

- [x] Agent spawn produces a span with `gen_ai.operation.name` = `create_agent` and `gen_ai.agent.name`. **Behavioral test in `agent_spawn_produces_create_agent_span_with_agent_name` — captures tracing spans.**
- [x] Agent invocation is wrapped in a span with `gen_ai.operation.name` = `invoke_agent`. **Tested behaviorally in `agent_invocation_is_wrapped_in_invoke_agent_span`.**
- [x] `simulacra.agent.turns` counter tracks turns per agent. **Tested behaviorally in `simulacra_agent_turns_counter_tracks_turns_per_agent`.**
- [x] Agent spawn is logged at `INFO` with agent name, parent, and capabilities. **Behavioral test in `agent_spawn_is_logged_at_info_with_agent_name_parent_and_capabilities` — captures tracing events.**
- [x] Agent completion is logged at `INFO` with agent name, exit reason, and token total. **Tested behaviorally in `agent_completion_is_logged_at_info_with_agent_name_exit_reason_and_token_total`.**
- [x] Agent restart is logged at `WARN` with agent name, strategy, and failure reason. **Behavioral test in `agent_restart_is_logged_at_warn_with_agent_name_strategy_and_failure_reason` — captures tracing events.**
