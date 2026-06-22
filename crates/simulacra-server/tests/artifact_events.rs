//! Verifies that `artifact.created` SSE events are emitted when MailboxFs
//! writes complete (papercut-9).
//!
//! The agent-run UI subscribes to the task event stream and refreshes its
//! artifacts sidebar when it sees `event === "artifact.created"`. Without
//! this event, the sidebar shows "none yet" until manual refresh.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_server::{BudgetPoolConfig, TaskEventChannel, TaskManager, TenantConfig};
use simulacra_types::{ArtifactStore, VirtualFs};
use simulacra_vfs::{ArtifactWriteSink, MailboxFs, MemoryFs};

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "worker".to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

#[derive(Default)]
struct InMemoryArtifactStore {
    inner: std::sync::Mutex<std::collections::BTreeMap<(String, String, String), Vec<u8>>>,
}

impl ArtifactStore for InMemoryArtifactStore {
    fn put(
        &self,
        task_id: &str,
        tenant: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), simulacra_types::ArtifactError> {
        self.inner.lock().unwrap().insert(
            (tenant.to_string(), task_id.to_string(), path.to_string()),
            data.to_vec(),
        );
        Ok(())
    }

    fn get(
        &self,
        tenant: &str,
        task_id: &str,
        path: &str,
    ) -> Result<Vec<u8>, simulacra_types::ArtifactError> {
        self.inner
            .lock()
            .unwrap()
            .get(&(tenant.to_string(), task_id.to_string(), path.to_string()))
            .cloned()
            .ok_or_else(|| simulacra_types::ArtifactError::NotFound(path.to_string()))
    }

    fn list(
        &self,
        _tenant: &str,
        _task_id: &str,
    ) -> Result<Vec<simulacra_types::ArtifactEntry>, simulacra_types::ArtifactError> {
        Ok(vec![])
    }

    fn delete_task(
        &self,
        _tenant: &str,
        _task_id: &str,
    ) -> Result<(), simulacra_types::ArtifactError> {
        Ok(())
    }
}

/// End-to-end-style: a write through MailboxFs (wired with an
/// emit_event-based sink) results in an `artifact.created` event observable
/// by an SSE subscriber that connects with history replay.
#[test]
fn write_to_proc_mailbox_emits_artifact_created_event_via_task_manager() {
    let manager = Arc::new(TaskManager::new());
    let tenant_cfg = tenant("tenant-a");
    let handle = manager
        .create_task(&tenant_cfg, "build a report", None, json!({}), None)
        .unwrap();
    let task_id = handle.task_id.clone();

    // Capture the history *before* the artifact write so we can diff later.
    let event_tx = manager.get_event_sender(&task_id).unwrap();
    let (history_before, _rx) = event_tx.subscribe_with_history();

    // Build MailboxFs wired to emit `artifact.created` via TaskManager::emit_event.
    let store: Arc<dyn ArtifactStore> = Arc::new(InMemoryArtifactStore::default());
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let manager_for_sink = Arc::clone(&manager);
    let task_id_for_sink = task_id.clone();
    let sink: ArtifactWriteSink = Arc::new(move |path: &str, _tenant: &str, size: u64| {
        let _ = manager_for_sink.emit_event(
            &task_id_for_sink,
            json!({
                "event": "artifact.created",
                "task_id": task_id_for_sink,
                "path": path,
                "size": size,
            }),
        );
    });

    let mailbox = MailboxFs::new(
        inner,
        task_id.clone(),
        "tenant-a".to_string(),
        Arc::clone(&store),
    )
    .with_artifact_sink(sink);

    let vfs: &dyn VirtualFs = &mailbox;
    vfs.write("/proc/mailbox/x.md", b"hello").unwrap();

    // Now snapshot history again — the new event should be there.
    let (history_after, _rx2) = event_tx.subscribe_with_history();
    let new_events: Vec<&Value> = history_after.iter().skip(history_before.len()).collect();

    let artifact_events: Vec<&Value> = new_events
        .iter()
        .copied()
        .filter(|e| e.get("event").and_then(|v| v.as_str()) == Some("artifact.created"))
        .collect();

    assert_eq!(
        artifact_events.len(),
        1,
        "expected exactly one artifact.created event, got history: {history_after:#?}"
    );
    let evt = artifact_events[0];
    assert_eq!(evt["task_id"].as_str(), Some(task_id.as_str()));
    assert_eq!(evt["path"].as_str(), Some("/proc/mailbox/x.md"));
    assert_eq!(evt["size"].as_u64(), Some(5));
    assert!(
        evt["seq"].as_u64().is_some(),
        "artifact.created event must carry a seq for SSE replay ordering"
    );
}

/// Sanity check on the test infrastructure: a MailboxFs without a sink does
/// NOT emit anything. (Pins the regression: before papercut-9, the engine
/// was wiring a no-op MailboxFs and the UI saw nothing.)
#[test]
fn write_without_artifact_sink_emits_no_artifact_created_event() {
    let store: Arc<dyn ArtifactStore> = Arc::new(InMemoryArtifactStore::default());
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let event_tx = TaskEventChannel::new(8);
    let (history_before, mut rx) = event_tx.subscribe_with_history();
    assert!(history_before.is_empty());

    let mailbox = MailboxFs::new(
        inner,
        "task-xyz".to_string(),
        "tenant-a".to_string(),
        Arc::clone(&store),
    );
    let vfs: &dyn VirtualFs = &mailbox;
    vfs.write("/proc/mailbox/x.md", b"hello").unwrap();

    // No sink wired -> no events arrive.
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}
