//! Tests for TaskManager extensions: emit_event, get_event_sender, complete_task with reason
//! (S034 assertions: TaskManager emit_event, get_event_sender).

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use simulacra_server::{BudgetPoolConfig, TaskManager, TaskManagerError, TaskState, TenantConfig};
use tokio::time::timeout;

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
async fn emit_event_sends_json_on_the_tasks_broadcast_channel() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "emit event", None, json!({}), None)
        .expect("task should be created");
    let (_history, mut rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("subscriber should attach to the task");

    let seq = manager
        .emit_event(
            &handle.task_id,
            json!({
                "event": "budget.warning",
                "task_id": handle.task_id,
                "budget_type": "tokens",
                "used": 8_000,
                "limit": 10_000,
                "pct": 80
            }),
        )
        .expect("emit_event should succeed");

    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("event should arrive before timeout")
        .expect("broadcast receive should succeed");
    assert_eq!(event["event"], "budget.warning");
    assert_eq!(event["pct"], 80);
    assert_eq!(event["seq"], seq);
}

#[test]
fn emit_event_returns_not_found_for_nonexistent_task_ids() {
    let manager = TaskManager::new();

    let error = manager
        .emit_event("missing-task", json!({"event": "agent.turn_complete"}))
        .expect_err("emit_event should reject unknown task ids");

    assert!(matches!(error, TaskManagerError::NotFound { .. }));
}

#[tokio::test]
async fn emit_event_increments_seq_for_every_event_not_only_state_transitions() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "sequence task", None, json!({}), None)
        .expect("task should be created");
    let (_history, mut rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("subscriber should attach to the task");

    let first_seq = manager
        .emit_event(
            &handle.task_id,
            json!({"event": "agent.message", "task_id": handle.task_id, "content": "one"}),
        )
        .expect("first event should succeed");
    let second_seq = manager
        .emit_event(
            &handle.task_id,
            json!({"event": "tool.result", "task_id": handle.task_id, "tool_name": "shell_exec"}),
        )
        .expect("second event should succeed");

    let first = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("first event should arrive")
        .expect("broadcast receive should succeed");
    let second = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("second event should arrive")
        .expect("broadcast receive should succeed");

    assert_eq!(first["seq"], first_seq);
    assert_eq!(second["seq"], second_seq);
    assert!(second_seq > first_seq, "event seq should be monotonic");
}

#[tokio::test]
async fn emit_event_is_non_blocking_even_without_any_subscribers() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "no listeners", None, json!({}), None)
        .expect("task should be created");

    timeout(Duration::from_secs(1), async {
        manager
            .emit_event(
                &handle.task_id,
                json!({"event": "agent.turn_complete", "task_id": handle.task_id}),
            )
            .expect("emit_event should not block");
    })
    .await
    .expect("emit_event must remain non-blocking");
}

#[test]
fn get_event_sender_returns_a_cloned_sender_for_existing_tasks() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "sender task", None, json!({}), None)
        .expect("task should be created");

    let sender = manager
        .get_event_sender(&handle.task_id)
        .expect("sender should be returned");

    // Sender exists and can be used.
    assert_eq!(sender.receiver_count(), 0);
}

#[test]
fn get_event_sender_returns_not_found_for_nonexistent_tasks() {
    let manager = TaskManager::new();

    let error = manager
        .get_event_sender("missing-task")
        .expect_err("unknown tasks should not expose a sender");

    assert!(matches!(error, TaskManagerError::NotFound { .. }));
}

#[tokio::test]
async fn complete_task_emits_the_final_state_change_with_a_reason_field() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "complete task", None, json!({}), None)
        .expect("task should be created");
    let (_history, mut rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("subscriber should attach to the task");

    manager
        .complete_task(
            &handle.task_id,
            TaskState::Completed,
            Some("max_turns".to_string()),
        )
        .expect("complete_task should accept terminal reasons");

    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("event should arrive before timeout")
        .expect("broadcast receive should succeed");
    assert_eq!(event["event"], "task.state_changed");
    assert_eq!(event["to"], "completed");
    assert_eq!(event["reason"], "max_turns");
}

#[tokio::test]
async fn subscribe_task_replays_events_emitted_before_the_subscriber_attached() {
    // Repro for the bug where the browser navigates to /run/:task_id *after*
    // the agent loop has already begun emitting tokens, tool.called/output/result.
    // Without history, the subscriber's BroadcastStream sees nothing — and the
    // activity feed renders blank even on a successful run.
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "replay task", None, json!({}), None)
        .expect("task should be created");

    // Emit several events BEFORE any subscriber attaches.
    manager
        .emit_event(
            &handle.task_id,
            json!({"event": "agent.message", "content": "one"}),
        )
        .unwrap();
    manager
        .emit_event(
            &handle.task_id,
            json!({"event": "tool.called", "tool_call_id": "tc-1", "tool_name": "shell_exec"}),
        )
        .unwrap();
    manager
        .emit_event(
            &handle.task_id,
            json!({"event": "tool.result", "tool_call_id": "tc-1", "is_error": false}),
        )
        .unwrap();

    // Now subscribe. History must contain the pending→running event from
    // create_task PLUS all three explicitly emitted events, in order.
    let (history, _rx) = manager.subscribe_task(&handle.task_id).unwrap();

    let events: Vec<&str> = history
        .iter()
        .map(|e| e["event"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        events,
        vec![
            "task.state_changed", // pending → running
            "agent.message",
            "tool.called",
            "tool.result",
        ],
        "subscribe_task must replay all events emitted before subscription"
    );
}

#[test]
fn complete_task_accepts_waiting_approval_as_non_terminal_transition() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("acme"), "approval task", None, json!({}), None)
        .expect("task should be created");

    manager
        .complete_task(&handle.task_id, TaskState::WaitingApproval, None)
        .expect("WaitingApproval should be accepted by complete_task");

    let task = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(task.state, TaskState::WaitingApproval);
}
