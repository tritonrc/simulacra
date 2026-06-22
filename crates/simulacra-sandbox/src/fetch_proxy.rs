//! [`FetchProxy`] implementation that delegates through the AgentCell's
//! Golden Rule chain (capability check, budget check, journal, HTTP client).
//!
//! When an [`IntegrationRegistry`](simulacra_integration::IntegrationRegistry) is
//! present, outbound `fetch()` calls that match a configured integration's
//! `base_url` get auth headers injected automatically. Credentials never
//! reach the agent or LLM.

use crate::SandboxError;
use simulacra_fetch::{FetchError, FetchProxy, FetchResponse};
use simulacra_http::HttpClient;
use simulacra_types::{AgentId, CapabilityToken, JournalStorage, ResourceBudget};
use std::sync::{Arc, Mutex};

/// A [`FetchProxy`] that routes `fetch()` calls through the AgentCell's
/// capability/budget/journal enforcement chain via `fetch_http_inner`.
///
/// If `integration_registry` is set, credentials are injected for URLs
/// matching configured integration endpoints.
pub struct AgentCellFetchProxy {
    pub capability: CapabilityToken,
    pub budget: Arc<Mutex<ResourceBudget>>,
    pub journal: Arc<dyn JournalStorage>,
    pub agent_id: AgentId,
    pub http_client: Arc<dyn HttpClient>,
    /// Optional integration registry for credential injection.
    pub integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    /// Integrations this agent's tenant is granted access to.
    pub tenant_integrations: Vec<String>,
}

impl FetchProxy for AgentCellFetchProxy {
    fn fetch(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<FetchResponse, FetchError> {
        // S033: Inject credentials if URL matches a configured integration.
        let mut all_headers = headers.to_vec();
        if let Some(ref registry) = self.integration_registry {
            match registry.inject_headers_sync(url, &self.tenant_integrations) {
                Ok(Some(injected)) => {
                    tracing::info!(
                        url = url,
                        agent_id = ?self.agent_id,
                        "credential injection for outbound fetch"
                    );
                    for (k, v) in injected {
                        if !all_headers
                            .iter()
                            .any(|(hk, _)| hk.eq_ignore_ascii_case(&k))
                        {
                            all_headers.push((k, v));
                        }
                    }
                }
                Ok(None) => {} // No matching integration
                Err(e) => {
                    return Err(FetchError::NetworkError(format!(
                        "integration credential error: {e}"
                    )));
                }
            }
        }

        // Convert headers from (String, String) to (&str, &str) for fetch_http_inner
        let header_refs: Vec<(&str, &str)> = all_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let result = crate::http::fetch_http_inner(
            url,
            method,
            &header_refs,
            body,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
            false, // don't increment turns — enclosing execute_js already claimed it
            "fetch_http",
            &*self.http_client,
            timeout_ms,
        );

        match result {
            Ok(resp) => Ok(FetchResponse {
                status: resp.status,
                status_text: resp.status_text,
                headers: resp.headers,
                body: resp.body,
                url: resp.url,
                redirected: resp.redirected,
            }),
            Err(e) => Err(sandbox_error_to_fetch_error(e)),
        }
    }
}

/// Map [`SandboxError`] to [`FetchError`].
fn sandbox_error_to_fetch_error(err: SandboxError) -> FetchError {
    match err {
        SandboxError::CapabilityDenied(denied) => FetchError::CapabilityDenied(denied.to_string()),
        SandboxError::BudgetExhausted(exhausted) => {
            FetchError::BudgetExhausted(exhausted.to_string())
        }
        SandboxError::Http(msg) => FetchError::NetworkError(msg),
        other => FetchError::NetworkError(other.to_string()),
    }
}
