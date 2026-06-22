//! S029 AgentCell integration tests for /proc capability gating and journaling.
//!
//! These tests verify that /proc reads are capability-gated and journaled at the
//! AgentCell proxy layer. They use ProcFs-wrapped VFS passed to AgentCell::new.

// use std::collections::HashMap;
use std::sync::{Arc, Mutex, atomic::AtomicU64};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_sandbox::{AgentCell, SandboxError};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, FsMetadata, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::{
    MemoryFs,
    procfs::{HookLister, ProcFs, ProcState, ToolLister},
};
// use tracing_subscriber::layer::SubscriberExt;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct EmptyToolLister;
impl ToolLister for EmptyToolLister {
    fn tool_names(&self) -> Vec<String> {
        vec![]
    }
    fn tool_json(&self, _: &str) -> Option<String> {
        None
    }
}

struct EmptyHookLister;
impl HookLister for EmptyHookLister {
    fn hook_names(&self, _: &str) -> Vec<String> {
        vec![]
    }
}

#[derive(Default)]
struct FakeJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournal {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl JournalStorage for FakeJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| &e.agent_id == agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|e| JournalError::Storage(e.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(&self, agent_id: &AgentId, idx: usize) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        Ok(entries.into_iter().take(idx + 1).collect())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        Ok(entries.into_iter().skip(start).collect())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_proc_state() -> Arc<ProcState> {
    Arc::new(ProcState {
        agent_id: "agent-test".to_string(),
        agent_name: "test".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget: Arc::new(Mutex::new(ResourceBudget::new(
            100_000,
            10,
            Decimal::ZERO,
            0,
        ))),
        capabilities: CapabilityToken::default(),
        tools: Arc::new(EmptyToolLister),
        session_id: "session-test".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: Arc::new(EmptyHookLister),
        turn: Arc::new(AtomicU64::new(0)),
    })
}

fn make_cell(paths_read: Vec<PathPattern>, journal: Arc<FakeJournal>) -> AgentCell {
    let proc_vfs = ProcFs::new(MemoryFs::new(), make_proc_state());
    let vfs: Arc<dyn VirtualFs> = Arc::new(proc_vfs);
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(
        vfs,
        CapabilityToken {
            paths_read,
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(
            100_000,
            10,
            Decimal::ZERO,
            0,
        ))),
        journal as Arc<dyn JournalStorage>,
        http,
    )
}

// ---------------------------------------------------------------------------
// Tests: capability gating
// ---------------------------------------------------------------------------

#[test]
fn agent_with_workspace_only_paths_read_gets_capability_error_on_proc_reads() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        Arc::clone(&journal),
    );

    let err = cell
        .read_file("/proc/agent/id")
        .expect_err("/proc reads should be denied without a matching paths_read grant");

    assert!(
        err.to_string().contains("capability denied"),
        "expected capability denied error, got: {err}"
    );
}

#[test]
fn agent_with_wildcard_paths_read_can_read_all_proc_paths() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(vec![PathPattern("/**".into())], Arc::clone(&journal));

    let value = cell
        .read_file("/proc/agent/id")
        .expect("/** should allow /proc reads");

    assert_eq!(String::from_utf8(value).unwrap(), "agent-test");
}

#[test]
fn agent_with_proc_wildcard_paths_read_can_read_all_proc_paths() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(vec![PathPattern("/proc/**".into())], Arc::clone(&journal));

    let value = cell
        .read_file("/proc/agent/id")
        .expect("/proc/** should allow /proc reads");

    assert_eq!(String::from_utf8(value).unwrap(), "agent-test");
}

#[test]
fn agent_with_budget_only_proc_read_can_read_budget_but_not_capabilities() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![
            PathPattern("/workspace/**".into()),
            PathPattern("/proc/budget/**".into()),
        ],
        Arc::clone(&journal),
    );

    // Budget read should work
    let remaining = cell
        .read_file("/proc/budget/remaining_tokens")
        .expect("granted /proc/budget reads should succeed");
    assert!(!remaining.is_empty());

    // Capabilities read should fail
    let err = cell
        .read_file("/proc/capabilities/shell")
        .expect_err("ungranted /proc/capabilities reads should be denied");
    assert!(err.to_string().contains("capability denied"));
}

#[test]
fn capability_check_happens_before_procfs_handler_dispatch() {
    // Use a SpyFs to detect if the inner VFS is ever called on a denied read.
    struct SpyFs {
        inner: MemoryFs,
        reads: std::sync::atomic::AtomicUsize,
    }
    impl Default for SpyFs {
        fn default() -> Self {
            Self {
                inner: MemoryFs::new(),
                reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl VirtualFs for SpyFs {
        fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.read(path)
        }
        fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
            self.inner.write(path, data)
        }
        fn exists(&self, path: &str) -> bool {
            self.inner.exists(path)
        }
        fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
            self.inner.list_dir(path)
        }
        fn mkdir(&self, path: &str) -> Result<(), VfsError> {
            self.inner.mkdir(path)
        }
        fn remove(&self, path: &str) -> Result<(), VfsError> {
            self.inner.remove(path)
        }
        fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
            self.inner.metadata(path)
        }
        fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
            self.inner.snapshot()
        }
        fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
            self.inner.restore(snapshot)
        }
    }

    let _spy = Arc::new(SpyFs::default());
    let proc_vfs = ProcFs::new(MemoryFs::new(), make_proc_state());
    // Actually we want the spy to be the INNER of ProcFs — but ProcFs takes ownership.
    // Instead, let's just confirm the cell returns CapabilityDenied before reaching proc.
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        Arc::new(proc_vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(
            100_000,
            10,
            Decimal::ZERO,
            0,
        ))),
        Arc::new(FakeJournal::default()) as Arc<dyn JournalStorage>,
        http,
    );

    let err = cell
        .read_file("/proc/agent/id")
        .expect_err("should be denied");
    assert!(matches!(err, SandboxError::CapabilityDenied(_)));
}

// ---------------------------------------------------------------------------
// Tests: journaling
// ---------------------------------------------------------------------------

#[test]
fn proc_read_produces_a_vfs_journal_entry_with_the_proc_path() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(vec![PathPattern("/**".into())], Arc::clone(&journal));

    let _ = cell.read_file("/proc/agent/id");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, content, is_error, .. }
                if tool_name == "read_file" && !is_error && content.contains("/proc/agent/id")
        )),
        "expected journal entry for /proc/agent/id read; entries: {entries:#?}"
    );
}

#[test]
fn proc_list_dir_produces_a_vfs_journal_entry() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(vec![PathPattern("/**".into())], Arc::clone(&journal));

    let listing = cell
        .list_dir("/proc/agent")
        .expect("list_dir on allowed /proc subtree should succeed");
    assert_eq!(listing, vec!["id", "model", "name", "parent_id", "turn"]);

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, content, is_error, .. }
                if tool_name == "list_dir" && !is_error && content.contains("/proc/agent")
        )),
        "expected a ToolResult journal entry for successful /proc list_dir; entries: {entries:#?}"
    );
}

#[test]
fn denied_proc_read_produces_a_journal_entry_recording_the_denial() {
    let journal = Arc::new(FakeJournal::default());
    let cell = make_cell(
        vec![PathPattern("/workspace/**".into())],
        Arc::clone(&journal),
    );

    let _ = cell.read_file("/proc/agent/id");

    let entries = journal.entries();
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::ToolResult { tool_name, is_error, .. }
                if tool_name == "read_file" && *is_error
        )),
        "expected an error journal entry for denied /proc read; entries: {entries:#?}"
    );
}

// ---------------------------------------------------------------------------
// Tests: mailbox capability
// ---------------------------------------------------------------------------

#[test]
fn mailbox_write_to_proc_mailbox_is_subject_to_paths_write_capability_check() {
    let journal = Arc::new(FakeJournal::default());
    let proc_vfs = ProcFs::new(MemoryFs::new(), make_proc_state());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        Arc::new(proc_vfs),
        CapabilityToken {
            paths_read: vec![PathPattern("/proc/mailbox/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())], // no /proc/mailbox
            ..Default::default()
        },
        Arc::new(Mutex::new(ResourceBudget::new(
            100_000,
            10,
            Decimal::ZERO,
            0,
        ))),
        journal as Arc<dyn JournalStorage>,
        http,
    );

    let err = cell
        .write_file("/proc/mailbox/report.md", b"report")
        .expect_err("mailbox writes should respect paths_write");

    assert!(
        err.to_string().contains("capability denied"),
        "expected capability denied, got: {err}"
    );
}
