#![allow(clippy::type_complexity)]
#![cfg(feature = "spawn")]

include!("subagent_spawn/test_harness.rs");
include!("subagent_spawn/fixtures.rs");
include!("subagent_spawn/budget_limits.rs");
include!("subagent_spawn/child_history.rs");
include!("subagent_spawn/o11y_spans.rs");
include!("subagent_spawn/capability_budget_exit.rs");
include!("subagent_spawn/generic_spawn.rs");
include!("subagent_spawn/tool_registry.rs");
include!("subagent_spawn/tier_model.rs");
