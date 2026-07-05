use crate::{AgentId, Message, ResourceBudget, TokenUsage};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema version for journal entries. Increments when the enum changes.
pub const JOURNAL_SCHEMA_VERSION: u32 = 2;

/// A single journal entry. Append-only, schema-versioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub agent_id: AgentId,
    pub timestamp_ms: u64,
    pub entry: JournalEntryKind,
}

/// Full-state snapshot taken at a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointData {
    pub messages: Vec<Message>,
    pub budget_snapshot: ResourceBudget,
    pub vfs_snapshot: Option<Vec<u8>>,
}

/// Injectable clock for deterministic replay.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

/// Real wall-clock implementation.
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// The kind of journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JournalEntryKind {
    TurnStart,
    LlmRequest {
        model: String,
        message_count: usize,
    },
    LlmResponse {
        model: String,
        token_usage: TokenUsage,
        finish_reason: String,
        /// Full assistant message for replay (includes tool_calls).
        #[serde(default)]
        assistant_message: Option<Message>,
    },
    ToolCall {
        /// Provider tool-call id. Optional for journals written before this
        /// field existed and for any non-provider synthetic entries.
        #[serde(default)]
        tool_call_id: Option<String>,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        /// Provider tool-call id for the top-level result corresponding to a
        /// ToolCall. Nested sandbox side-effect entries leave this unset.
        #[serde(default)]
        tool_call_id: Option<String>,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    ShellCommand {
        command: String,
        exit_code: i32,
    },
    CodeExecution {
        language: String,
    },
    SubAgentSpawned {
        child_id: AgentId,
        agent_type: String,
        /// Full inline prompt for generic sub-agents. `None` for configured
        /// agent types and legacy journal entries.
        #[serde(default)]
        system_prompt: Option<String>,
    },
    SubAgentCompleted {
        child_id: AgentId,
        success: bool,
    },
    FileWrite {
        path: String,
        size_bytes: u64,
    },
    FileDelete {
        path: String,
    },
    FileMove {
        from: String,
        to: String,
    },
    HttpRequest {
        method: String,
        url: String,
        status: u16,
    },
    Checkpoint {
        snapshot_data: Vec<u8>,
    },
    HookDenial {
        hook_name: String,
        operation: String,
        reason: String,
    },
    HookKill {
        hook_name: String,
        operation: String,
        reason: String,
    },
}

/// Storage backend for journal entries. Object-safe.
pub trait JournalStorage: Send + Sync + 'static {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError>;
    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError>;
    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError>;

    /// Save a checkpoint after the given entry index.
    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError>;

    /// Fork from a checkpoint: return entries from the checkpoint onward (inclusive).
    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError>;

    /// Read entries starting from `start_index` (for replay-from-checkpoint).
    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError>;
}

/// Errors from journal operations.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("schema version mismatch: expected {expected}, got {got}")]
    SchemaVersionMismatch { expected: u32, got: u32 },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid checkpoint index: {0}")]
    InvalidCheckpointIndex(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentId, Message, Role, TokenUsage};

    fn make_entry(kind: JournalEntryKind) -> JournalEntry {
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1700000000000,
            entry: kind,
        }
    }

    #[test]
    fn roundtrip_turn_start() {
        let entry = make_entry(JournalEntryKind::TurnStart);
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.schema_version, JOURNAL_SCHEMA_VERSION);
        assert_eq!(decoded.agent_id.0, "test-agent");
        assert_eq!(decoded.timestamp_ms, 1700000000000);
    }

    #[test]
    fn roundtrip_llm_request() {
        let entry = make_entry(JournalEntryKind::LlmRequest {
            model: "gpt-4".into(),
            message_count: 5,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::LlmRequest {
                model,
                message_count,
            } => {
                assert_eq!(model, "gpt-4");
                assert_eq!(*message_count, 5);
            }
            other => panic!("expected LlmRequest, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_llm_response_with_assistant_message() {
        let msg = Message {
            role: Role::Assistant,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        };
        let entry = make_entry(JournalEntryKind::LlmResponse {
            model: "claude-3".into(),
            token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
            finish_reason: "stop".into(),
            assistant_message: Some(msg),
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::LlmResponse {
                model,
                token_usage,
                finish_reason,
                assistant_message,
            } => {
                assert_eq!(model, "claude-3");
                assert_eq!(token_usage.input_tokens, 100);
                assert_eq!(token_usage.output_tokens, 50);
                assert_eq!(finish_reason, "stop");
                assert!(assistant_message.is_some());
            }
            other => panic!("expected LlmResponse, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_llm_response_without_assistant_message() {
        let entry = make_entry(JournalEntryKind::LlmResponse {
            model: "claude-3".into(),
            token_usage: TokenUsage::default(),
            finish_reason: "length".into(),
            assistant_message: None,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::LlmResponse {
                assistant_message, ..
            } => {
                assert!(assistant_message.is_none());
            }
            other => panic!("expected LlmResponse, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_tool_call() {
        let entry = make_entry(JournalEntryKind::ToolCall {
            tool_call_id: Some("tc-1".into()),

            tool_name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/test.rs"}),
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                assert_eq!(tool_call_id.as_deref(), Some("tc-1"));
                assert_eq!(tool_name, "read_file");
                assert_eq!(arguments["path"], "/tmp/test.rs");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_tool_result() {
        let entry = make_entry(JournalEntryKind::ToolResult {
            tool_call_id: Some("tc-1".into()),

            tool_name: "read_file".into(),
            content: "file contents here".into(),
            is_error: false,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id.as_deref(), Some("tc-1"));
                assert_eq!(tool_name, "read_file");
                assert_eq!(content, "file contents here");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_tool_result_error() {
        let entry = make_entry(JournalEntryKind::ToolResult {
            tool_call_id: None,

            tool_name: "exec".into(),
            content: "permission denied".into(),
            is_error: true,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::ToolResult { is_error, .. } => {
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_shell_command() {
        let entry = make_entry(JournalEntryKind::ShellCommand {
            command: "ls -la".into(),
            exit_code: 0,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::ShellCommand { command, exit_code } => {
                assert_eq!(command, "ls -la");
                assert_eq!(*exit_code, 0);
            }
            other => panic!("expected ShellCommand, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_sub_agent_spawned_and_completed() {
        let spawned = make_entry(JournalEntryKind::SubAgentSpawned {
            child_id: AgentId("child-1".into()),
            agent_type: "reviewer".into(),
            system_prompt: None,
        });
        let completed = make_entry(JournalEntryKind::SubAgentCompleted {
            child_id: AgentId("child-1".into()),
            success: true,
        });
        for entry in [spawned, completed] {
            let json = serde_json::to_string(&entry).unwrap();
            let _decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn roundtrip_file_write() {
        let entry = make_entry(JournalEntryKind::FileWrite {
            path: "/workspace/src/main.rs".into(),
            size_bytes: 4096,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::FileWrite { path, size_bytes } => {
                assert_eq!(path, "/workspace/src/main.rs");
                assert_eq!(*size_bytes, 4096);
            }
            other => panic!("expected FileWrite, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_file_delete_and_move() {
        let delete = make_entry(JournalEntryKind::FileDelete {
            path: "/workspace/old.rs".into(),
        });
        let move_entry = make_entry(JournalEntryKind::FileMove {
            from: "/workspace/old.rs".into(),
            to: "/workspace/new.rs".into(),
        });

        let decoded_delete: JournalEntry =
            serde_json::from_str(&serde_json::to_string(&delete).unwrap()).unwrap();
        match &decoded_delete.entry {
            JournalEntryKind::FileDelete { path } => {
                assert_eq!(path, "/workspace/old.rs");
            }
            other => panic!("expected FileDelete, got {other:?}"),
        }

        let decoded_move: JournalEntry =
            serde_json::from_str(&serde_json::to_string(&move_entry).unwrap()).unwrap();
        match &decoded_move.entry {
            JournalEntryKind::FileMove { from, to } => {
                assert_eq!(from, "/workspace/old.rs");
                assert_eq!(to, "/workspace/new.rs");
            }
            other => panic!("expected FileMove, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_http_request() {
        let entry = make_entry(JournalEntryKind::HttpRequest {
            method: "POST".into(),
            url: "https://api.example.com/v1/chat".into(),
            status: 200,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::HttpRequest {
                method,
                url,
                status,
            } => {
                assert_eq!(method, "POST");
                assert_eq!(url, "https://api.example.com/v1/chat");
                assert_eq!(*status, 200);
            }
            other => panic!("expected HttpRequest, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_checkpoint() {
        let entry = make_entry(JournalEntryKind::Checkpoint {
            snapshot_data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::Checkpoint { snapshot_data } => {
                assert_eq!(snapshot_data, &[0xDE, 0xAD, 0xBE, 0xEF]);
            }
            other => panic!("expected Checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_code_execution() {
        let entry = make_entry(JournalEntryKind::CodeExecution {
            language: "python".into(),
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::CodeExecution { language } => {
                assert_eq!(language, "python");
            }
            other => panic!("expected CodeExecution, got {other:?}"),
        }
    }

    #[test]
    fn schema_version_is_preserved_in_serialization() {
        let entry = make_entry(JournalEntryKind::TurnStart);
        let json = serde_json::to_string(&entry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], JOURNAL_SCHEMA_VERSION);
    }

    #[test]
    fn empty_strings_roundtrip_correctly() {
        let entry = make_entry(JournalEntryKind::ToolResult {
            tool_call_id: None,

            tool_name: "".into(),
            content: "".into(),
            is_error: false,
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::ToolResult {
                tool_name, content, ..
            } => {
                assert_eq!(tool_name, "");
                assert_eq!(content, "");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn large_snapshot_data_roundtrips() {
        let large_data = vec![42u8; 1_000_000];
        let entry = make_entry(JournalEntryKind::Checkpoint {
            snapshot_data: large_data.clone(),
        });
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: JournalEntry = serde_json::from_str(&json).unwrap();
        match &decoded.entry {
            JournalEntryKind::Checkpoint { snapshot_data } => {
                assert_eq!(snapshot_data.len(), 1_000_000);
                assert_eq!(*snapshot_data, large_data);
            }
            other => panic!("expected Checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn tagged_enum_uses_type_field_in_json() {
        let entry = make_entry(JournalEntryKind::TurnStart);
        let json = serde_json::to_string(&entry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["entry"]["type"], "TurnStart");
    }

    #[test]
    fn system_clock_returns_nonzero_timestamp() {
        let clock = SystemClock;
        let ts = clock.now_ms();
        assert!(ts > 0, "SystemClock should return a nonzero timestamp");
    }
}
