//! Tests for SimulacraEngine construction (S034 assertions: engine construction).

use std::collections::HashMap;
use std::sync::Arc;

use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, HooksConfig, ProjectConfig,
    SimulacraConfig, TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::SimulacraEngine;

fn valid_engine_config() -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "ollama:llama3".to_string(),
            acp_profile: None,
            system_prompt: Some("You are the worker.".to_string()),
            skills: vec![],
            max_turns: Some(8),
            max_tokens: Some(4_096),
            max_sub_agents: Some(2),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: true,
                javascript: false,
                python: false,
                paths_read: vec!["/**".to_string()],
                paths_write: vec!["/workspace/**".to_string()],

                skill_patterns: vec![],

                memory: None,
            }),
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        "acme".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-engine-tests".to_string(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants,
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

#[tokio::test]
async fn simulacra_engine_new_returns_ok_with_valid_config_and_no_integration_registry() {
    let config = valid_engine_config();
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None).await;
    assert!(
        engine.is_ok(),
        "valid config without integrations should construct the engine"
    );
}

#[tokio::test]
async fn simulacra_engine_new_returns_ok_with_valid_config_and_integration_registry() {
    let config = valid_engine_config();
    // Build a real (empty) IntegrationRegistry.
    let registry = simulacra_integration::IntegrationRegistry::from_config(&HashMap::new())
        .expect("empty registry should succeed");
    let engine =
        SimulacraEngine::new_with_in_memory_catalog(config, Some(Arc::new(registry))).await;
    assert!(
        engine.is_ok(),
        "valid config with a registry should construct the engine"
    );
}

// S042: SimulacraEngine no longer validates that tenants reference known agent
// types at construction time. Agent existence is enforced at spawn_task
// time against the catalog (covered by engine_catalog.rs).
#[tokio::test]
async fn simulacra_engine_new_does_not_reject_tenant_referencing_unknown_agent_type() {
    let mut config = valid_engine_config();
    config.tenants.insert(
        "broken".to_string(),
        SimulacraTenantConfig {
            agent_type: "missing-agent".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None).await;
    assert!(
        engine.is_ok(),
        "engine construction must not validate tenant agent types post-S042"
    );
}

#[tokio::test]
async fn simulacra_engine_new_accepts_config_with_global_hooks() {
    let mut config = valid_engine_config();
    config.hooks = Some(HooksConfig::default());

    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None).await;
    assert!(
        engine.is_ok(),
        "global hooks must not prevent engine construction"
    );
}
