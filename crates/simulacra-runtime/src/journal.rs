//! In-memory journal storage implementation.

use simulacra_types::{
    AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// In-memory journal storage backed by a `RwLock<Vec<JournalEntry>>`.
#[derive(Debug, Default)]
pub struct InMemoryJournalStorage {
    entries: RwLock<Vec<JournalEntry>>,
}

impl InMemoryJournalStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Validate schema version of a journal entry.
/// Returns an error if the entry's schema version exceeds the current version.
fn validate_schema_version(entry: &JournalEntry) -> Result<(), JournalError> {
    if entry.schema_version > JOURNAL_SCHEMA_VERSION {
        tracing::error!(
            "schema version mismatch: expected {} but found {}",
            JOURNAL_SCHEMA_VERSION,
            entry.schema_version
        );
        return Err(JournalError::SchemaVersionMismatch {
            expected: JOURNAL_SCHEMA_VERSION,
            got: entry.schema_version,
        });
    }
    Ok(())
}

impl JournalStorage for InMemoryJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        entries.push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let mut result = Vec::new();
        for entry in entries.iter().filter(|e| e.agent_id == *agent_id) {
            validate_schema_version(entry)?;
            result.push(entry.clone());
        }
        Ok(result)
    }

    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let mut total = TokenUsage::default();
        for entry in entries.iter().filter(|e| e.agent_id == *agent_id) {
            if let JournalEntryKind::LlmResponse { token_usage, .. } = &entry.entry {
                total.input_tokens += token_usage.input_tokens;
                total.output_tokens += token_usage.output_tokens;
            }
        }
        Ok(total)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let serialized =
            serde_json::to_vec(&data).map_err(|e| JournalError::Storage(e.to_string()))?;
        let entry = JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0, // Checkpoints don't need wall-clock time
            entry: JournalEntryKind::Checkpoint {
                snapshot_data: serialized,
            },
        };
        // Validate that after_entry is within bounds
        let mut entries = self
            .entries
            .write()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_count = entries.iter().filter(|e| e.agent_id == *agent_id).count();
        if after_entry > agent_count {
            return Err(JournalError::InvalidCheckpointIndex(after_entry));
        }
        entries.push(entry);
        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_entries: Vec<JournalEntry> = entries
            .iter()
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect();

        if checkpoint_idx >= agent_entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }

        // The entry at checkpoint_idx must be a Checkpoint
        if !matches!(
            agent_entries[checkpoint_idx].entry,
            JournalEntryKind::Checkpoint { .. }
        ) {
            return Err(JournalError::NotFound(format!(
                "entry at index {checkpoint_idx} is not a checkpoint"
            )));
        }

        // Return entries up to and including the checkpoint
        Ok(agent_entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_entries: Vec<JournalEntry> = entries
            .iter()
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect();

        if start_index > agent_entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }

        // Validate schema version on each entry (same as read_all)
        for entry in &agent_entries[start_index..] {
            if entry.schema_version != JOURNAL_SCHEMA_VERSION {
                return Err(JournalError::SchemaVersionMismatch {
                    expected: JOURNAL_SCHEMA_VERSION,
                    got: entry.schema_version,
                });
            }
        }

        Ok(agent_entries[start_index..].to_vec())
    }
}

/// Journal wrapper that mirrors successful appends into an atomic counter.
///
/// This is used by `/proc/session/journal_entries` so agent-visible
/// orientation reflects all journal writes, including writes performed by
/// sandbox/tool layers outside the `AgentLoop`.
pub struct CountingJournalStorage {
    inner: Arc<dyn JournalStorage>,
    counter: Arc<AtomicU64>,
}

impl CountingJournalStorage {
    pub fn new(inner: Arc<dyn JournalStorage>, counter: Arc<AtomicU64>) -> Self {
        Self { inner, counter }
    }
}

impl JournalStorage for CountingJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.inner.append(entry)?;
        self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.read_all(agent_id)
    }

    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        self.inner.query_token_usage(agent_id)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        self.inner.save_checkpoint(agent_id, after_entry, data)?;
        self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.fork_from(agent_id, checkpoint_idx)
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.read_from(agent_id, start_index)
    }
}

#[cfg(all(test, feature = "spawn"))]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use simulacra_types::{Message, ResourceBudget, Role};
    use simulacra_types::{VfsSnapshot, VirtualFs};
    use simulacra_vfs::MemoryFs;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        level: String,
        fields: HashMap<String, String>,
    }

    struct EventCaptureLayer {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for EventCaptureLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut fields);
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                level: event.metadata().level().to_string(),
                fields,
            });
        }
    }

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    fn setup_event_capture() -> (
        impl tracing::Subscriber + Send + Sync,
        Arc<Mutex<Vec<CapturedEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(EventCaptureLayer {
                events: Arc::clone(&events),
            });
        (subscriber, events)
    }

    #[test]
    fn counting_journal_storage_counts_successful_appends_and_checkpoints() {
        let agent_id = AgentId("counting-agent".into());
        let counter = Arc::new(AtomicU64::new(0));
        let inner: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
        let storage = CountingJournalStorage::new(inner, Arc::clone(&counter));

        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        storage
            .save_checkpoint(
                &agent_id,
                1,
                CheckpointData {
                    messages: vec![],
                    budget_snapshot: ResourceBudget::new(32, 2, Decimal::new(10, 0), 0),
                    vfs_snapshot: None,
                },
            )
            .unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        let err = storage
            .save_checkpoint(
                &agent_id,
                99,
                CheckpointData {
                    messages: vec![],
                    budget_snapshot: ResourceBudget::new(32, 2, Decimal::new(10, 0), 0),
                    vfs_snapshot: None,
                },
            )
            .expect_err("failed checkpoint should not increment counter");
        assert!(matches!(err, JournalError::InvalidCheckpointIndex(99)));
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn fork_from_checkpoint_creates_storage_independent_of_original_mutations() {
        // Edge case: a forked journal must be able to diverge without inheriting later mutations
        // from the original storage, while still sharing history up to the checkpoint.
        let agent_id = AgentId("fork-agent".into());
        let original = InMemoryJournalStorage::new();

        original
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();
        original
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 2,
                entry: JournalEntryKind::FileWrite {
                    path: "shared-before-checkpoint.txt".into(),
                    size_bytes: 12,
                },
            })
            .unwrap();
        original
            .save_checkpoint(
                &agent_id,
                2,
                CheckpointData {
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "checkpoint".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                    }],
                    budget_snapshot: ResourceBudget::new(256, 8, Decimal::new(100, 0), 0),
                    vfs_snapshot: Some(vec![1, 2, 3]),
                },
            )
            .unwrap();
        original
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 3,
                entry: JournalEntryKind::HttpRequest {
                    method: "GET".into(),
                    url: "https://original-only.example".into(),
                    status: 200,
                },
            })
            .unwrap();

        let forked_history = original.fork_from(&agent_id, 2).unwrap();
        let forked = InMemoryJournalStorage::new();
        for entry in forked_history {
            forked.append(entry).unwrap();
        }
        forked
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 4,
                entry: JournalEntryKind::FileWrite {
                    path: "fork-only.txt".into(),
                    size_bytes: 7,
                },
            })
            .unwrap();

        let original_entries = original.read_all(&agent_id).unwrap();
        let forked_entries = forked.read_all(&agent_id).unwrap();

        assert_eq!(original_entries.len(), 4);
        assert_eq!(forked_entries.len(), 4);
        assert!(matches!(
            original_entries[2].entry,
            JournalEntryKind::Checkpoint { .. }
        ));
        assert!(matches!(
            forked_entries[2].entry,
            JournalEntryKind::Checkpoint { .. }
        ));
        assert!(matches!(
            original_entries[3].entry,
            JournalEntryKind::HttpRequest { .. }
        ));
        assert!(forked_entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileWrite { path, .. } if path == "fork-only.txt"
            )
        }));
        assert!(original_entries.iter().all(|entry| {
            !matches!(
                &entry.entry,
                JournalEntryKind::FileWrite { path, .. } if path == "fork-only.txt"
            )
        }));
        assert!(
            forked_entries
                .iter()
                .all(|entry| { !matches!(&entry.entry, JournalEntryKind::HttpRequest { .. }) })
        );
    }

    #[test]
    fn read_from_rejects_schema_mismatch_on_individual_entry() {
        // Edge case: replay reads should fail loudly if any individual entry has a newer schema,
        // even when surrounding entries are otherwise valid.
        let agent_id = AgentId("schema-agent".into());
        let storage = InMemoryJournalStorage::new();

        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 10,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();
        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION + 1,
                agent_id: agent_id.clone(),
                timestamp_ms: 11,
                entry: JournalEntryKind::LlmRequest {
                    model: "future-model".into(),
                    message_count: 2,
                },
            })
            .unwrap();
        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id,
                timestamp_ms: 12,
                entry: JournalEntryKind::LlmResponse {
                    model: "current-model".into(),
                    token_usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    finish_reason: "EndTurn".into(),
                    assistant_message: None,
                },
            })
            .unwrap();

        let err = storage
            .read_from(&AgentId("schema-agent".into()), 0)
            .expect_err("mixed-schema replay should not silently continue");

        assert!(matches!(
            err,
            JournalError::SchemaVersionMismatch {
                expected: JOURNAL_SCHEMA_VERSION,
                got
            } if got == JOURNAL_SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn schema_version_mismatch_is_logged_at_error_with_expected_and_found_versions() {
        let (subscriber, captured_events) = setup_event_capture();
        let storage = InMemoryJournalStorage::new();
        let agent_id = AgentId("schema-log-agent".into());

        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION + 1,
                agent_id: agent_id.clone(),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();

        let _guard = tracing::subscriber::set_default(subscriber);
        let err = storage
            .read_all(&agent_id)
            .expect_err("schema mismatch should be surfaced");
        assert!(matches!(
            err,
            JournalError::SchemaVersionMismatch {
                expected: JOURNAL_SCHEMA_VERSION,
                got
            } if got == JOURNAL_SCHEMA_VERSION + 1
        ));

        let events = captured_events.lock().unwrap();
        assert!(
            events.iter().any(|event| {
                event.level == "ERROR"
                    && event
                        .fields
                        .values()
                        .any(|value| value.contains("schema version mismatch"))
                    && event.fields.values().any(|value| {
                        value.contains(&JOURNAL_SCHEMA_VERSION.to_string())
                            && value.contains(&(JOURNAL_SCHEMA_VERSION + 1).to_string())
                    })
            }),
            "expected an ERROR log with the expected and found schema versions"
        );
    }

    #[test]
    fn fork_from_checkpoint_restores_vfs_snapshot_state() {
        let agent_id = AgentId("fork-vfs-agent".into());
        let storage = InMemoryJournalStorage::new();
        let original_fs = MemoryFs::new();
        let original_view: &dyn VirtualFs = &original_fs;

        original_view
            .write("/workspace/note.txt", b"checkpoint contents")
            .unwrap();
        let checkpoint_snapshot = original_view.snapshot().unwrap();

        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();
        storage
            .save_checkpoint(
                &agent_id,
                1,
                CheckpointData {
                    messages: vec![],
                    budget_snapshot: ResourceBudget::new(32, 2, Decimal::new(10, 0), 0),
                    vfs_snapshot: Some(checkpoint_snapshot.data.clone()),
                },
            )
            .unwrap();

        original_view
            .write("/workspace/note.txt", b"mutated")
            .unwrap();

        let forked_history = storage.fork_from(&agent_id, 1).unwrap();
        let forked_fs = MemoryFs::new();
        let forked_view: &dyn VirtualFs = &forked_fs;

        assert_eq!(forked_history.len(), 2);

        // Restore VFS snapshot from the checkpoint entry in the forked history
        if let JournalEntryKind::Checkpoint { snapshot_data } = &forked_history[1].entry {
            let checkpoint: CheckpointData =
                serde_json::from_slice(snapshot_data).expect("checkpoint should deserialize");
            let vfs_snapshot = VfsSnapshot {
                data: checkpoint
                    .vfs_snapshot
                    .clone()
                    .expect("checkpoint should contain a VFS snapshot"),
            };
            forked_view
                .restore(&vfs_snapshot)
                .expect("restoring the forked snapshot should succeed");
        } else {
            panic!("expected checkpoint entry in forked history");
        }

        assert_eq!(
            forked_view.read("/workspace/note.txt").unwrap(),
            b"checkpoint contents",
            "forking from a checkpoint should restore the checkpoint's VFS snapshot into the fork"
        );
    }
}
