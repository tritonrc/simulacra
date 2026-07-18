use super::*;

// ---------------------------------------------------------------------------
// JSON serialization for child terminal/status/wait results
// ---------------------------------------------------------------------------
//
// These shape the JSON returned to the LLM by the child-control tools. They
// intentionally surface a stable, minimal projection of the underlying
// supervisor types.

fn child_success_json(terminal: ChildTerminalResult, output: AgentLoopOutput) -> serde_json::Value {
    let exit_reason_str = exit_reason_to_snake_case(&output.exit_reason);
    let message = crate::supervisor::final_assistant_message(&output).unwrap_or_default();

    serde_json::json!({
        "child_id": terminal.child_id.0,
        "agent_type": terminal.agent_type,
        "status": terminal.status,
        "ready": true,
        "exit_reason": exit_reason_str,
        "message": message,
        "elapsed_ms": terminal.elapsed_ms,
        "tool_uses": terminal.tool_uses,
        "token_usage": {
            "input_tokens": output.token_usage.input_tokens,
            "output_tokens": output.token_usage.output_tokens
        },
        "artifacts": [],
        "vfs_changes": []
    })
}

fn child_failed_json(terminal: ChildTerminalResult, error: String) -> serde_json::Value {
    serde_json::json!({
        "child_id": terminal.child_id.0,
        "agent_type": terminal.agent_type,
        "status": terminal.status,
        "ready": true,
        "exit_reason": terminal.status,
        "message": error,
        "elapsed_ms": terminal.elapsed_ms,
        "tool_uses": terminal.tool_uses,
        "token_usage": {
            "input_tokens": 0,
            "output_tokens": 0
        },
        "artifacts": [],
        "vfs_changes": []
    })
}

pub(super) fn child_terminal_json(
    terminal: ChildTerminalResult,
    status_override: Option<String>,
) -> serde_json::Value {
    let mut json = match terminal.result.clone() {
        Ok(output) => child_success_json(terminal, output),
        Err(error) => child_failed_json(terminal, error),
    };
    if let serde_json::Value::Object(ref mut object) = json
        && let Some(status) = status_override
    {
        object.insert("status".to_string(), serde_json::Value::String(status));
    }
    json
}

pub(super) fn child_status_json(status: ChildStatus) -> serde_json::Value {
    serde_json::json!({
        "child_id": status.child_id.0,
        "agent_type": status.agent_type,
        "status": status.status,
        "ready": status.ready,
        "elapsed_ms": status.elapsed_ms
    })
}

pub(super) fn list_children_json(children: Vec<ChildRosterEntry>) -> serde_json::Value {
    serde_json::Value::Array(
        children
            .into_iter()
            .map(|child| {
                serde_json::json!({
                    "child_id": child.child_id,
                    "agent_type": child.agent_type,
                    "task": child.task,
                    "status": child.status,
                    "ready": child.ready,
                    "elapsed_ms": child.elapsed_ms
                })
            })
            .collect(),
    )
}

pub(super) fn wait_child_json(wait: WaitChildResult) -> serde_json::Value {
    if let Some(terminal) = wait.terminal {
        child_terminal_json(terminal, Some(wait.status))
    } else {
        serde_json::json!({
            "child_id": wait.child_id.0,
            "status": "running",
            "ready": false
        })
    }
}

pub(super) fn wait_children_json(wait: WaitChildrenResult) -> serde_json::Value {
    if let Some(terminal) = wait.terminal {
        child_terminal_json(terminal, Some(wait.status))
    } else {
        serde_json::json!({
            "child_ids": wait.child_ids.into_iter().map(|child_id| child_id.0).collect::<Vec<_>>(),
            "status": "running",
            "ready": false
        })
    }
}
