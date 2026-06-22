//! [`ShellHttpProxy`] implementation that delegates through the AgentCell's
//! Golden Rule chain (capability check, budget check, journal, HTTP client).

use crate::SandboxError;
use simulacra_http::HttpClient;
use simulacra_shell::{ShellHttpError, ShellHttpProxy, ShellHttpResponse};
use simulacra_types::{AgentId, CapabilityToken, JournalStorage, ResourceBudget};
use std::sync::{Arc, Mutex};

/// A [`ShellHttpProxy`] that routes `curl`/`wget` calls through the AgentCell's
/// capability/budget/journal enforcement chain via `fetch_http_inner`.
pub struct AgentCellShellHttpProxy {
    pub capability: CapabilityToken,
    pub budget: Arc<Mutex<ResourceBudget>>,
    pub journal: Arc<dyn JournalStorage>,
    pub agent_id: AgentId,
    pub http_client: Arc<dyn HttpClient>,
}

impl ShellHttpProxy for AgentCellShellHttpProxy {
    fn execute(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<ShellHttpResponse, ShellHttpError> {
        // Convert headers from (String, String) to (&str, &str) for fetch_http_inner
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Call fetch_http_inner with increment_turns: true
        // (unlike AgentCellFetchProxy which passes false because execute_js already claimed the turn)
        let result = crate::http::fetch_http_inner(
            url,
            method,
            &header_refs,
            body,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
            true, // INCREMENT TURNS — shell HTTP is its own turn
            "shell_http",
            &*self.http_client,
            timeout_ms,
        );

        match result {
            Ok(resp) => Ok(ShellHttpResponse {
                status: resp.status,
                status_text: resp.status_text,
                headers: resp.headers,
                body: resp.body,
                url: resp.url,
            }),
            Err(e) => Err(sandbox_error_to_shell_http_error(e)),
        }
    }
}

/// Map [`SandboxError`] to [`ShellHttpError`].
fn sandbox_error_to_shell_http_error(err: SandboxError) -> ShellHttpError {
    match err {
        SandboxError::CapabilityDenied(denied) => {
            ShellHttpError::CapabilityDenied(denied.to_string())
        }
        SandboxError::BudgetExhausted(exhausted) => {
            ShellHttpError::BudgetExhausted(exhausted.to_string())
        }
        SandboxError::Http(msg) => ShellHttpError::NetworkError(msg),
        other => ShellHttpError::NetworkError(other.to_string()),
    }
}
