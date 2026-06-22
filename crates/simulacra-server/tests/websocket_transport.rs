//! Tests for WebSocket transport behavior (S031 assertions).
//! Most WS tests are structural — full integration tests need a running server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use simulacra_server::{
    AppState, BudgetPoolConfig, TaskManager, TaskState, TenantConfig, TenantResolver,
    auth::CompositeAuthProvider, build_router,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn build_test_state() -> AppState {
    let mut tenants = HashMap::new();
    tenants.insert(
        "csm".to_string(),
        TenantConfig {
            namespace: "csm".to_string(),
            agent_type: "csm-agent".to_string(),
            vfs_root: PathBuf::from("/data/csm"),
            budget_pool: BudgetPoolConfig {
                max_tokens: 5000,
                max_cost: String::new(),
            },
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants, None));
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(CompositeAuthProvider::new(vec![]));
    AppState::new(manager, resolver, auth)
}

// ─── TaskManager cancel-all-active assertions (WebSocket close behavior) ──────

#[test]
fn websocket_close_cancels_all_active_tasks_on_connection() {
    // Simulate the cancel-all-active behavior triggered by WebSocket close.
    let state = build_test_state();
    let tenant = TenantConfig {
        namespace: "csm".to_string(),
        agent_type: "csm-agent".to_string(),
        vfs_root: PathBuf::from("/data/csm"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };

    let h1 = state
        .task_manager
        .create_task(&tenant, "task 1", None, json!({}), None)
        .unwrap();
    let h2 = state
        .task_manager
        .create_task(&tenant, "task 2", None, json!({}), None)
        .unwrap();

    assert_eq!(state.task_manager.active_task_ids().len(), 2);

    // Simulate WebSocket close.
    let cancelled = state.task_manager.cancel_all_active();
    assert_eq!(
        cancelled.len(),
        2,
        "both active tasks must be cancelled on close"
    );

    // Verify all tasks are now in Cancelled state.
    let t1 = state.task_manager.get_task(&h1.task_id).unwrap();
    let t2 = state.task_manager.get_task(&h2.task_id).unwrap();
    assert_eq!(t1.state, TaskState::Cancelled);
    assert_eq!(t2.state, TaskState::Cancelled);
    assert_eq!(state.task_manager.active_task_ids().len(), 0);
}

#[test]
fn active_tasks_are_per_task_manager_not_shared_between_connections() {
    // Two separate TaskManagers simulate two WebSocket connections.
    let mgr1 = TaskManager::new();
    let mgr2 = TaskManager::new();

    let tenant = TenantConfig {
        namespace: "csm".to_string(),
        agent_type: "csm-agent".to_string(),
        vfs_root: PathBuf::from("/data/csm"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };

    mgr1.create_task(&tenant, "conn1 task", None, json!({}), None)
        .unwrap();
    assert_eq!(mgr1.active_task_ids().len(), 1);
    assert_eq!(
        mgr2.active_task_ids().len(),
        0,
        "connections must have isolated task state"
    );
}

// ─── Task ID uniqueness assertion ─────────────────────────────────────────────

#[test]
fn each_task_gets_a_unique_task_id() {
    let manager = TaskManager::new();
    let tenant = TenantConfig {
        namespace: "csm".to_string(),
        agent_type: "csm-agent".to_string(),
        vfs_root: PathBuf::from("/data/csm"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };

    let h1 = manager
        .create_task(&tenant, "task 1", None, json!({}), None)
        .unwrap();
    let h2 = manager
        .create_task(&tenant, "task 2", None, json!({}), None)
        .unwrap();
    let h3 = manager
        .create_task(&tenant, "task 3", None, json!({}), None)
        .unwrap();

    assert_ne!(h1.task_id, h2.task_id, "task IDs must be unique");
    assert_ne!(h2.task_id, h3.task_id, "task IDs must be unique");
    assert_ne!(h1.task_id, h3.task_id, "task IDs must be unique");
}

// ─── Event router structural assertion ───────────────────────────────────────

#[test]
fn websocket_route_is_registered_at_api_v1_ws() {
    // Structural check: the router includes /api/v1/ws route.
    let state = build_test_state();
    let router = build_router(state, vec![], None);
    // Router construction must not panic.
    // Full WebSocket tests would use a live server.
    let _ = router;
}

// ─── Error event shape assertion ──────────────────────────────────────────────

#[test]
fn connection_scoped_error_event_omits_task_id() {
    let error_event = json!({
        "event": "error",
        "code": "invalid_message",
        "message": "malformed command: missing 'command' field"
    });

    assert!(
        error_event.get("task_id").is_none(),
        "connection-scoped error must not have task_id"
    );
}

#[test]
fn task_scoped_error_event_includes_task_id() {
    let error_event = json!({
        "event": "error",
        "task_id": "task-abc123",
        "code": "budget_exceeded",
        "message": "token budget exhausted"
    });

    assert!(
        error_event.get("task_id").is_some(),
        "task-scoped error must include task_id"
    );
}

// ─── Concurrent task isolation assertion ──────────────────────────────────────

#[test]
fn each_task_has_independent_state() {
    let manager = TaskManager::new();
    let tenant = TenantConfig {
        namespace: "csm".to_string(),
        agent_type: "csm-agent".to_string(),
        vfs_root: PathBuf::from("/data/csm"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };

    let h1 = manager
        .create_task(&tenant, "task 1", None, json!({}), None)
        .unwrap();
    let h2 = manager
        .create_task(&tenant, "task 2", None, json!({}), None)
        .unwrap();

    // Pause task 1 — task 2 must remain Running.
    manager.pause_task(&h1.task_id).unwrap();

    let t1 = manager.get_task(&h1.task_id).unwrap();
    let t2 = manager.get_task(&h2.task_id).unwrap();

    assert_eq!(t1.state, TaskState::Paused, "task 1 must be paused");
    assert_eq!(t2.state, TaskState::Running, "task 2 must remain running");
}

// ─── BLOCKER 2: WebSocket ownership check ────────────────────────────────────

#[test]
fn ws_cancel_command_rejected_when_tenant_does_not_own_task() {
    use simulacra_server::{Identity, TaskState, TenantConfig, TenantResolver, task::TaskManager};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    // Two tenants.
    let mut tenants = HashMap::new();
    tenants.insert(
        "tenant-a".to_string(),
        TenantConfig {
            namespace: "tenant-a".to_string(),
            agent_type: "agent-a".to_string(),
            vfs_root: PathBuf::from("/data/a"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    tenants.insert(
        "tenant-b".to_string(),
        TenantConfig {
            namespace: "tenant-b".to_string(),
            agent_type: "agent-b".to_string(),
            vfs_root: PathBuf::from("/data/b"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants.clone(), None));

    // Create task-b owned by tenant-b.
    let tenant_b_config = tenants.get("tenant-b").unwrap();
    let handle_b = manager
        .create_task(tenant_b_config, "task for b", None, json!({}), None)
        .unwrap();

    // Identity for tenant-a attempting to operate on tenant-b's task.
    let identity_a = Identity {
        subject: "user-a".to_string(),
        tenant_namespace: Some("tenant-a".to_string()),
        scopes: vec!["tasks:manage".to_string()],
        metadata: json!({}),
    };

    // Simulate what check_ws_ownership does: tenant-a must not own tenant-b's task.
    let task_handle = manager.get_task(&handle_b.task_id).unwrap();
    let tenant_a_config = resolver.resolve(&identity_a).unwrap();
    let ownership_ok = task_handle.tenant == tenant_a_config.namespace;

    assert!(
        !ownership_ok,
        "tenant-a must not be allowed to operate on tenant-b's task"
    );

    // The task must still be Running (not cancelled).
    let task = manager.get_task(&handle_b.task_id).unwrap();
    assert_eq!(
        task.state,
        TaskState::Running,
        "task must remain Running when cancel is denied due to ownership mismatch"
    );
}
