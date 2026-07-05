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
    let subscriber = tracing_subscriber::registry::Registry::default().with(EventCaptureLayer {
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
                    provider_content: vec![],
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
