use serde_json::Value;
use simulacra_runtime::ActivitySink;
use simulacra_server::{EngineActivitySink, TaskEventChannel};
use simulacra_types::ActivityEvent;

fn emit(event: ActivityEvent) -> Value {
    let channel = TaskEventChannel::new(8);
    let sink = EngineActivitySink::new("root-task".into(), channel.clone());
    sink.emit(event);
    let (history, _receiver) = channel.subscribe_with_history();
    assert_eq!(
        history.len(),
        1,
        "one runtime event should make one projection"
    );
    history.into_iter().next().expect("projected event")
}

fn assert_no_role_fields(value: &Value) {
    assert!(
        value.get("agent_type").is_none(),
        "projection leaked agent_type: {value}"
    );
    assert!(
        value.get("child_agent_type").is_none(),
        "projection leaked child_agent_type: {value}"
    );
}

#[test]
fn s060_a33_top_level_child_lifecycle_projection_uses_placement() {
    const CHILD_ID: &str = "child-0123456789abcdef0123456789abcdef";
    let spawned = emit(ActivityEvent::ChildSpawned {
        child_id: CHILD_ID.into(),
        placement: "workspace".into(),
        task: "  preserve projected task \n".into(),
    });
    assert_eq!(spawned["event"], "agent.child_spawned");
    assert_eq!(spawned["child_id"], CHILD_ID);
    assert_eq!(spawned["placement"], "workspace");
    assert_eq!(spawned["child_task"], "  preserve projected task \n");
    assert_no_role_fields(&spawned);

    let finished = emit(ActivityEvent::ChildFinished {
        child_id: CHILD_ID.into(),
        placement: "workspace".into(),
        exit_reason: "completed".into(),
        duration_ms: 5,
        tool_uses: 1,
        token_count: 13,
    });
    assert_eq!(finished["event"], "agent.child_finished");
    assert_eq!(finished["child_id"], CHILD_ID);
    assert_eq!(finished["placement"], "workspace");
    assert_no_role_fields(&finished);
}

#[test]
fn s060_a33_recursive_child_projection_uses_child_placement() {
    const MIDDLE_ID: &str = "child-11111111111111111111111111111111";
    const LEAF_ID: &str = "child-22222222222222222222222222222222";
    let projected = emit(ActivityEvent::ChildActivity {
        child_id: MIDDLE_ID.into(),
        placement: "middle_native".into(),
        event: Box::new(ActivityEvent::ChildActivity {
            child_id: LEAF_ID.into(),
            placement: "leaf_native".into(),
            event: Box::new(ActivityEvent::ToolOutput {
                tool_call_id: "leaf-tool-output".into(),
                line: "leaf evidence".into(),
            }),
        }),
    });

    assert_eq!(projected["event"], "tool.output");
    assert_eq!(projected["task_id"], "root-task");
    assert_eq!(projected["tool_call_id"], "leaf-tool-output");
    assert_eq!(projected["child_id"], LEAF_ID);
    assert_eq!(projected["child_placement"], "leaf_native");
    assert_ne!(projected["child_id"], MIDDLE_ID);
    assert_no_role_fields(&projected);
}
