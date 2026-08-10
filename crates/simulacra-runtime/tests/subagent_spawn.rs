#![allow(clippy::type_complexity)]
#![cfg(feature = "spawn")]

include!("subagent_spawn/test_harness.rs");
include!("subagent_spawn/fixtures.rs");
include!("subagent_spawn/budget_limits.rs");
include!("subagent_spawn/child_history.rs");
include!("subagent_spawn/o11y_spans.rs");
include!("subagent_spawn/capability_budget_exit.rs");
include!("subagent_spawn/capability_override_paths.rs");
include!("subagent_spawn/generic_spawn.rs");
include!("subagent_spawn/tool_registry.rs");
include!("subagent_spawn/tier_model.rs");
include!("subagent_spawn/s060_slice1_spawn_contract.rs");
include!("subagent_spawn/s060_slice2_native_runtime.rs");
include!("subagent_spawn/s060_slice4_lifecycle.rs");
include!("subagent_spawn/s060_slice4_hooks.rs");
include!("subagent_spawn/s060_remediation_capability_hooks.rs");
include!("subagent_spawn/s060_hierarchical_budget_security.rs");
include!("subagent_spawn/s060_policy_outcomes.rs");
include!("subagent_spawn/s060_slice5_failure_privacy.rs");
