#![allow(clippy::type_complexity)]
#![cfg(feature = "spawn")]

include!("subagent_spawn/test_harness.rs");
include!("subagent_spawn/fixtures.rs");
include!("subagent_spawn/budget_limits.rs");
include!("subagent_spawn/child_history.rs");
include!("subagent_spawn/o11y_spans.rs");
include!("subagent_spawn/capability_budget_exit.rs");
include!("subagent_spawn/capability_override_paths.rs");
include!("subagent_spawn/child_cancellation.rs");
include!("subagent_spawn/tool_registry.rs");
include!("subagent_spawn/sandbox_prompt.rs");
include!("subagent_spawn/spawn_contract.rs");
include!("subagent_spawn/spawn_native_runtime.rs");
include!("subagent_spawn/spawn_lifecycle.rs");
include!("subagent_spawn/spawn_hooks.rs");
include!("subagent_spawn/spawn_capability_hook_remediation.rs");
include!("subagent_spawn/spawn_hierarchical_budget_security.rs");
include!("subagent_spawn/spawn_policy_outcomes.rs");
include!("subagent_spawn/spawn_failure_privacy.rs");
