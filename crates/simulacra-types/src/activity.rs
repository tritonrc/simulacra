//! Activity events for real-time observability of agent work.
//!
//! The `ActivityEvent` enum defines the event protocol that makes tool execution,
//! sub-agent delegation, and model thinking observable in real time.

use serde::{Deserialize, Serialize};

/// Events emitted during agent execution for real-time observability.
///
/// Consumers (CLI renderer, SSE server) subscribe to these events to display
/// activity blocks showing what the agent is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ActivityEvent {
    /// LLM token arrived (streaming response text).
    Token { text: String },

    /// Model has started an extended thinking block.
    ThinkStart,

    /// A chunk of thinking text arrived (streaming).
    ThinkDelta { text: String },

    /// Model thinking block has ended.
    ThinkEnd {
        /// Thinking duration in milliseconds.
        think_duration_ms: u64,
        /// Approximate token count of thinking content.
        think_tokens: u64,
    },

    /// A tool call has started.
    ToolStart {
        tool_call_id: String,
        name: String,
        /// Full arguments. Display layer truncates for rendering.
        arguments: serde_json::Value,
    },

    /// A tool call is waiting for human approval before execution starts.
    ToolApprovalRequired {
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
        reason: Option<String>,
    },

    /// A provider streamed part of a tool-call argument payload.
    ///
    /// Display-only; the actual tool execution starts at `ToolStart`.
    ToolCallDelta {
        index: u64,
        tool_call_id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },

    /// A line of output from a running tool (e.g. shell stdout/stderr).
    ToolOutput { tool_call_id: String, line: String },

    /// The agent is waiting for user-provided input.
    InputRequired {
        prompt: String,
        schema: Option<serde_json::Value>,
    },

    /// A tool call has finished.
    ToolFinish {
        tool_call_id: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
        /// Optional exit code (for shell tools).
        exit_code: Option<i32>,
    },

    /// A child agent has been spawned.
    ChildSpawned {
        child_id: String,
        agent_type: String,
        task: String,
    },

    /// A forwarded event from a running child agent.
    ChildActivity {
        child_id: String,
        agent_type: String,
        event: Box<ActivityEvent>,
    },

    /// A child agent has finished.
    ChildFinished {
        child_id: String,
        agent_type: String,
        exit_reason: String,
        duration_ms: u64,
        tool_uses: u32,
        token_count: u64,
    },

    /// The agent turn has completed.
    TurnComplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_nested_child_activity() {
        let inner = ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell_exec".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        };
        let wrapped = ActivityEvent::ChildActivity {
            child_id: "child-1".into(),
            agent_type: "researcher".into(),
            event: Box::new(ActivityEvent::ChildActivity {
                child_id: "grandchild-1".into(),
                agent_type: "coder".into(),
                event: Box::new(inner),
            }),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }
}
