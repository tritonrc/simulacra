//! ModuleFetcher implementation that delegates to [`fetch_http_inner`](crate::http::fetch_http_inner).
//!
//! Per spec S011 §23, `ModuleFetcher::fetch` delegates to `AgentCell::fetch_http`
//! logic, ensuring remote module fetches go through the full Golden Rule chain:
//! span → capability → budget → execute → journal → return.

use simulacra_http::HttpClient;
use simulacra_quickjs::ModuleFetcher;
use simulacra_types::{
    AgentId, CapabilityToken, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
    JournalStorage, ResourceBudget,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::guards::{check_and_journal_capability, check_turns_budget};
use crate::http;

pub(crate) struct AgentCellModuleFetcher {
    pub(crate) capability: CapabilityToken,
    pub(crate) budget: Arc<Mutex<ResourceBudget>>,
    pub(crate) journal: Arc<dyn JournalStorage>,
    pub(crate) agent_id: AgentId,
    pub(crate) http_client: Arc<dyn HttpClient>,
    /// Pre-registered module source stubs (shared with AgentCell).
    pub(crate) stubs: HashMap<String, String>,
}

impl ModuleFetcher for AgentCellModuleFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        // Module fetch follows its own Golden Rule chain with operation name "module_fetch"
        // so that OTel events carry the correct operation context.
        let _span = tracing::info_span!(
            "module_fetch",
            simulacra.operation.name = "module_fetch",
            simulacra.module.url = %url,
        )
        .entered();

        // Check for a pre-registered stub before attempting HTTP.
        // Stubs still need capability + budget checks via the shared guard helpers,
        // but short-circuit the actual HTTP request.
        if let Some(source) = self.stubs.get(url) {
            let host = http::extract_host(url).unwrap_or_default();
            check_and_journal_capability(
                || self.capability.check_network(&host),
                "module_fetch",
                "network",
                &self.journal,
                &self.agent_id,
            )
            .map_err(|e| e.to_string())?;

            check_turns_budget(&self.budget, &self.journal, &self.agent_id)
                .map_err(|e| e.to_string())?;

            // Journal the stub as a synthetic HTTP request
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::HttpRequest {
                    method: "GET".to_string(),
                    url: url.to_string(),
                    status: 200,
                },
            }) {
                tracing::error!(error = %err, "journal append failed for module_fetch");
            }
            return Ok(source.clone());
        }

        // Delegate to the shared HTTP Golden Rule chain.
        // `increment_turns: false` because module fetches share the enclosing
        // execute_js turn — we only check the budget, we don't consume another turn.
        let response = http::fetch_http_inner(
            url,
            "GET",
            &[],
            None,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
            false,
            "module_fetch",
            &*self.http_client,
            None,
        )
        .map_err(|e| e.to_string())?;

        String::from_utf8(response.body)
            .map_err(|e| format!("module response is not valid UTF-8: {e}"))
    }
}
