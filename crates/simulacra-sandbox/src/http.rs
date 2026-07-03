//! HTTP request handling with Golden Rule enforcement.
//!
//! Contains the core HTTP fetch logic shared by [`AgentCell::fetch_http`](super::AgentCell::fetch_http)
//! and [`AgentCellModuleFetcher`](super::module_fetcher::AgentCellModuleFetcher).

use crate::SandboxError;
use crate::guards::{check_and_journal_capability, check_turns_budget, reserve_turn};
use simulacra_http::{HttpClient, HttpRequest, HttpResponse};
use simulacra_types::{
    AgentId, CapabilityToken, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
    JournalStorage, ResourceBudget,
};
use std::sync::{Arc, Mutex};

/// Extract host (without port) from a URL string.
pub(crate) fn extract_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(ToOwned::to_owned)
}

/// Core fetch_http logic following the Golden Rule:
/// span → capability → budget → execute → journal → return.
///
/// Shared by [`AgentCell::fetch_http`](super::AgentCell::fetch_http) and
/// [`AgentCellModuleFetcher::fetch`](super::module_fetcher::AgentCellModuleFetcher).
///
/// When `increment_turns` is `false`, the budget check is still performed but
/// `used_turns` is not incremented. This is used by the module fetcher, where
/// the enclosing `execute_js` call has already claimed the turn.
///
/// `operation_name` is used in journal entries and OTel events so callers can
/// distinguish between a top-level `fetch_http` and a `module_fetch`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_http_inner(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    capability: &CapabilityToken,
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
    increment_turns: bool,
    operation_name: &str,
    http_client: &dyn HttpClient,
    timeout_ms: Option<u64>,
) -> Result<HttpResponse, SandboxError> {
    // Span first — all subsequent events (denials, budget, journal) nest under it
    let span = tracing::info_span!(
        "sandbox_http_fetch",
        simulacra.operation.name = "sandbox_http_fetch",
        simulacra.http.url = %url,
        simulacra.http.method = %method,
        simulacra.http.status = tracing::field::Empty,
    );
    let _guard = span.enter();

    // Extract host from URL for capability check
    let host = extract_host(url).unwrap_or_default();

    // Check network capability
    check_and_journal_capability(
        || capability.check_network(&host),
        operation_name,
        "network",
        journal,
        agent_id,
    )?;

    if increment_turns {
        // Atomically reserve the turn before execution.
        reserve_turn(budget, journal, agent_id)?;
    } else {
        // Caller already claimed the turn, but global budget limits still apply.
        check_turns_budget(budget, journal, agent_id)?;
    }

    // Build the HttpRequest
    let request = HttpRequest {
        url: url.to_string(),
        method: method.to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.map(|b| b.to_vec()),
        timeout_ms,
        max_redirects: None,
    };

    // Execute HTTP request via the injected client
    let response = http_client.execute(&request);

    // Record status on the span now that we know it
    let status = match &response {
        Ok(resp) => resp.status,
        Err(_) => 0,
    };
    span.record("simulacra.http.status", status);

    // Journal the HTTP request BEFORE returning (even on failure)
    if let Err(err) = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::HttpRequest {
            method: method.to_string(),
            url: url.to_string(),
            status,
        },
    }) {
        tracing::error!(error = %err, "journal append failed for fetch_http");
    }

    response.map_err(|e| SandboxError::Http(format!("{url} — {e}")))
}
