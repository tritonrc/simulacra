include!("agent_supervisor/test_harness.rs");
include!("agent_supervisor/fixtures.rs");
include!("agent_supervisor/capability_and_restart.rs");
include!("agent_supervisor/actor_loop.rs");
include!("agent_supervisor/actor_join_and_retry.rs");
include!("agent_supervisor/result_delivered.rs");
#[cfg(feature = "spawn")]
include!("agent_supervisor/s060_slice4_observability.rs");
include!("agent_supervisor/s060_identity_security.rs");
