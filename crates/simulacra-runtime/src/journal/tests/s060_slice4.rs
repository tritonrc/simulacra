use super::*;

fn injected_entry(version: u32, agent_id: &AgentId) -> JournalEntry {
    JournalEntry {
        schema_version: version,
        agent_id: agent_id.clone(),
        timestamp_ms: 1,
        entry: JournalEntryKind::Checkpoint {
            snapshot_data: vec![],
        },
    }
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

fn seeded(version: u32) -> (InMemoryJournalStorage, AgentId) {
    let storage = InMemoryJournalStorage::new();
    let agent_id = AgentId(format!("memory-schema-{version}"));
    storage
        .entries
        .write()
        .expect("in-memory journal lock")
        .push(injected_entry(version, &agent_id));
    (storage, agent_id)
}

fn checkpoint_data() -> CheckpointData {
    CheckpointData {
        messages: vec![],
        budget_snapshot: ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        vfs_snapshot: None,
    }
}

#[test]
fn s060_a34_in_memory_read_query_fork_and_replay_reject_v2_and_v4_seeded_entries() {
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
fn s060_a34_in_memory_save_checkpoint_rejects_v2_and_v4_corrupted_streams() {
    for version in [2, 4] {
        let (storage, agent_id) = seeded(version);
        assert_mismatch(
            storage.save_checkpoint(&agent_id, 1, checkpoint_data()),
            version,
        );
        assert_eq!(
            storage
                .entries
                .read()
                .expect("in-memory journal lock")
                .iter()
                .filter(|entry| entry.agent_id == agent_id)
                .count(),
            1,
            "checkpoint rejection must not append to a corrupted v{version} stream"
        );
    }
}

#[test]
fn s060_a34_wrong_version_rejection_logs_expected_got_and_new_session_action() {
    for version in [2, 4] {
        let (subscriber, events) = setup_event_capture();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (storage, agent_id) = seeded(version);
        assert_mismatch(storage.read_all(&agent_id), version);

        let events = events.lock().expect("captured event lock");
        let rejection = events
            .iter()
            .find(|event| event.level == "ERROR")
            .expect("schema rejection should log at ERROR");
        let expected = rejection
            .fields
            .get("expected")
            .expect("schema rejection log should have structured expected field");
        let got = rejection
            .fields
            .get("got")
            .expect("schema rejection log should have structured got field");
        assert_eq!(expected.trim_matches('"'), "3");
        assert_eq!(got.trim_matches('"'), version.to_string());
        let message = rejection
            .fields
            .get("message")
            .expect("schema rejection log should have an operator message");
        assert!(
            message.to_lowercase().contains("start a new session"),
            "log should tell the operator how to recover: {message}"
        );
    }
}
