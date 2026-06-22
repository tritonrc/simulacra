//! Tests for Pending -> Running task state transition (S035).

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use simulacra_server::{BudgetPoolConfig, TaskManager, TaskManagerError, TaskState, TenantConfig};

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

#[test]
fn create_pending_task_starts_in_pending_state() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    assert_eq!(
        handle.state,
        TaskState::Pending,
        "create_pending_task must return a task in Pending state"
    );

    // Verify the stored state matches.
    let stored = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(stored.state, TaskState::Pending);
}

#[test]
fn start_task_transitions_pending_to_running() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    let state = manager.start_task(&handle.task_id).unwrap();
    assert_eq!(
        state,
        TaskState::Running,
        "start_task must transition to Running"
    );

    let stored = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(stored.state, TaskState::Running);
}

#[test]
fn start_task_rejects_already_running_task() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    manager.start_task(&handle.task_id).unwrap();

    // Second start_task should fail.
    let err = manager.start_task(&handle.task_id).unwrap_err();
    assert!(
        matches!(err, TaskManagerError::InvalidTransition { .. }),
        "start_task on a running task must fail, got: {err:?}"
    );
}

#[tokio::test]
async fn pending_to_running_transition_emits_state_changed_event() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    let (_history, mut rx) = manager.subscribe_task(&handle.task_id).unwrap();

    manager.start_task(&handle.task_id).unwrap();

    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within 1s")
        .expect("broadcast recv should not fail");

    assert_eq!(event["event"], "task.state_changed");
    assert_eq!(event["from"], "pending");
    assert_eq!(event["to"], "running");
    assert_eq!(event["task_id"], handle.task_id);
}

#[test]
fn create_pending_task_preserves_tenant_and_metadata() {
    let manager = TaskManager::new();
    let t = tenant("beta-corp");
    let handle = manager
        .create_pending_task(
            &t,
            "analyze data",
            Some("researcher".to_string()),
            json!({"priority": "high"}),
            Some("conn-5".to_string()),
        )
        .unwrap();

    assert_eq!(handle.tenant, "beta-corp");
    assert_eq!(handle.agent_type, "researcher");
    assert_eq!(handle.description, "analyze data");
    assert_eq!(handle.connection_id.as_deref(), Some("conn-5"));
}

#[test]
fn create_pending_task_does_not_auto_transition_to_running() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    // After creation, the task should remain in Pending (not Running).
    let stored = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(
        stored.state,
        TaskState::Pending,
        "create_pending_task must NOT auto-transition to Running"
    );
    assert_eq!(
        stored.seq, 0,
        "pending task should have seq 0 (no transitions yet)"
    );
}

#[test]
fn start_task_increments_seq_counter() {
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant("acme"), "test task", None, json!({}), None)
        .unwrap();

    assert_eq!(handle.seq, 0);

    manager.start_task(&handle.task_id).unwrap();

    let stored = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(stored.seq, 1, "start_task must increment seq to 1");
}

#[test]
fn start_task_on_nonexistent_task_returns_not_found() {
    let manager = TaskManager::new();
    let err = manager.start_task("nonexistent-task-id").unwrap_err();
    assert!(
        matches!(err, TaskManagerError::NotFound { .. }),
        "start_task on nonexistent task must return NotFound, got: {err:?}"
    );
}
