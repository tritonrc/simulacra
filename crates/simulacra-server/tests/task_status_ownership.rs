//! Tests for task_status ownership check (S035 IDOR fix).
//!
//! Verifies that GET /api/v1/tasks/{task_id}/status uses
//! resolve_and_check_ownership to prevent cross-tenant task access.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use serde_json::json;
use simulacra_server::{
    AppState, BudgetPoolConfig, TaskManager, TenantConfig, TenantResolver,
    auth::{ApiKeyAuthProvider, ApiKeyEntry},
    build_router,
};

fn two_tenant_state() -> (AppState, Arc<TaskManager>, HashMap<String, TenantConfig>) {
    let mut tenants = HashMap::new();
    tenants.insert(
        "tenant-a".to_string(),
        TenantConfig {
            namespace: "tenant-a".to_string(),
            agent_type: "worker".to_string(),
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
            agent_type: "worker".to_string(),
            vfs_root: PathBuf::from("/data/b"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants.clone(), None));

    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![
            ApiKeyEntry {
                key: "key-a".to_string(),
                subject: "user-a".to_string(),
                tenant_namespace: Some("tenant-a".to_string()),
                scopes: vec!["tasks:manage".to_string()],
            },
            ApiKeyEntry {
                key: "key-b".to_string(),
                subject: "user-b".to_string(),
                tenant_namespace: Some("tenant-b".to_string()),
                scopes: vec!["tasks:manage".to_string()],
            },
        ]));

    let state = AppState::new(manager.clone(), resolver, auth);
    (state, manager, tenants)
}

#[tokio::test]
async fn task_status_returns_403_for_cross_tenant_access() {
    let (state, manager, tenants) = two_tenant_state();

    // Create a task owned by tenant-a.
    let tenant_a = tenants.get("tenant-a").unwrap();
    let handle = manager
        .create_task(tenant_a, "task for a", None, json!({}), None)
        .unwrap();

    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    // tenant-b tries to read tenant-a's task status -> 403.
    let response = server
        .get(&format!("/api/v1/tasks/{}/status", handle.task_id))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-b"),
        )
        .await;

    response.assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn task_status_returns_200_for_owned_task() {
    let (state, manager, tenants) = two_tenant_state();

    // Create a task owned by tenant-a.
    let tenant_a = tenants.get("tenant-a").unwrap();
    let handle = manager
        .create_task(tenant_a, "task for a", None, json!({}), None)
        .unwrap();

    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    // tenant-a reads their own task -> 200.
    let response = server
        .get(&format!("/api/v1/tasks/{}/status", handle.task_id))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["data"]["task_id"], json!(handle.task_id));
}

#[tokio::test]
async fn task_status_returns_404_for_nonexistent_task() {
    let (state, _manager, _tenants) = two_tenant_state();

    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    let response = server
        .get("/api/v1/tasks/nonexistent-task-id/status")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn task_status_returns_401_without_credentials() {
    let (state, _manager, _tenants) = two_tenant_state();

    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    let response = server.get("/api/v1/tasks/any-task-id/status").await;

    response.assert_status(StatusCode::UNAUTHORIZED);
}
