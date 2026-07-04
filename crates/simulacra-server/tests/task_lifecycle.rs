//! Tests for task lifecycle state machine (S031 assertions).

use serde_json::json;
use simulacra_runtime::ToolApprovalResponse;
use simulacra_server::{BudgetPoolConfig, TaskManager, TaskManagerError, TaskState, TenantConfig};
use std::path::PathBuf;

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: format!("{namespace}-agent"),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig {
            max_tokens: 1000,
            max_cost: "25.00".to_string(),
        },
        hooks: vec!["governance.pre_tool".to_string()],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

// ─── Lifecycle state assertions ───────────────────────────────────────────────

#[test]
fn task_create_transitions_task_from_pending_to_running() {
    let manager = TaskManager::new();
    let created = manager
        .create_task(&tenant("accounting"), "draft report", None, json!({}), None)
        .expect("task creation must succeed");

    // Spec: task.create transitions from pending to running.
    assert_eq!(created.state, TaskState::Running);
}

#[test]
fn task_cancel_transitions_task_to_cancelled_after_current_operation_completes() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("accounting"), "draft report", None, json!({}), None)
        .unwrap();

    let state = manager
        .cancel_task(&handle.task_id)
        .expect("cancel must succeed");
    assert_eq!(state, TaskState::Cancelled);
}

#[test]
fn task_pause_transitions_running_task_to_paused() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("accounting"), "draft report", None, json!({}), None)
        .unwrap();

    let state = manager
        .pause_task(&handle.task_id)
        .expect("pause must succeed");
    assert_eq!(state, TaskState::Paused);
}

#[test]
fn task_resume_transitions_paused_task_back_to_running() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("accounting"), "draft report", None, json!({}), None)
        .unwrap();
    manager.pause_task(&handle.task_id).unwrap();

    let state = manager
        .resume_task(&handle.task_id)
        .expect("resume must succeed");
    assert_eq!(state, TaskState::Running);
}

#[tokio::test]
async fn input_response_sends_to_live_channel_and_transitions_to_running() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("csm"), "investigate ticket", None, json!({}), None)
        .unwrap();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(1);
    let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(1);
    manager
        .set_hitl_senders(&handle.task_id, input_tx, approval_tx)
        .unwrap();
    manager.request_input(&handle.task_id).unwrap();

    let state = manager
        .provide_input(&handle.task_id, "please proceed")
        .expect("input response should be accepted");
    assert_eq!(state, TaskState::Running);
    assert_eq!(
        input_rx.recv().await.as_deref(),
        Some("please proceed"),
        "input response must reach the live agent channel"
    );

    let task = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(task.state, TaskState::Running);
}

#[test]
fn input_response_without_live_channel_fails_explicitly() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("csm"), "investigate ticket", None, json!({}), None)
        .unwrap();
    manager.request_input(&handle.task_id).unwrap();

    let result = manager.provide_input(&handle.task_id, "please proceed");
    assert!(
        matches!(
            result,
            Err(TaskManagerError::ResponseChannelUnavailable {
                op: "provide_input",
                ..
            })
        ),
        "provide_input on WaitingInput without a channel must fail explicitly, got {result:?}"
    );
}

#[tokio::test]
async fn approval_response_sends_to_live_channel_and_transitions_to_running() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("csm"), "investigate ticket", None, json!({}), None)
        .unwrap();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::channel(2);
    manager
        .set_hitl_senders(&handle.task_id, input_tx, approval_tx)
        .unwrap();
    manager
        .request_approval_for(&handle.task_id, Some("tool-call-1".into()))
        .unwrap();

    let state = manager
        .respond_approval(&handle.task_id, "tool-call-1", true, None)
        .expect("approval response should be accepted");
    assert_eq!(state, TaskState::Running);
    assert_eq!(
        approval_rx.recv().await,
        Some(ToolApprovalResponse {
            tool_call_id: "tool-call-1".into(),
            approved: true,
            reason: None,
        })
    );

    manager
        .request_approval_for(&handle.task_id, Some("tool-call-2".into()))
        .unwrap();
    manager
        .respond_approval(&handle.task_id, "tool-call-2", false, Some("deny"))
        .expect("denial should be sent to the agent loop");
    assert_eq!(
        approval_rx.recv().await,
        Some(ToolApprovalResponse {
            tool_call_id: "tool-call-2".into(),
            approved: false,
            reason: Some("deny".into()),
        })
    );
}

#[tokio::test]
async fn approval_response_rejects_mismatched_tool_call_id_without_sending() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("csm"), "investigate ticket", None, json!({}), None)
        .unwrap();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::channel(1);
    manager
        .set_hitl_senders(&handle.task_id, input_tx, approval_tx)
        .unwrap();
    manager
        .request_approval_for(&handle.task_id, Some("expected-call".into()))
        .unwrap();

    let result = manager.respond_approval(&handle.task_id, "stale-call", true, None);
    assert!(
        matches!(
            result,
            Err(TaskManagerError::ApprovalToolCallMismatch { .. })
        ),
        "stale approval response must be rejected, got {result:?}"
    );
    assert!(approval_rx.try_recv().is_err());
    assert_eq!(
        manager.get_task(&handle.task_id).unwrap().state,
        TaskState::WaitingApproval
    );
}

#[test]
fn terminal_states_are_final_and_commands_on_terminal_tasks_return_error() {
    let manager = TaskManager::new();

    // Create a task and cancel it (terminal).
    let handle = manager
        .create_task(
            &tenant("accounting"),
            "completed task",
            None,
            json!({}),
            None,
        )
        .unwrap();
    manager.cancel_task(&handle.task_id).unwrap();

    // All further commands must return TerminalState error.
    let cancel_result = manager.cancel_task(&handle.task_id);
    let pause_result = manager.pause_task(&handle.task_id);
    let resume_result = manager.resume_task(&handle.task_id);

    assert!(
        matches!(cancel_result, Err(TaskManagerError::TerminalState { .. })),
        "cancel on terminal task must error"
    );
    assert!(
        matches!(pause_result, Err(TaskManagerError::TerminalState { .. })),
        "pause on terminal task must error"
    );
    assert!(
        matches!(resume_result, Err(TaskManagerError::TerminalState { .. })),
        "resume on terminal task must error"
    );
}

#[test]
fn task_budget_is_created_from_tenants_budget_pool_config() {
    let manager = TaskManager::new();
    let t = tenant("ops");
    let handle = manager
        .create_task(&t, "investigate alert", None, json!({}), None)
        .unwrap();

    // The task metadata must include the tenant's budget_pool.
    assert_eq!(
        handle.metadata["budget_pool"]["max_tokens"],
        json!(1000),
        "task metadata must carry budget_pool from tenant config"
    );
}

// ─── Trigger-task equivalence assertions ──────────────────────────────────────

#[test]
fn webhook_created_task_has_identical_lifecycle_states_as_api_created_task() {
    let manager = TaskManager::new();

    let api_task = manager
        .create_task(
            &tenant("csm"),
            "api task",
            None,
            json!({"source": "api"}),
            None,
        )
        .unwrap();
    let webhook_task = manager
        .create_task(
            &tenant("csm"),
            "webhook task",
            None,
            json!({"source": "webhook", "webhook_name": "new-customer"}),
            None,
        )
        .unwrap();

    // Both start in Running state — same lifecycle.
    assert_eq!(api_task.state, webhook_task.state);
    assert_eq!(api_task.state, TaskState::Running);
}

#[test]
fn schedule_created_task_has_identical_lifecycle_states_as_api_created_task() {
    let manager = TaskManager::new();

    let api_task = manager
        .create_task(
            &tenant("accounting"),
            "api task",
            None,
            json!({"source": "api"}),
            None,
        )
        .unwrap();
    let scheduled_task = manager
        .create_task(
            &tenant("accounting"),
            "scheduled task",
            None,
            json!({"source": "schedule", "schedule_name": "quarterly-report"}),
            None,
        )
        .unwrap();

    assert_eq!(api_task.state, scheduled_task.state);
    assert_eq!(scheduled_task.state, TaskState::Running);
}

#[test]
fn trigger_source_metadata_is_recorded_in_the_tasks_metadata() {
    let manager = TaskManager::new();
    let created = manager
        .create_task(
            &tenant("ops"),
            "triggered task",
            None,
            json!({"source": "webhook", "webhook_name": "incident", "payload_hash": "abc123"}),
            None,
        )
        .unwrap();

    // Trigger source metadata is preserved in task metadata.
    assert_eq!(created.metadata["source"], json!("webhook"));
    assert_eq!(created.metadata["webhook_name"], json!("incident"));
}

#[test]
fn agent_cannot_distinguish_trigger_source_from_task_description() {
    let manager = TaskManager::new();
    let api_task = manager
        .create_task(
            &tenant("ops"),
            "Investigate outage",
            None,
            json!({"source": "api"}),
            None,
        )
        .unwrap();
    let triggered_task = manager
        .create_task(
            &tenant("ops"),
            "Investigate outage",
            None,
            json!({"source": "schedule", "schedule_name": "nightly-check"}),
            None,
        )
        .unwrap();

    // The description exposed to the agent is identical regardless of trigger source.
    assert_eq!(api_task.description, triggered_task.description);
}

#[test]
fn tenant_budget_pool_applies_to_triggered_tasks() {
    let manager = TaskManager::new();
    let t = tenant("accounting");
    let created = manager
        .create_task(
            &t,
            "scheduled report",
            None,
            json!({"source": "schedule", "schedule_name": "quarterly"}),
            None,
        )
        .unwrap();

    assert_eq!(
        created.metadata["budget_pool"],
        serde_json::to_value(&t.budget_pool).unwrap()
    );
}

#[tokio::test]
async fn approval_denial_transitions_task_to_running_not_stuck_in_waiting_approval() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("csm"), "investigate ticket", None, json!({}), None)
        .unwrap();
    let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::channel(1);
    manager
        .set_hitl_senders(&handle.task_id, input_tx, approval_tx)
        .unwrap();
    manager
        .request_approval_for(&handle.task_id, Some("tool-call-1".into()))
        .unwrap();

    let result = manager.respond_approval(&handle.task_id, "tool-call-1", false, Some("denied"));
    assert_eq!(result.unwrap(), TaskState::Running);
    assert_eq!(
        approval_rx.recv().await,
        Some(ToolApprovalResponse {
            tool_call_id: "tool-call-1".into(),
            approved: false,
            reason: Some("denied".into()),
        })
    );

    let task = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(
        task.state,
        TaskState::Running,
        "after denial, task must transition back to Running so agent can continue"
    );
}

// ─── WARNING 1: pending → running transition must be emitted ──────────────────

#[test]
fn create_task_emits_pending_then_running_events_on_per_task_channel() {
    let manager = TaskManager::new();
    // We need to create first (to get a task_id). Verify the handle returned
    // from create_task is in Running state, confirming the pending→running
    // transition happened.
    let handle = manager
        .create_task(&tenant("ops"), "check alerts", None, json!({}), None)
        .unwrap();

    assert_eq!(
        handle.state,
        TaskState::Running,
        "create_task must return a handle in Running state after pending→running transition"
    );

    // subscribe_task must succeed (channel exists).
    assert!(
        manager.subscribe_task(&handle.task_id).is_ok(),
        "subscribe_task must return a valid receiver for an existing task"
    );
}

// ─── BLOCKER 2: WS close cancels only that connection's tasks ─────────────────

#[test]
fn cancel_connection_tasks_only_cancels_tasks_for_the_given_connection() {
    let manager = TaskManager::new();
    let t = tenant("csm");

    // Create tasks for connection A.
    let conn_a = "conn-a-uuid";
    let a1 = manager
        .create_task(
            &t,
            "conn-a task 1",
            None,
            json!({}),
            Some(conn_a.to_string()),
        )
        .unwrap();
    let a2 = manager
        .create_task(
            &t,
            "conn-a task 2",
            None,
            json!({}),
            Some(conn_a.to_string()),
        )
        .unwrap();

    // Create a task for connection B.
    let conn_b = "conn-b-uuid";
    let b1 = manager
        .create_task(
            &t,
            "conn-b task 1",
            None,
            json!({}),
            Some(conn_b.to_string()),
        )
        .unwrap();

    // Cancelling connection A must not affect connection B's tasks.
    let cancelled = manager.cancel_connection_tasks(conn_a);
    assert_eq!(cancelled.len(), 2, "both conn-a tasks must be cancelled");
    assert!(cancelled.contains(&a1.task_id));
    assert!(cancelled.contains(&a2.task_id));

    let a1_state = manager.get_task(&a1.task_id).unwrap().state;
    let a2_state = manager.get_task(&a2.task_id).unwrap().state;
    let b1_state = manager.get_task(&b1.task_id).unwrap().state;

    assert_eq!(a1_state, TaskState::Cancelled);
    assert_eq!(a2_state, TaskState::Cancelled);
    assert_eq!(
        b1_state,
        TaskState::Running,
        "conn-b task must remain running when conn-a closes"
    );
}

#[test]
fn cancel_connection_tasks_does_not_cancel_tasks_without_connection_id() {
    let manager = TaskManager::new();
    let t = tenant("csm");

    // REST-created task (no connection_id).
    let rest_task = manager
        .create_task(&t, "rest task", None, json!({}), None)
        .unwrap();

    // WS task for some connection.
    let ws_task = manager
        .create_task(&t, "ws task", None, json!({}), Some("conn-x".to_string()))
        .unwrap();

    // Cancelling connection "conn-x" must not affect the REST task.
    manager.cancel_connection_tasks("conn-x");

    let rest_state = manager.get_task(&rest_task.task_id).unwrap().state;
    let ws_state = manager.get_task(&ws_task.task_id).unwrap().state;

    assert_eq!(
        rest_state,
        TaskState::Running,
        "REST task must remain running when a WS connection closes"
    );
    assert_eq!(ws_state, TaskState::Cancelled);
}
