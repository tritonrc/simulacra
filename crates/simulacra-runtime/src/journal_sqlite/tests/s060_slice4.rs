use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone)]
struct CapturedSqliteEvent {
    level: String,
    fields: HashMap<String, String>,
}

struct SqliteEventCapture {
    events: Arc<Mutex<Vec<CapturedSqliteEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for SqliteEventCapture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visitor<'a>(&'a mut HashMap<String, String>);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }

            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().into(), value.to_string());
            }
        }
        let mut fields = HashMap::new();
        event.record(&mut Visitor(&mut fields));
        self.events
            .lock()
            .expect("SQLite event capture lock")
            .push(CapturedSqliteEvent {
                level: event.metadata().level().to_string(),
                fields,
            });
    }
}

fn capture_sqlite_events() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSqliteEvent>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(SqliteEventCapture {
        events: Arc::clone(&events),
    });
    (subscriber, events)
}

fn assert_mismatch<T>(result: Result<T, JournalError>, got: u32) {
    assert!(matches!(
        result,
        Err(JournalError::SchemaVersionMismatch {
            expected: 3,
            got: actual
        }) if actual == got
    ));
}

fn seeded(version: u32) -> (SqliteJournalStorage, AgentId) {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal");
    let agent_id = AgentId(format!("sqlite-schema-{version}"));
    let conn = storage.conn.lock().expect("SQLite journal lock");
    conn.execute(
        "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json) VALUES (?1, ?2, 1, ?3)",
        rusqlite::params![agent_id.0, version, r#"{"type":"DefinitelyNotAJournalVariant"}"#],
    )
    .expect("seed raw wrong-version journal row");
    drop(conn);
    (storage, agent_id)
}

fn checkpoint_data() -> CheckpointData {
    CheckpointData {
        messages: vec![],
        budget_snapshot: ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        vfs_snapshot: None,
    }
}

fn row_count(storage: &SqliteJournalStorage, agent_id: &AgentId) -> usize {
    storage
        .conn
        .lock()
        .expect("SQLite journal lock")
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE agent_id = ?1",
            rusqlite::params![agent_id.0],
            |row| row.get(0),
        )
        .expect("count raw SQLite journal rows")
}

#[test]
fn s060_a34_sqlite_checks_version_before_payload_decode_on_every_read_path() {
    let _schema_guard = SCHEMA_MISMATCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for version in [2, 4] {
        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.read_all(&agent_id), version);

        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.query_token_usage(&agent_id), version);

        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.fork_from(&agent_id, 0), version);

        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.read_from(&agent_id, 0), version);
    }
}

#[test]
fn s060_a34_sqlite_save_checkpoint_rejects_v2_and_v4_corrupted_streams() {
    let _schema_guard = SCHEMA_MISMATCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for version in [2, 4] {
        let (storage, agent_id) = seeded(version);
        assert_mismatch(
            storage.save_checkpoint(&agent_id, 1, checkpoint_data()),
            version,
        );
        assert_eq!(
            row_count(&storage, &agent_id),
            1,
            "checkpoint rejection must not append to a corrupted v{version} stream"
        );
    }
}

#[test]
fn s060_a34_sqlite_rejects_v3_legacy_sub_agent_spawned_payload() {
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal");
    let agent_id = AgentId("sqlite-v3-legacy-spawn".into());
    storage
        .conn
        .lock()
        .expect("SQLite journal lock")
        .execute(
            "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json) VALUES (?1, 3, 1, ?2)",
            rusqlite::params![
                agent_id.0,
                r#"{"type":"SubAgentSpawned","child_id":"child-0123456789abcdef0123456789abcdef","agent_type":"coder","system_prompt":"legacy shaping"}"#,
            ],
        )
        .expect("seed raw v3 legacy SubAgentSpawned payload");

    assert!(
        matches!(storage.read_all(&agent_id), Err(JournalError::Storage(_))),
        "a v3 entry with the legacy SubAgentSpawned shape must be malformed, never inferred"
    );
}

#[test]
fn s060_a34_sqlite_token_query_rejects_malformed_v3_payload_instead_of_partial_total() {
    for (case, corrupt_payload) in [
        ("malformed-json", r#"{"type":"LlmResponse""#),
        (
            "legacy-spawn",
            r#"{"type":"SubAgentSpawned","child_id":"child-0123456789abcdef0123456789abcdef","agent_type":"coder","system_prompt":"legacy shaping"}"#,
        ),
    ] {
        let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal");
        let agent_id = AgentId(format!("sqlite-v3-{case}-token-query"));
        let valid_response = serde_json::to_string(&JournalEntryKind::LlmResponse {
            model: "test-model".into(),
            token_usage: TokenUsage {
                input_tokens: 13,
                output_tokens: 8,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
            finish_reason: "EndTurn".into(),
            assistant_message: None,
        })
        .expect("valid response payload");
        let conn = storage.conn.lock().expect("SQLite journal lock");
        conn.execute(
            "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json) VALUES (?1, 3, 1, ?2)",
            rusqlite::params![agent_id.0, valid_response],
        )
        .expect("seed valid token-bearing row");
        conn.execute(
            "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json) VALUES (?1, 3, 2, ?2)",
            rusqlite::params![agent_id.0, corrupt_payload],
        )
        .expect("seed corrupt v3 row after valid token usage");
        drop(conn);

        assert!(
            matches!(
                storage.query_token_usage(&agent_id),
                Err(JournalError::Storage(_))
            ),
            "{case} token aggregation must propagate decode failure, never return a partial total"
        );
    }
}

#[test]
fn s060_a34_sqlite_wrong_versions_log_structured_expected_got_and_recovery() {
    let _schema_guard = SCHEMA_MISMATCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for version in [2, 4] {
        let (subscriber, events) = capture_sqlite_events();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.read_all(&agent_id), version);

        let events = events.lock().expect("SQLite event capture lock");
        let event = events
            .iter()
            .find(|event| event.level == "ERROR")
            .expect("wrong-version SQLite read should log at ERROR");
        assert_eq!(
            event
                .fields
                .get("expected")
                .expect("structured expected field")
                .trim_matches('"'),
            "3"
        );
        assert_eq!(
            event
                .fields
                .get("got")
                .expect("structured got field")
                .trim_matches('"'),
            version.to_string()
        );
        assert!(
            event
                .fields
                .get("message")
                .expect("operator recovery message")
                .to_lowercase()
                .contains("start a new session")
        );
    }
}
