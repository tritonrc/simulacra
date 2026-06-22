use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use serde_json::json;
use simulacra_python::PyExecTool;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, ResourceBudget, TokenUsage, Tool, ToolError,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
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
            .filter(|entry| &entry.agent_id == agent_id)
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

fn py_capability() -> CapabilityToken {
    CapabilityToken {
        python: true,
        ..Default::default()
    }
}

fn make_tool(budget: Arc<Mutex<ResourceBudget>>, journal: Arc<FakeJournalStorage>) -> PyExecTool {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let journal_dyn: Arc<dyn JournalStorage> = journal;
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = Arc::new(AgentCell::new(
        vfs,
        py_capability(),
        budget,
        journal_dyn,
        http_client,
    ));
    PyExecTool::new(cell)
}

#[tokio::test]
async fn py_exec_reserves_turn_and_journals_code_execution_before_returning() {
    let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 1, Decimal::ZERO, 0)));
    let journal = Arc::new(FakeJournalStorage::default());
    let tool = make_tool(Arc::clone(&budget), Arc::clone(&journal));

    let value = tool
        .call(json!({ "code": "print('hello')" }), &py_capability())
        .await
        .expect("py_exec should succeed");

    assert_eq!(value, json!("hello\n"));
    assert_eq!(budget.lock().unwrap().used_turns, 1);
    assert!(journal.entries().iter().any(|entry| matches!(
        &entry.entry,
        JournalEntryKind::CodeExecution { language } if language == "python"
    )));
}

#[tokio::test]
async fn py_exec_with_exhausted_turn_budget_fails_without_running_or_journaling_execution() {
    let mut exhausted = ResourceBudget::new(0, 1, Decimal::ZERO, 0);
    exhausted.used_turns = 1;
    let budget = Arc::new(Mutex::new(exhausted));
    let journal = Arc::new(FakeJournalStorage::default());
    let tool = make_tool(Arc::clone(&budget), Arc::clone(&journal));

    let error = tool
        .call(json!({ "code": "print('blocked')" }), &py_capability())
        .await
        .expect_err("py_exec should respect exhausted turn budget");

    match error {
        ToolError::ExecutionFailed(message) => {
            assert!(message.contains("budget exhausted"), "{message}");
            assert!(message.contains("turns"), "{message}");
        }
        other => panic!("expected execution failed budget error, got {other:?}"),
    }
    assert_eq!(budget.lock().unwrap().used_turns, 1);
    assert!(!journal.entries().iter().any(|entry| matches!(
        &entry.entry,
        JournalEntryKind::CodeExecution { language } if language == "python"
    )));
}
