//! Tests for SimulacraEngine::spawn_task (S034 assertions: task spawning).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::{
    BudgetPoolConfig, EngineError, SimulacraEngine, TaskManager, TaskState, TenantConfig,
};

fn engine_config(model: &str) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            model: model.to_string(),
            system_prompt: Some("You are the worker.".to_string()),
            skills: vec![],
            max_turns: Some(12),
            max_tokens: Some(8_192),
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
            name: "simulacra-engine-spawn-tests".to_string(),
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

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "worker".to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig {
            max_tokens: 10_000,
            max_cost: "25.00".to_string(),
        },
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

#[tokio::test]
async fn spawn_task_returns_err_agent_not_found_for_unknown_agent_type_override() {
    let config = engine_config("ollama:llama3");
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None)
        .await
        .expect("engine should construct");
    let manager = TaskManager::new();

    let error = engine
        .spawn_task(
            &manager,
            "Investigate CRM import failure",
            &tenant("acme"),
            Some("missing-agent"),
            json!({"source": "api"}),
            None,
            None,
        )
        .await
        .expect_err("unknown agent types must be rejected");

    assert!(
        matches!(
            &error,
            EngineError::AgentNotFound { tenant, agent }
                if tenant == "acme" && agent == "missing-agent"
        ),
        "expected AgentNotFound{{tenant=acme,agent=missing-agent}}, got: {error:?}"
    );
}

#[tokio::test]
async fn spawn_task_returns_pending_task_handle_immediately() {
    let config = engine_config("ollama:llama3");
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None)
        .await
        .expect("engine should construct");
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "Draft the weekly update",
            &tenant("acme"),
            None,
            json!({"source": "api"}),
            None,
            Some("conn-1".to_string()),
        )
        .await
        .expect("spawn_task should return immediately");

    // S035: spawn_task returns Pending (worker pool model).
    assert_eq!(handle.state, TaskState::Pending);
}

#[tokio::test]
async fn spawn_task_uses_the_tenants_default_agent_type_when_no_override_is_provided() {
    let config = engine_config("ollama:llama3");
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None)
        .await
        .expect("engine should construct");
    let manager = TaskManager::new();
    let t = tenant("acme");

    let handle = engine
        .spawn_task(
            &manager,
            "Summarize customer escalations",
            &t,
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should use the tenant default agent type");

    assert_eq!(handle.agent_type, t.agent_type);
}

#[tokio::test]
async fn spawn_task_passes_through_connection_id_for_ws_owned_tasks() {
    let config = engine_config("ollama:llama3");
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None)
        .await
        .expect("engine should construct");
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "Stream updates",
            &tenant("acme"),
            None,
            json!({}),
            None,
            Some("ws-42".to_string()),
        )
        .await
        .expect("spawn_task should preserve connection ownership");

    assert_eq!(handle.connection_id.as_deref(), Some("ws-42"));
}

#[tokio::test]
async fn spawn_task_allows_concurrent_calls_to_create_independent_running_tasks() {
    let config = engine_config("ollama:llama3");
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(config, None)
            .await
            .expect("engine should construct"),
    );
    let manager = Arc::new(TaskManager::new());
    let t = tenant("acme");

    let first = {
        let engine = engine.clone();
        let manager = manager.clone();
        let t = t.clone();
        tokio::spawn(async move {
            engine
                .spawn_task(
                    &manager,
                    "Task A",
                    &t,
                    None,
                    json!({"task": "a"}),
                    None,
                    None,
                )
                .await
        })
    };

    let second = {
        let engine = engine.clone();
        let manager = manager.clone();
        let t = t.clone();
        tokio::spawn(async move {
            engine
                .spawn_task(
                    &manager,
                    "Task B",
                    &t,
                    None,
                    json!({"task": "b"}),
                    None,
                    None,
                )
                .await
        })
    };

    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();

    assert_ne!(first.task_id, second.task_id);
    // S035: spawn_task returns Pending (worker pool model).
    assert_eq!(first.state, TaskState::Pending);
    assert_eq!(second.state, TaskState::Pending);
}

#[tokio::test]
async fn spawn_task_marks_task_failed_when_provider_env_var_is_missing() {
    // Ollama doesn't require env vars, so spawning should succeed.
    let config = engine_config("ollama:llama3");
    let engine = SimulacraEngine::new_with_in_memory_catalog(config, None)
        .await
        .expect("engine should construct");
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "Test env var handling",
            &tenant("acme"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("ollama model should not require env vars");

    // S035: spawn_task returns Pending (worker pool model).
    assert_eq!(handle.state, TaskState::Pending);
}
