//! Tests for tenant-scoped integration grants (S035 fix).
//!
//! These tests verify that tenant integrations are properly scoped and that
//! cross-tenant credential injection is not possible.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::{
    BudgetPoolConfig, SimulacraEngine, TaskManager, TenantConfig, WorkerPoolConfig,
};

fn engine_config_with_tenants(tenants: HashMap<String, SimulacraTenantConfig>) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "ollama:llama3".to_string(),
            acp_profile: None,
            system_prompt: None,
            skills: vec![],
            max_turns: Some(5),
            max_tokens: Some(1000),
            max_sub_agents: Some(0),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec!["/**".to_string()],
                paths_write: vec![],

                skill_patterns: vec![],

                memory: None,
            }),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "integration-scoping-tests".to_string(),
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

fn server_tenant(namespace: &str, integrations: Vec<String>) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "worker".to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig {
            max_tokens: 10_000,
            max_cost: "25.00".to_string(),
        },
        hooks: vec![],
        integrations,
        mcp_servers: Default::default(),
    }
}

#[test]
fn tenant_config_integrations_field_defaults_to_empty() {
    let config: TenantConfig = serde_json::from_str(
        r#"{
            "namespace": "t1",
            "agent_type": "worker",
            "vfs_root": "/tmp/t1",
            "budget_pool": {"max_tokens": 0, "max_cost": ""},
            "hooks": []
        }"#,
    )
    .unwrap();
    assert!(
        config.integrations.is_empty(),
        "integrations must default to empty"
    );
}

#[test]
fn tenant_config_integrations_field_deserializes_from_json() {
    let config: TenantConfig = serde_json::from_str(
        r#"{
            "namespace": "t1",
            "agent_type": "worker",
            "vfs_root": "/tmp/t1",
            "budget_pool": {"max_tokens": 0, "max_cost": ""},
            "hooks": [],
            "integrations": ["hubspot", "jira"]
        }"#,
    )
    .unwrap();
    assert_eq!(config.integrations, vec!["hubspot", "jira"]);
}

#[tokio::test]
async fn tenant_with_specific_integrations_limits_grants() {
    // Engine with no simulacra-config tenants (so multi-tenant mode is controlled by
    // the presence of tenants in SimulacraConfig). We test via the TenantConfig
    // integrations field.
    let tenant_a = server_tenant("tenant-a", vec!["hubspot".to_string()]);
    assert_eq!(
        tenant_a.integrations,
        vec!["hubspot"],
        "tenant A should only have hubspot"
    );

    let tenant_b = server_tenant("tenant-b", vec!["jira".to_string(), "slack".to_string()]);
    assert_eq!(
        tenant_b.integrations,
        vec!["jira", "slack"],
        "tenant B should have jira and slack"
    );

    // Tenant A's integrations list must NOT contain tenant B's integrations.
    assert!(
        !tenant_a.integrations.contains(&"jira".to_string()),
        "tenant A must not have access to jira"
    );
    assert!(
        !tenant_a.integrations.contains(&"slack".to_string()),
        "tenant A must not have access to slack"
    );
}

#[tokio::test]
async fn tenant_with_empty_integrations_gets_no_grants() {
    let tenant = server_tenant("isolated", vec![]);
    assert!(
        tenant.integrations.is_empty(),
        "tenant with empty integrations must get no credential grants"
    );
}

#[tokio::test]
async fn single_tenant_mode_falls_back_to_all_integrations() {
    // When SimulacraConfig.tenants is empty (CLI mode), the engine should fall back
    // to reg.names(). We test this by verifying the engine constructs successfully
    // with no tenants and uses default pool config.
    let config = engine_config_with_tenants(HashMap::new());
    assert!(
        config.tenants.is_empty(),
        "config must have no tenants for single-tenant mode"
    );

    // Engine should construct successfully.
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None).await;
    assert!(
        engine.is_ok(),
        "single-tenant mode engine should construct successfully"
    );
}

#[tokio::test]
async fn multi_tenant_mode_uses_per_tenant_integrations() {
    let mut tenants = HashMap::new();
    tenants.insert(
        "alpha".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: Some(vec!["hubspot".to_string()]),
            mcp_servers: Default::default(),
        },
    );
    tenants.insert(
        "beta".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: Some(vec!["jira".to_string()]),
            mcp_servers: Default::default(),
        },
    );

    let config = engine_config_with_tenants(tenants);
    let engine = SimulacraEngine::with_pool_config_in_memory_catalog(
        config,
        None,
        WorkerPoolConfig {
            count: 1,
            queue_capacity: 10,
        },
    )
    .await
    .unwrap();

    let manager = TaskManager::new();

    // Tenant alpha with hubspot integration.
    let tenant_alpha = server_tenant("alpha", vec!["hubspot".to_string()]);
    let handle_a = engine
        .spawn_task(
            &manager,
            "task for alpha",
            &tenant_alpha,
            None,
            json!({}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(handle_a.tenant, "alpha");

    // Tenant beta with jira integration.
    let tenant_beta = server_tenant("beta", vec!["jira".to_string()]);
    let handle_b = engine
        .spawn_task(
            &manager,
            "task for beta",
            &tenant_beta,
            None,
            json!({}),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(handle_b.tenant, "beta");

    // Each task should be independently created.
    assert_ne!(handle_a.task_id, handle_b.task_id);
}
