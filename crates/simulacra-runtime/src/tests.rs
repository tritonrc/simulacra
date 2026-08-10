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

fn token_usage_from_json(value: serde_json::Value) -> TokenUsage {
    serde_json::from_value(value).expect("token usage fixture should deserialize")
}

fn token_usage_json(usage: &TokenUsage) -> serde_json::Value {
    serde_json::to_value(usage).expect("token usage should serialize")
}

fn append_cached_llm_response(
    storage: &dyn JournalStorage,
    agent_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: u64,
) {
    storage
        .append(make_journal_entry(
            agent_id,
            JournalEntryKind::LlmResponse {
                model: "gpt-4o-mini".into(),
                token_usage: token_usage_from_json(serde_json::json!({
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_read_input_tokens": cache_read_input_tokens,
                    "cache_write_input_tokens": cache_write_input_tokens
                })),
                finish_reason: "stop".into(),
                assistant_message: None,
            },
        ))
        .expect("cached LlmResponse should append");
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
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
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
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
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
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
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

fn assert_s059_journal_cache_roundtrip_and_query(storage: &dyn JournalStorage) {
    let agent = AgentId("agent-s059".into());

    append_cached_llm_response(storage, "agent-s059", 100, 25, 40, 5);
    append_cached_llm_response(storage, "agent-s059", 20, 7, 3, 11);
    append_cached_llm_response(storage, "other-agent", 999, 999, 999, 999);

    let entries = storage
        .read_all(&agent)
        .expect("journal entries should read back");
    assert_eq!(entries.len(), 2);
    match &entries[0].entry {
        JournalEntryKind::LlmResponse { token_usage, .. } => {
            let usage = token_usage_json(token_usage);
            assert_eq!(
                usage["cache_read_input_tokens"], 40,
                "LlmResponse read path must preserve cache reads"
            );
            assert_eq!(
                usage["cache_write_input_tokens"], 5,
                "LlmResponse read path must preserve cache writes"
            );
        }
        other => panic!("expected LlmResponse, got {other:?}"),
    }

    let totals = storage
        .query_token_usage(&agent)
        .expect("journal token usage query should succeed");
    assert_eq!(totals.input_tokens, 120);
    assert_eq!(totals.output_tokens, 32);
    assert_eq!(totals.total(), 152);
    let totals_json = token_usage_json(&totals);
    assert_eq!(
        totals_json["cache_read_input_tokens"], 43,
        "journal queries must independently aggregate cache reads"
    );
    assert_eq!(
        totals_json["cache_write_input_tokens"], 16,
        "journal queries must independently aggregate cache writes"
    );
}

fn assert_s059_extreme_journal_cache_aggregation_saturates(
    storage: &dyn JournalStorage,
    agent_id: &str,
    cache_reads: bool,
) {
    let (first_read, second_read, first_write, second_write) = if cache_reads {
        (u64::MAX, 1, 0, 0)
    } else {
        (0, 0, u64::MAX, 1)
    };
    append_cached_llm_response(storage, agent_id, 0, 0, first_read, first_write);
    append_cached_llm_response(storage, agent_id, 0, 0, second_read, second_write);

    let totals = storage
        .query_token_usage(&AgentId(agent_id.into()))
        .expect("extreme journal cache counters must aggregate without panic or wrap");
    let totals = token_usage_json(&totals);
    let field = if cache_reads {
        "cache_read_input_tokens"
    } else {
        "cache_write_input_tokens"
    };
    assert_eq!(
        totals[field],
        serde_json::json!(u64::MAX),
        "{field} journal aggregation must saturate"
    );
}

fn assert_s059_extreme_journal_logical_aggregation_saturates(
    storage: &dyn JournalStorage,
    agent_id: &str,
    logical_input: bool,
) {
    let (first_input, second_input, first_output, second_output) = if logical_input {
        (u64::MAX - 4, 10, 0, 0)
    } else {
        (10, 10, u64::MAX - 4, 10)
    };
    append_cached_llm_response(storage, agent_id, first_input, first_output, 3, 1);
    append_cached_llm_response(storage, agent_id, second_input, second_output, 2, 1);

    let totals = storage
        .query_token_usage(&AgentId(agent_id.into()))
        .expect("extreme logical journal counters must aggregate without panic or wrap");
    let logical_total = if logical_input {
        totals.input_tokens
    } else {
        totals.output_tokens
    };
    assert_eq!(
        logical_total,
        u64::MAX,
        "logical journal aggregation must saturate"
    );
    assert_eq!(totals.total(), u64::MAX);
    assert_eq!(
        totals.cache_read_input_tokens, 5,
        "cache reads must aggregate independently without entering the logical total"
    );
    assert_eq!(
        totals.cache_write_input_tokens, 2,
        "cache writes must aggregate independently without entering the logical total"
    );
}

#[test]
fn s059_in_memory_journal_preserves_and_aggregates_cache_counters() {
    let storage = InMemoryJournalStorage::new();
    assert_s059_journal_cache_roundtrip_and_query(&storage);
}

#[test]
fn s059_in_memory_journal_cache_read_aggregation_saturates() {
    let storage = InMemoryJournalStorage::new();
    assert_s059_extreme_journal_cache_aggregation_saturates(
        &storage,
        "agent-s059-extreme-read",
        true,
    );
}

#[test]
fn s059_in_memory_journal_cache_write_aggregation_saturates() {
    let storage = InMemoryJournalStorage::new();
    assert_s059_extreme_journal_cache_aggregation_saturates(
        &storage,
        "agent-s059-extreme-write",
        false,
    );
}

#[test]
fn s059_in_memory_journal_logical_input_aggregation_saturates() {
    let storage = InMemoryJournalStorage::new();
    assert_s059_extreme_journal_logical_aggregation_saturates(
        &storage,
        "agent-s059-extreme-logical-input",
        true,
    );
}

#[test]
fn s059_in_memory_journal_output_aggregation_saturates() {
    let storage = InMemoryJournalStorage::new();
    assert_s059_extreme_journal_logical_aggregation_saturates(
        &storage,
        "agent-s059-extreme-output",
        false,
    );
}

#[test]
fn s059_sqlite_journal_preserves_and_aggregates_cache_counters() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    assert_s059_journal_cache_roundtrip_and_query(&storage);
}

#[test]
fn s059_sqlite_journal_cache_read_aggregation_saturates() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    assert_s059_extreme_journal_cache_aggregation_saturates(
        &storage,
        "agent-s059-extreme-read",
        true,
    );
}

#[test]
fn s059_sqlite_journal_cache_write_aggregation_saturates() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    assert_s059_extreme_journal_cache_aggregation_saturates(
        &storage,
        "agent-s059-extreme-write",
        false,
    );
}

#[test]
fn s059_sqlite_journal_logical_input_aggregation_saturates() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    assert_s059_extreme_journal_logical_aggregation_saturates(
        &storage,
        "agent-s059-extreme-logical-input",
        true,
    );
}

#[test]
fn s059_sqlite_journal_output_aggregation_saturates() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    assert_s059_extreme_journal_logical_aggregation_saturates(
        &storage,
        "agent-s059-extreme-output",
        false,
    );
}

#[test]
fn s059_legacy_llm_response_json_deserializes_with_zero_cache_counters() {
    let entry: JournalEntryKind = serde_json::from_value(serde_json::json!({
        "type": "LlmResponse",
        "model": "legacy-model",
        "token_usage": {
            "input_tokens": 8,
            "output_tokens": 5
        },
        "finish_reason": "stop"
    }))
    .expect("legacy LlmResponse JSON should deserialize");

    match entry {
        JournalEntryKind::LlmResponse { token_usage, .. } => {
            let usage = token_usage_json(&token_usage);
            assert_eq!(usage["cache_read_input_tokens"], 0);
            assert_eq!(usage["cache_write_input_tokens"], 0);
            assert_eq!(token_usage.total(), 13);
        }
        other => panic!("expected LlmResponse, got {other:?}"),
    }
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
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
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

    // Strict v3 rejects an entry with a future schema version at append.
    let future_entry = JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION + 1,
        agent_id: agent.clone(),
        timestamp_ms: 1000,
        entry: JournalEntryKind::TurnStart,
    };
    let err = storage
        .append(future_entry)
        .expect_err("future schema must fail at append");
    match err {
        simulacra_types::JournalError::SchemaVersionMismatch { expected, got } => {
            assert_eq!(expected, JOURNAL_SCHEMA_VERSION);
            assert_eq!(got, JOURNAL_SCHEMA_VERSION + 1);
        }
        other => panic!("expected SchemaVersionMismatch, got: {other}"),
    }
    assert!(
        storage
            .read_all(&agent)
            .expect("rejected append must leave a readable stream")
            .is_empty()
    );
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
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
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
