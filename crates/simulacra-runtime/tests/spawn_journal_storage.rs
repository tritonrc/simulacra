use simulacra_runtime::{CountingJournalStorage, InMemoryJournalStorage, SqliteJournalStorage};
use simulacra_types::{
    AgentId, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError, JournalStorage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn entry(version: u32, agent: &AgentId) -> JournalEntry {
    JournalEntry {
        schema_version: version,
        agent_id: agent.clone(),
        timestamp_ms: 9,
        entry: JournalEntryKind::TurnStart,
    }
}

fn assert_schema_mismatch(result: Result<(), JournalError>, got: u32) {
    assert!(
        matches!(
            result,
            Err(JournalError::SchemaVersionMismatch { expected: 3, got: actual })
                if actual == got
        ),
        "expected strict v3 rejection for schema {got}, got {result:?}"
    );
}

fn exercise_strict_append(storage: &dyn JournalStorage) {
    let agent = AgentId("parent-strict-version".into());

    assert_schema_mismatch(storage.append(entry(2, &agent)), 2);
    assert_schema_mismatch(storage.append(entry(4, &agent)), 4);

    assert!(
        storage
            .read_all(&agent)
            .expect("rejected entries must not poison subsequent reads")
            .is_empty(),
        "wrong-version entries must not be retained or counted"
    );
    assert_eq!(
        storage
            .query_token_usage(&agent)
            .expect("rejected entries must not affect queries")
            .total(),
        0
    );
    assert!(matches!(
        storage.fork_from(&agent, 0),
        Err(JournalError::InvalidCheckpointIndex(0))
    ));
    assert!(
        storage
            .read_from(&agent, 0)
            .expect("empty journal should replay as empty")
            .is_empty()
    );
}

#[test]
fn s060_a34_in_memory_append_is_strict_and_rejected_entries_never_reach_other_paths() {
    assert_eq!(
        JOURNAL_SCHEMA_VERSION, 3,
        "test requires the S060 v3 boundary"
    );
    let storage = InMemoryJournalStorage::new();
    exercise_strict_append(&storage);
}

#[test]
fn s060_a34_sqlite_append_is_strict_and_rejected_entries_never_reach_other_paths() {
    assert_eq!(
        JOURNAL_SCHEMA_VERSION, 3,
        "test requires the S060 v3 boundary"
    );
    let storage = SqliteJournalStorage::in_memory().expect("in-memory SQLite journal should open");
    exercise_strict_append(&storage);
}

#[test]
fn s060_a34_counting_wrapper_does_not_count_a_rejected_wrong_version_append() {
    let counter = Arc::new(AtomicU64::new(0));
    let inner: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let storage = CountingJournalStorage::new(inner, Arc::clone(&counter));
    let agent = AgentId("parent-counting-version".into());

    assert_schema_mismatch(storage.append(entry(2, &agent)), 2);
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}
