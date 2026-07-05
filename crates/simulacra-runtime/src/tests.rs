use super::*;
use simulacra_types::{
    AgentId, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalStorage, Message, Role,
    TokenUsage,
};

fn make_session(id: &str) -> Session {
    Session {
        id: id.to_string(),
        agent_id: AgentId("agent-1".into()),
        messages: vec![Message {
            role: Role::User,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }],
        vfs_snapshot: None,
        created_at: 1000,
        used_tokens: 0,
        used_turns: 0,
    }
}

#[test]
fn session_save_load_roundtrip() {
    let storage = InMemorySessionStorage::new();
    let session = make_session("sess-1");
    storage.save(&session).unwrap();

    let loaded = storage.load("sess-1").unwrap().expect("session not found");
    assert_eq!(loaded.id, "sess-1");
    assert_eq!(loaded.agent_id, AgentId("agent-1".into()));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.created_at, 1000);
}

#[test]
fn session_load_missing_returns_none() {
    let storage = InMemorySessionStorage::new();
    let result = storage.load("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn session_save_overwrites() {
    let storage = InMemorySessionStorage::new();
    let mut session = make_session("sess-1");
    storage.save(&session).unwrap();

    session.messages.push(Message {
        role: Role::Assistant,
        content: "world".into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_content: vec![],
    });
    storage.save(&session).unwrap();

    let loaded = storage.load("sess-1").unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2);
}

fn make_journal_entry(agent_id: &str, kind: JournalEntryKind) -> JournalEntry {
    JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: AgentId(agent_id.into()),
        timestamp_ms: 1000,
        entry: kind,
    }
}

#[test]
fn journal_append_and_read_all_roundtrip() {
    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());

    storage
        .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
        .unwrap();
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::ShellCommand {
                command: "echo hi".into(),
                exit_code: 0,
            },
        ))
        .unwrap();
    // Different agent — should not appear in query
    storage
        .append(make_journal_entry("agent-2", JournalEntryKind::TurnStart))
        .unwrap();

    let entries = storage.read_all(&agent).unwrap();
    assert_eq!(entries.len(), 2);
}

#[test]
fn journal_query_token_usage() {
    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());

    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmResponse {
                model: "gpt-4".into(),
                token_usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .unwrap();
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmResponse {
                model: "gpt-4".into(),
                token_usage: TokenUsage {
                    input_tokens: 200,
                    output_tokens: 75,
                },
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .unwrap();
    // Different agent
    storage
        .append(make_journal_entry(
            "agent-2",
            JournalEntryKind::LlmResponse {
                model: "gpt-4".into(),
                token_usage: TokenUsage {
                    input_tokens: 999,
                    output_tokens: 999,
                },
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .unwrap();

    let usage = storage.query_token_usage(&agent).unwrap();
    assert_eq!(usage.input_tokens, 300);
    assert_eq!(usage.output_tokens, 125);
    assert_eq!(usage.total(), 425);
}

#[test]
fn journal_query_token_usage_no_entries() {
    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());
    let usage = storage.query_token_usage(&agent).unwrap();
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
}

// -----------------------------------------------------------------------
// S005: Checkpoint + fork creates independent journal sharing history
// -----------------------------------------------------------------------
#[test]
fn checkpoint_fork_creates_independent_journal() {
    use rust_decimal::Decimal;
    use simulacra_types::{CheckpointData, ResourceBudget};

    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());

    // Append some entries before the checkpoint
    storage
        .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
        .unwrap();
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmRequest {
                model: "gpt-4".into(),
                message_count: 2,
            },
        ))
        .unwrap();

    // Save a checkpoint at index 2 (after the 2 entries above)
    let checkpoint_data = CheckpointData {
        messages: vec![Message {
            role: Role::User,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        }],
        budget_snapshot: ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5),
        vfs_snapshot: None,
    };
    storage.save_checkpoint(&agent, 2, checkpoint_data).unwrap();

    // Append more entries after the checkpoint
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmResponse {
                model: "gpt-4".into(),
                token_usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .unwrap();

    // Fork from the checkpoint (index 2 — the checkpoint entry)
    let forked = storage.fork_from(&agent, 2).unwrap();

    // Forked journal shares history up to and including the checkpoint
    assert_eq!(forked.len(), 3); // TurnStart + LlmRequest + Checkpoint
    assert!(matches!(forked[0].entry, JournalEntryKind::TurnStart));
    assert!(matches!(
        forked[1].entry,
        JournalEntryKind::LlmRequest { .. }
    ));
    assert!(matches!(
        forked[2].entry,
        JournalEntryKind::Checkpoint { .. }
    ));

    // The post-checkpoint entry (LlmResponse) is NOT in the forked journal
    // Original journal has 4 entries for this agent
    let all = storage.read_all(&agent).unwrap();
    assert_eq!(all.len(), 4);

    // Forked journal is independent — only 3 entries
    assert_eq!(forked.len(), 3);
}

// -----------------------------------------------------------------------
// S005: Schema version mismatch produces clear error
// -----------------------------------------------------------------------
#[test]
fn schema_version_mismatch_produces_error() {
    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());

    // Append an entry with a future schema version
    let future_entry = JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION + 1,
        agent_id: agent.clone(),
        timestamp_ms: 1000,
        entry: JournalEntryKind::TurnStart,
    };
    // append itself does not validate — it's the storage write path
    storage.append(future_entry).unwrap();

    // read_all should detect the version mismatch
    let result = storage.read_all(&agent);
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        simulacra_types::JournalError::SchemaVersionMismatch { expected, got } => {
            assert_eq!(expected, JOURNAL_SCHEMA_VERSION);
            assert_eq!(got, JOURNAL_SCHEMA_VERSION + 1);
        }
        other => panic!("expected SchemaVersionMismatch, got: {other}"),
    }
}

// -----------------------------------------------------------------------
// S005: Replay from checkpoint skips entries before checkpoint
// -----------------------------------------------------------------------
#[test]
fn replay_from_checkpoint_skips_earlier_entries() {
    use rust_decimal::Decimal;
    use simulacra_types::{CheckpointData, ResourceBudget};

    let storage = InMemoryJournalStorage::new();
    let agent = AgentId("agent-1".into());

    // 3 entries before the checkpoint
    storage
        .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
        .unwrap();
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmRequest {
                model: "m".into(),
                message_count: 1,
            },
        ))
        .unwrap();
    storage
        .append(make_journal_entry(
            "agent-1",
            JournalEntryKind::LlmResponse {
                model: "m".into(),
                token_usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .unwrap();

    // Checkpoint at index 3
    let checkpoint_data = CheckpointData {
        messages: vec![],
        budget_snapshot: ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5),
        vfs_snapshot: None,
    };
    storage.save_checkpoint(&agent, 3, checkpoint_data).unwrap();

    // 1 entry after the checkpoint
    storage
        .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
        .unwrap();

    // read_from starting after the checkpoint (index 4) skips everything before
    let entries = storage.read_from(&agent, 4).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));

    // read_from starting at the checkpoint itself (index 3) includes checkpoint + after
    let entries = storage.read_from(&agent, 3).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        entries[0].entry,
        JournalEntryKind::Checkpoint { .. }
    ));
}
