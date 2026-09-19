//! Shared test doubles for the sandbox crate's in-crate tests: journal
//! fakes and the scripted recording HTTP client.

use simulacra_http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use simulacra_types::{
    AgentId, CheckpointData, JournalEntry, JournalEntryKind, JournalError, JournalStorage,
    TokenUsage,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub(super) struct NullJournal;

pub(super) struct CapturingJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl Default for CapturingJournal {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl CapturingJournal {
    pub(super) fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub(super) fn http_entries(&self) -> Vec<(String, String, u16)> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|entry| match &entry.entry {
                JournalEntryKind::HttpRequest {
                    method,
                    url,
                    status,
                } => Some((method.clone(), url.clone(), *status)),
                _ => None,
            })
            .collect()
    }
}

impl JournalStorage for NullJournal {
    fn append(&self, _entry: JournalEntry) -> Result<(), JournalError> {
        Ok(())
    }
    fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }
    fn save_checkpoint(
        &self,
        _agent_id: &AgentId,
        _after_entry: usize,
        _data: CheckpointData,
    ) -> Result<(), JournalError> {
        Ok(())
    }
    fn fork_from(
        &self,
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
}

impl JournalStorage for CapturingJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self.entries())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        _agent_id: &AgentId,
        _after_entry: usize,
        _data: CheckpointData,
    ) -> Result<(), JournalError> {
        Ok(())
    }

    fn fork_from(
        &self,
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }

    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
}

/// A fake `HttpClient` that records every request and answers from a
/// per-URL script; an unscripted URL is a network error. Requests are
/// recorded BEFORE the response lookup, so even unscripted calls are
/// visible to the assertions.
#[derive(Default)]
pub(super) struct ScriptedHttpClient {
    responses: Mutex<HashMap<String, VecDeque<Result<HttpResponse, HttpError>>>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedHttpClient {
    pub(super) fn with(self, url: &str, response: HttpResponse) -> Self {
        self.responses
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_default()
            .push_back(Ok(response));
        self
    }

    pub(super) fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpClient for ScriptedHttpClient {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request.clone());
        self.responses
            .lock()
            .unwrap()
            .get_mut(&request.url)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                Err(HttpError::Network(format!(
                    "no scripted response for {}",
                    request.url
                )))
            })
    }
}
