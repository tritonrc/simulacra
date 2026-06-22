# S006 — Resource Budgets

**Status:** Active
**Crate:** `simulacra-types`, `simulacra-runtime`

## Behavior

1. Every agent has a `ResourceBudget` assigned at creation.
2. Budget is checked **before** each operation, not after.
3. When a budget is exhausted, the operation returns a typed error (not a panic).
4. Budget counters: `max_tokens`, `max_turns`, `max_cost` (Decimal), `max_sub_agents`.
5. `check_budget()` returns `Ok(())` or `Err(BudgetExhausted { resource, used, limit })`.
6. Budget state is included in journal checkpoints for accurate replay.

## Assertions

- [x] `check_budget()` passes when under all limits. **Tested in simulacra-types budget module.**
- [x] `check_budget()` returns error when any single limit is exceeded. **Tested with all four resource types.**
- [x] Budget error includes which resource was exhausted and the current usage. **Tested in `check_budget_error_includes_resource_usage_and_limit_details`.**
- [x] Budget with limit of 0 means unlimited (not "already exhausted"). **Tested in `limit_zero_means_unlimited_not_already_exhausted`.**
- [x] Budget is checked before LLM call in the provider. **Tested in provider (`budget_exhausted_returns_error_without_http_call`) but not asserted as a budget spec assertion.**
- [x] Budget is checked before LLM call in the agent loop. **Tested in `exhausted_budget_returns_error_without_calling_provider`.**
- [x] Budget `used_turns` is incremented in the agent loop per turn. **Tested in `budget_used_turns_increments`.**
- [x] Budget state is serialized into checkpoint data. **Tested in `checkpoint_budget_snapshot_survives_serialize_deserialize_roundtrip`. Budget restored during replay in agent_loop.**
- [x] Child agent budget is deducted from parent budget. **Tested in `child_budget_deduction_increases_parent_used_tokens_turns_cost` and `multiple_child_deductions_accumulate_in_parent`.**

## Observability (see S010 for conventions)

- [x] Budget exhaustion is logged at `WARN` with resource name, used count, and limit. **Tested in `budget_exhaustion_is_logged_at_warn_with_resource_usage_and_limit`.**
- [x] `simulacra.agent.budget.remaining` gauge is updated after each budget-consuming operation. **Tested in `budget_remaining_gauge_is_updated_after_each_budget_consuming_operation`.**
- [x] Budget check failures produce an event on the current span with exhaustion details. **Tested in `budget_check_failures_emit_current_span_event_with_exhaustion_details`.**
