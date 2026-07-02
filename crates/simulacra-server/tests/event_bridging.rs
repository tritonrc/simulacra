//! Tests for EngineActivitySink event translation (S034 assertions: event bridging).

use std::time::Duration;

use serde_json::json;
use simulacra_runtime::ActivitySink;
use simulacra_server::{
    BudgetPoolConfig, EngineActivitySink, TaskEventChannel, TaskManager, TaskState, TenantConfig,
};
use simulacra_types::ActivityEvent;
use tokio::sync::broadcast;
use tokio::time::timeout;

async fn recv_event(rx: &mut broadcast::Receiver<serde_json::Value>) -> serde_json::Value {
    timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("event should arrive before timeout")
        .expect("broadcast receive should succeed")
}

/// Build a TaskEventChannel and a live receiver attached to it. Mirrors the
/// shape of `broadcast::channel(cap)` so the test bodies can stay terse.
fn make_channel(cap: usize) -> (TaskEventChannel, broadcast::Receiver<serde_json::Value>) {
    let chan = TaskEventChannel::new(cap);
    let (_history, rx) = chan.subscribe_with_history();
    (chan, rx)
}

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "agent".to_string(),
        vfs_root: std::path::PathBuf::from("/tmp/simulacra-test"),
        budget_pool: BudgetPoolConfig {
            max_tokens: 1000,
            max_cost: "0".into(),
        },
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

#[tokio::test]
async fn token_events_translate_to_agent_message_with_task_id_and_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-1".to_string(), tx);

    sink.emit(ActivityEvent::Token {
        text: "hello world".to_string(),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.message");
    assert_eq!(event["task_id"], "task-1");
    assert_eq!(event["content"], "hello world");
    assert_eq!(event["role"], "assistant");
    assert!(event["seq"].is_number());
}

#[tokio::test]
async fn tool_start_events_translate_to_tool_called_with_arguments_and_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-2".to_string(), tx);

    sink.emit(ActivityEvent::ToolStart {
        tool_call_id: "tool-1".to_string(),
        name: "file_read".to_string(),
        arguments: json!({"path": "/workspace/task.md"}),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "tool.called");
    assert_eq!(event["task_id"], "task-2");
    assert_eq!(event["tool_call_id"], "tool-1");
    assert_eq!(event["tool_name"], "file_read");
    assert_eq!(event["arguments"]["path"], "/workspace/task.md");
}

#[tokio::test]
async fn tool_call_delta_events_translate_to_tool_call_delta_with_optional_metadata_and_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-delta".to_string(), tx);

    sink.emit(ActivityEvent::ToolCallDelta {
        index: 2,
        tool_call_id: Some("tool-delta-1".to_string()),
        name: Some("file_read".to_string()),
        arguments_delta: "{\"path\"".to_string(),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "tool.call_delta");
    assert_eq!(event["task_id"], "task-delta");
    assert_eq!(event["index"], 2);
    assert_eq!(event["tool_call_id"], "tool-delta-1");
    assert_eq!(event["tool_name"], "file_read");
    assert_eq!(event["arguments_delta"], "{\"path\"");
    assert!(event["seq"].is_number());
}

#[tokio::test]
async fn hitl_activity_events_translate_to_server_events_and_waiting_states() {
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant("hitl"), "needs human", None, json!({}), None)
        .expect("task should be created");
    let (history, mut rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("task subscription should exist");
    assert!(
        history
            .iter()
            .any(|event| event["event"] == "task.state_changed")
    );
    let sender = manager
        .get_event_sender(&handle.task_id)
        .expect("event sender should exist");
    let sink =
        EngineActivitySink::with_task_manager(handle.task_id.clone(), sender, manager.clone());

    sink.emit(ActivityEvent::InputRequired {
        prompt: "Need detail".into(),
        schema: None,
    });
    let state_event = recv_event(&mut rx).await;
    assert_eq!(state_event["event"], "task.state_changed");
    assert_eq!(state_event["to"], "waiting_input");
    let input_event = recv_event(&mut rx).await;
    assert_eq!(input_event["event"], "input.required");
    assert_eq!(input_event["prompt"], "Need detail");
    assert_eq!(
        manager.get_task(&handle.task_id).unwrap().state,
        TaskState::WaitingInput
    );

    let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
    let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(1);
    manager
        .set_hitl_senders(&handle.task_id, input_tx, approval_tx)
        .unwrap();
    manager
        .provide_input(&handle.task_id, "resume")
        .expect("input should resume task");
    let _running_event = recv_event(&mut rx).await;

    sink.emit(ActivityEvent::ToolApprovalRequired {
        tool_call_id: "tool-1".into(),
        name: "shell_exec".into(),
        arguments: json!({"command": "echo hi"}),
        reason: Some("tool execution requires approval".into()),
    });
    let state_event = recv_event(&mut rx).await;
    assert_eq!(state_event["to"], "waiting_approval");
    let approval_event = recv_event(&mut rx).await;
    assert_eq!(approval_event["event"], "tool.approval_required");
    assert_eq!(approval_event["tool_call_id"], "tool-1");
    assert_eq!(
        manager.get_task(&handle.task_id).unwrap().state,
        TaskState::WaitingApproval
    );
}

#[tokio::test]
async fn tool_output_events_translate_to_tool_output_with_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-3".to_string(), tx);

    sink.emit(ActivityEvent::ToolOutput {
        tool_call_id: "tool-2".to_string(),
        line: "line 1".to_string(),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "tool.output");
    assert_eq!(event["tool_call_id"], "tool-2");
    assert_eq!(event["line"], "line 1");
}

#[tokio::test]
async fn tool_finish_events_translate_to_tool_result_with_duration_and_error_state() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-4".to_string(), tx);

    sink.emit(ActivityEvent::ToolFinish {
        tool_call_id: "tool-3".to_string(),
        name: "shell_exec".to_string(),
        is_error: true,
        duration_ms: 451,
        exit_code: Some(23),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "tool.result");
    assert_eq!(event["tool_call_id"], "tool-3");
    assert_eq!(event["tool_name"], "shell_exec");
    assert_eq!(event["is_error"], true);
    assert_eq!(event["duration_ms"], 451);
}

#[tokio::test]
async fn think_events_translate_to_agent_thinking_events_with_monotonic_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-5".to_string(), tx);

    sink.emit(ActivityEvent::ThinkStart);
    sink.emit(ActivityEvent::ThinkDelta {
        text: "considering options".to_string(),
    });
    sink.emit(ActivityEvent::ThinkEnd {
        think_duration_ms: 91,
        think_tokens: 12,
    });

    let e1 = recv_event(&mut rx).await;
    assert_eq!(e1["event"], "agent.thinking");
    assert_eq!(e1["state"], "started");

    let e2 = recv_event(&mut rx).await;
    assert_eq!(e2["event"], "agent.thinking");
    assert_eq!(e2["content"], "considering options");

    let e3 = recv_event(&mut rx).await;
    assert_eq!(e3["event"], "agent.thinking");
    assert_eq!(e3["state"], "ended");
    assert_eq!(e3["duration_ms"], 91);
    assert_eq!(e3["tokens"], 12);

    // Verify monotonic seq.
    assert!(e1["seq"].as_u64() < e2["seq"].as_u64());
    assert!(e2["seq"].as_u64() < e3["seq"].as_u64());
}

#[tokio::test]
async fn child_spawned_events_translate_to_agent_child_spawned_with_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-6".to_string(), tx);

    sink.emit(ActivityEvent::ChildSpawned {
        child_id: "child-1".to_string(),
        agent_type: "reviewer".to_string(),
        task: "Review the patch".to_string(),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.child_spawned");
    assert_eq!(event["child_id"], "child-1");
    assert_eq!(event["agent_type"], "reviewer");
    assert_eq!(event["child_task"], "Review the patch");
}

#[tokio::test]
async fn child_finished_events_translate_to_agent_child_finished_with_seq() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-7".to_string(), tx);

    sink.emit(ActivityEvent::ChildFinished {
        child_id: "child-1".to_string(),
        agent_type: "reviewer".to_string(),
        exit_reason: "completed".to_string(),
        duration_ms: 225,
        tool_uses: 3,
        token_count: 120,
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.child_finished");
    assert_eq!(event["child_id"], "child-1");
    assert_eq!(event["exit_reason"], "completed");
    assert_eq!(event["duration_ms"], 225);
}

#[tokio::test]
async fn turn_complete_translates_to_agent_turn_complete() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-8".to_string(), tx);

    sink.emit(ActivityEvent::TurnComplete);

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.turn_complete");
    assert_eq!(event["task_id"], "task-8");
}

#[tokio::test]
async fn child_activity_is_flattened_with_child_attribution_added_to_the_inner_event() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-9".to_string(), tx);

    sink.emit(ActivityEvent::ChildActivity {
        child_id: "child-1".to_string(),
        agent_type: "researcher".to_string(),
        event: Box::new(ActivityEvent::Token {
            text: "child message".to_string(),
        }),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.message");
    assert_eq!(event["task_id"], "task-9");
    assert_eq!(event["content"], "child message");
    assert_eq!(event["child_id"], "child-1");
    assert_eq!(event["child_agent_type"], "researcher");
}

#[tokio::test]
async fn nested_child_activity_is_flattened_to_the_innermost_child_identity() {
    let (tx, mut rx) = make_channel(16);
    let sink = EngineActivitySink::new("task-10".to_string(), tx);

    sink.emit(ActivityEvent::ChildActivity {
        child_id: "child-outer".to_string(),
        agent_type: "planner".to_string(),
        event: Box::new(ActivityEvent::ChildActivity {
            child_id: "child-inner".to_string(),
            agent_type: "researcher".to_string(),
            event: Box::new(ActivityEvent::ToolOutput {
                tool_call_id: "tool-10".to_string(),
                line: "nested child output".to_string(),
            }),
        }),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "tool.output");
    assert_eq!(event["task_id"], "task-10");
    // Innermost child attribution preserved.
    assert_eq!(event["child_id"], "child-inner");
    assert_eq!(event["child_agent_type"], "researcher");
}

#[tokio::test]
async fn engine_activity_sink_events_are_observable_through_taskmanager_broadcast_subscribers() {
    let manager = simulacra_server::TaskManager::new();
    let handle = manager
        .create_task(
            &simulacra_server::TenantConfig {
                namespace: "acme".to_string(),
                agent_type: "worker".to_string(),
                vfs_root: std::path::PathBuf::from("/tmp/acme"),
                budget_pool: simulacra_server::BudgetPoolConfig {
                    max_tokens: 10_000,
                    max_cost: "25.00".to_string(),
                },
                hooks: vec![],
                integrations: vec![],
                mcp_servers: Default::default(),
            },
            "Emit events",
            None,
            serde_json::json!({}),
            None,
        )
        .expect("task should be created");
    let sender = manager
        .get_event_sender(&handle.task_id)
        .expect("task manager should expose a cloned sender");
    let (_history, mut rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("subscriber should attach to the task channel");
    let sink = EngineActivitySink::new(handle.task_id.clone(), sender);

    sink.emit(ActivityEvent::Token {
        text: "hello from engine".to_string(),
    });

    let event = recv_event(&mut rx).await;
    assert_eq!(event["event"], "agent.message");
    assert_eq!(event["task_id"], handle.task_id);
}

#[tokio::test]
async fn engine_activity_sink_emit_is_non_blocking_even_without_receivers() {
    let (tx, rx) = make_channel(16);
    drop(rx); // No receivers.
    let sink = EngineActivitySink::new("task-11".to_string(), tx);

    timeout(Duration::from_secs(1), async {
        sink.emit(ActivityEvent::Token {
            text: "fire and forget".to_string(),
        });
    })
    .await
    .expect("emit must not block when the broadcast channel has no receivers");
}

#[tokio::test]
async fn all_events_include_task_id_and_monotonic_seq() {
    let (tx, mut rx) = make_channel(32);
    let sink = EngineActivitySink::new("task-seq".to_string(), tx);

    sink.emit(ActivityEvent::Token { text: "a".into() });
    sink.emit(ActivityEvent::ToolStart {
        tool_call_id: "tc".into(),
        name: "t".into(),
        arguments: json!({}),
    });
    sink.emit(ActivityEvent::TurnComplete);

    let e1 = recv_event(&mut rx).await;
    let e2 = recv_event(&mut rx).await;
    let e3 = recv_event(&mut rx).await;

    // All have task_id.
    assert_eq!(e1["task_id"], "task-seq");
    assert_eq!(e2["task_id"], "task-seq");
    assert_eq!(e3["task_id"], "task-seq");

    // All have seq, and seq is monotonically increasing.
    let s1 = e1["seq"].as_u64().unwrap();
    let s2 = e2["seq"].as_u64().unwrap();
    let s3 = e3["seq"].as_u64().unwrap();
    assert!(s1 < s2);
    assert!(s2 < s3);
}
