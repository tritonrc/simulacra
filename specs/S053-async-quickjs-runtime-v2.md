# S053 - Async QuickJS Runtime V2

**Status:** Active

**Crates involved:** `simulacra-quickjs`, `simulacra-sandbox`, `simulacra-workflow`

## Summary

S053 upgrades Simulacra's QuickJS substrate so general `js_exec` and workflow
scripts share one async-capable runtime implementation. The runtime remains
VFS-first and Golden-Rule-unaware: all side effects still route through
`AgentCell` proxies for capability checks, budget checks, journaling, and
observability.

This spec replaces the old sync-only runtime constraint. It preserves the
current fresh-per-eval behavior: each `eval` gets a fresh QuickJS
runtime/context, while the `JsRuntime` wrapper persists host configuration and
remote module source caches.

## Behavior

1. `simulacra-quickjs` exposes a shared async evaluation substrate backed by
   `rquickjs::AsyncRuntime` and `rquickjs::AsyncContext`.
2. `JsRuntime::eval` remains as a compatibility wrapper for synchronous callers.
3. `JsRuntime` also exposes an async evaluation API for async callers, including
   workflow execution.
4. Both sync and async exported values are supported. Promise-returning script or
   module results are awaited before `JsOutput` is returned.
5. Rejected JavaScript promises surface as `JsError::Execution` with the
   rejection message.
6. Unresolved promises and CPU-bound loops are bounded by the runtime timeout.
7. Each evaluation uses a fresh global object, fresh prototypes, and fresh module
   instances. JS state does not leak across `eval` calls.
8. Host configuration, runtime limits, and remote module source caches persist on
   the `JsRuntime` wrapper across fresh eval contexts.
9. Host APIs are selected by an explicit host profile. The full profile preserves
   the `js_exec` surface; the workflow profile exposes only workflow globals.
10. Host functions that cross a host boundary return Promise-compatible behavior
    to JavaScript. Underlying operations still delegate through `AgentCell`
    proxies where side effects happen.
11. Remote ESM imports are prefetched before evaluation by walking static import
    specifiers. The QuickJS loader then serves only built-ins, VFS modules, or
    prefetched remote sources and performs no network I/O itself.
12. Dynamic remote imports that were not reached by static prefetch fail closed
    with a clear module-loading error.
13. Remote module fetches continue through `AgentCellModuleFetcher` and therefore
    preserve network capability checks, budget checks, `HttpRequest` journaling,
    `module_fetch` spans, and fetch counters.
14. Synchronous remote-module prefetch work is bounded by the same public eval
    timeout as JavaScript execution. Timeout returns may abandon non-abortable
    blocking host work, but must not leave the eval call waiting on it or allow
    late cache population for that timed-out eval.
15. TypeScript transformation is out of scope. Imported sources must be valid
    JavaScript ESM after fetch/read.
16. Workflow runtime uses the shared QuickJS substrate with the workflow host
    profile. It does not maintain a separate QuickJS harness.
17. Workflow scripts remain unable to access filesystem, shell, fetch, process,
    time, random, or `simulacra:*` APIs directly.
18. Workflow `agent()` waits are governed by S052 workflow cancellation rather
    than the ordinary five-second QuickJS eval timeout; long-running worker
    futures must not be detached and continue after workflow cancellation.
19. `rquickjs-serde` is used for typed cross-boundary conversion where practical;
    stringified JSON bridges are avoided for new runtime-facing APIs.

## Assertions

- [x] `JsRuntime::eval_async` awaits an async script result and returns the
  resolved value. **Tested in `eval_async_awaits_async_script_result`
  (`simulacra-quickjs`).**
- [x] `JsRuntime::eval` remains available and delegates through the same
  evaluation semantics as the async API. **Tested in
  `eval_sync_uses_the_same_promise_resolution_semantics_as_eval_async`
  (`simulacra-quickjs`).**
- [x] Rejected promises return `JsError::Execution` containing the rejection
  message. **Tested in
  `rejected_promises_surface_as_execution_errors_with_the_rejection_message`
  (`simulacra-quickjs`),
  `rejected_module_result_promises_surface_as_execution_errors`
  (`simulacra-quickjs`), and
  `workflow_async_rejections_surface_the_javascript_rejection_message`
  (`simulacra-workflow`).**
- [x] Promise-returning ESM module results are awaited before `JsOutput` is
  returned. **Tested in
  `promise_returning_module_results_are_awaited_before_returning_output`
  (`simulacra-quickjs`).**
- [x] Unresolved promises time out with a runtime timeout error. **Tested in
  `eval_async_times_out_unresolved_promises` (`simulacra-quickjs`).**
- [x] CPU-bound loops are interrupted by the runtime timeout. **Tested in
  `runtime_timeout_interrupts_cpu_bound_loops` (`simulacra-quickjs`).**
- [x] Synchronous remote module prefetch is bounded by the eval timeout for both
  async and sync callers. **Tested in
  `eval_async_timeout_bounds_synchronous_remote_module_prefetch` and
  `eval_sync_timeout_bounds_synchronous_remote_module_prefetch`
  (`simulacra-quickjs`).**
- [x] Remote module fetches that complete after an eval timeout do not populate
  the shared source cache for later evals. **Tested in
  `timed_out_prefetch_does_not_populate_remote_source_cache_later`
  (`simulacra-quickjs`).**
- [x] JS globals and prototype mutations do not persist across eval calls.
  **Tested by the existing S003/S011 fresh-eval tests and by
  `remote_module_cache_persists_while_module_instances_stay_fresh_per_eval`
  (`simulacra-quickjs`).**
- [x] Remote static ESM imports are prefetched before module evaluation.
  **Tested in `remote_static_imports_prefetch_transitive_sources_before_module_evaluation`
  and `multiline_static_imports_are_prefetched_before_module_evaluation`
  (`simulacra-quickjs`).**
- [x] A transitive remote static import is prefetched and served by the loader.
  **Tested in `remote_static_imports_prefetch_transitive_sources_before_module_evaluation`
  (`simulacra-quickjs`) and
  `static_remote_module_prefetch_uses_agent_cell_module_fetcher_for_transitive_imports`
  (`simulacra-sandbox`).**
- [x] Dynamic remote imports that were not prefetched fail closed. **Tested in
  `dynamic_remote_imports_that_were_not_static_prefetched_fail_closed`
  (`simulacra-quickjs`) and
  `dynamic_remote_imports_fail_closed_before_agent_cell_module_fetch`
  (`simulacra-sandbox`).**
- [x] Remote module fetches use the `AgentCellModuleFetcher` path and preserve
  capability, budget, journal, and span behavior. **Tested in
  `static_remote_module_prefetch_uses_agent_cell_module_fetcher_for_transitive_imports`
  (`simulacra-sandbox`) plus the existing ESM module-fetch Golden Rule tests.**
- [x] Workflow execution uses the shared `simulacra-quickjs` async substrate.
  **Tested in
  `workflow_agent_host_function_returns_a_promise_resolving_to_typed_json`
  (`simulacra-workflow`).**
- [x] Workflow host profile keeps restricted APIs unavailable. **Tested in
  `workflow_host_profile_removes_restricted_apis_from_the_shared_runtime`
  (`simulacra-quickjs`),
  `workflow_host_profile_does_not_prefetch_remote_static_imports`
  (`simulacra-quickjs`),
  `workflow_shared_profile_keeps_restricted_apis_unavailable_during_execution`
  (`simulacra-workflow`), and
  `workflow_static_simulacra_imports_are_rejected_before_worker_execution`
  (`simulacra-workflow`).**
- [x] Workflow `agent()` waits can exceed the ordinary QuickJS eval timeout
  without failing or detaching worker futures. **Tested in
  `workflow_agent_calls_are_not_limited_by_the_default_quickjs_eval_timeout`
  (`simulacra-workflow`) and the existing S052 cancellation test
  `cancellation_marks_run_cancelled_and_does_not_fabricate_successes`.**
- [x] Existing `js_exec` tool behavior remains compatible for successful output
  and JS exceptions. **Tested in
  `js_exec_success_output_and_javascript_exceptions_remain_compatible`
  (`simulacra-sandbox`).**

## Out of Scope

- TypeScript parsing or transformation.
- Remote module lockfiles or content-addressed cache pinning.
- General-purpose workflow host APIs beyond S052.
- Persistent JavaScript global/module state across tool calls.
