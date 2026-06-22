//! Tests for CancellationToken wiring (S034 assertions: cancellation).

use std::path::PathBuf;

use simulacra_server::{BudgetPoolConfig, TaskManager, TaskState, TenantConfig};
use tokio_util::sync::CancellationToken;

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
fn task_cancel_signals_the_stored_cancellation_token_for_that_task() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(
            &tenant("acme"),
            "Cancel me",
            None,
            serde_json::json!({}),
            None,
        )
        .expect("task should be created");
    let token = CancellationToken::new();
    let observed = token.clone();

    manager
        .set_cancellation_token(&handle.task_id, token)
        .expect("the cancellation token should be stored");
    manager
        .cancel_task(&handle.task_id)
        .expect("cancel should signal the token");

    assert!(
        observed.is_cancelled(),
        "task.cancel should signal the task's CancellationToken"
    );
}

#[test]
fn task_cancel_only_signals_the_target_tasks_token_and_not_other_running_tasks() {
    let manager = TaskManager::new();
    let first = manager
        .create_task(&tenant("acme"), "first", None, serde_json::json!({}), None)
        .expect("first task should be created");
    let second = manager
        .create_task(&tenant("acme"), "second", None, serde_json::json!({}), None)
        .expect("second task should be created");

    let first_token = CancellationToken::new();
    let second_token = CancellationToken::new();
    let observed_first = first_token.clone();
    let observed_second = second_token.clone();

    manager
        .set_cancellation_token(&first.task_id, first_token)
        .expect("first token should be stored");
    manager
        .set_cancellation_token(&second.task_id, second_token)
        .expect("second token should be stored");

    manager
        .cancel_task(&first.task_id)
        .expect("cancel should signal the target token");

    assert!(
        observed_first.is_cancelled(),
        "target token must be cancelled"
    );
    assert!(
        !observed_second.is_cancelled(),
        "other tasks must keep independent cancellation tokens"
    );
}

#[test]
fn task_transitions_to_cancelled_after_the_agent_loop_reports_cancelled_exit() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(
            &tenant("acme"),
            "cancel later",
            None,
            serde_json::json!({}),
            None,
        )
        .expect("task should be created");

    manager
        .complete_task(&handle.task_id, TaskState::Cancelled, None)
        .expect("background completion should finalize the task as cancelled");

    let task = manager
        .get_task(&handle.task_id)
        .expect("task must still exist");
    assert_eq!(task.state, TaskState::Cancelled);
}

#[test]
fn cancel_task_without_a_stored_token_falls_back_to_the_existing_immediate_transition() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(
            &tenant("acme"),
            "cancel immediately",
            None,
            serde_json::json!({}),
            None,
        )
        .expect("task should be created");

    let state = manager
        .cancel_task(&handle.task_id)
        .expect("cancel should still succeed without a stored token");

    assert_eq!(state, TaskState::Cancelled);
}

#[test]
fn each_task_has_its_own_cancellation_token() {
    let manager = TaskManager::new();
    let h1 = manager
        .create_task(&tenant("acme"), "task-1", None, serde_json::json!({}), None)
        .unwrap();
    let h2 = manager
        .create_task(&tenant("acme"), "task-2", None, serde_json::json!({}), None)
        .unwrap();

    let t1 = CancellationToken::new();
    let t2 = CancellationToken::new();
    let o1 = t1.clone();
    let o2 = t2.clone();

    manager.set_cancellation_token(&h1.task_id, t1).unwrap();
    manager.set_cancellation_token(&h2.task_id, t2).unwrap();

    // Cancel task 2 only.
    manager.cancel_task(&h2.task_id).unwrap();
    assert!(!o1.is_cancelled(), "task-1 token should not be cancelled");
    assert!(o2.is_cancelled(), "task-2 token should be cancelled");
}
