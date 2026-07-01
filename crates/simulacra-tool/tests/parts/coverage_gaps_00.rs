use rust_decimal::Decimal;
use serde_json::{Value, json};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{
    SkillMeta, SkillTool, ToolError, ToolRegistry, parse_skill_frontmatter, register_builtins,
};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage, Tool,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::future::Future;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Shared fakes and helpers (mirrors s012_builtins_red.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl JournalStorage for FakeJournalStorage {
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
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

struct Harness {
    registry: ToolRegistry,
    vfs: Arc<MemoryFs>,
    #[allow(dead_code)]
    cell: Arc<AgentCell>,
}

impl Harness {
    fn new(capability: CapabilityToken, budget: ResourceBudget) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = Arc::new(AgentCell::new(
            vfs_dyn,
            capability,
            Arc::new(Mutex::new(budget)),
            journal,
            http_client,
        ));
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, Arc::clone(&cell));

        Self {
            registry,
            vfs,
            cell,
        }
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn full_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn unlimited_budget() -> ResourceBudget {
    ResourceBudget::new(0, 0, Decimal::ZERO, 0)
}

fn call_tool(
    harness: &Harness,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    run_async(harness.registry.call(name, arguments, capability))
}

fn assert_error_result_contains(value: &Value, expected_substring: &str) {
    assert_eq!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "expected an error-shaped tool result, got {value:?}"
    );

    let rendered = value.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains(&expected_substring.to_ascii_lowercase()),
        "expected {value:?} to mention {expected_substring:?}"
    );
}

fn assert_invalid_arguments(result: Result<Value, ToolError>) {
    match result {
        Err(ToolError::InvalidArguments(_)) => {}
        other => panic!("expected invalid arguments error, got {other:?}"),
    }
}

