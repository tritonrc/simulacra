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

/// Maximum number of redirects followed before the chain is rejected.
const MAX_REDIRECTS: usize = 5;

/// Extract host (without port) from a URL string.
pub(crate) fn extract_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(ToOwned::to_owned)
}

/// Scheme denial: names the required scheme without embedding the
/// script-chosen URL.
fn https_scheme_error() -> SandboxError {
    SandboxError::Http("https scheme required for network requests".into())
}

/// https is admitted anywhere; cleartext http only for loopback hosts,
/// which never leave the machine. Any other scheme is rejected.
fn scheme_is_admitted(url: &url::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain("localhost")) => true,
            _ => false,
        },
        _ => false,
    }
}

/// A terminal response that followed one or more redirects reports that it
/// did, with the final hop's URL.
fn finalize_response(
    mut response: HttpResponse,
    final_url: &str,
    redirects: usize,
) -> HttpResponse {
    if redirects > 0 {
        response.redirected = true;
        response.url = final_url.to_string();
    }
    response
}

/// Credential headers never ride a redirect: a grant to reach the
/// destination host never authorizes forwarding the origin's credentials.
fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie"
    )
}

/// Append an HttpRequest journal entry; append failures are logged, not fatal.
fn journal_http_request(
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
    method: &str,
    url: &str,
    status: u16,
) {
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
    // Span first — all subsequent events (denials, budget, journal) nest under it.
    // The URL is script-chosen, so it is NOT a span attribute: only the bounded
    // operation name and method ride the span; the host is checked below and the
    // full URL stays in the caller-facing error, not in telemetry. The method is
    // normalized to a closed set so an arbitrary script-supplied string cannot
    // grow the label surface.
    let method_label = match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    };
    let span = tracing::info_span!(
        "sandbox_http_fetch",
        simulacra.operation.name = "sandbox_http_fetch",
        simulacra.http.method = %method_label,
        simulacra.http.status = tracing::field::Empty,
    );
    let _guard = span.enter();

    // Extract host from URL for capability check
    let parsed = url::Url::parse(url).ok();
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .map(ToOwned::to_owned)
        .unwrap_or_default();

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

    // https-only off the loopback: the initial URL is rejected before any
    // request is issued. The rejection is journaled as a failed request so
    // the audit trail keeps one entry per attempted fetch.
    let mut current = match parsed {
        Some(parsed) if scheme_is_admitted(&parsed) => parsed,
        _ => {
            journal_http_request(journal, agent_id, method, url, 0);
            return Err(https_scheme_error());
        }
    };
    // The transport receives the same parsed, serialized URL the checks above
    // authorized.
    let mut request_url = current.to_string();
    let mut hop_headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut redirects = 0usize;

    loop {
        // Redirects are followed manually so every hop is authorized before
        // its request is issued: client-level following stays off.
        let request = HttpRequest {
            url: request_url.clone(),
            method: method.to_string(),
            headers: hop_headers.clone(),
            body: body.map(|b| b.to_vec()),
            timeout_ms,
            max_redirects: Some(0),
        };

        // Execute HTTP request via the injected client
        let outcome = http_client.execute(&request);

        // Record status on the span now that we know it
        let status = match &outcome {
            Ok(resp) => resp.status,
            Err(_) => 0,
        };
        span.record("simulacra.http.status", status);

        // Journal every hop BEFORE returning (even on failure)
        journal_http_request(journal, agent_id, method, &request_url, status);

        let response = outcome.map_err(|e| SandboxError::Http(format!("{request_url} — {e}")))?;

        // Only GET/HEAD auto-follow; other methods get the 3xx as terminal.
        if !(300..400).contains(&response.status) || !matches!(method_label, "GET" | "HEAD") {
            return Ok(finalize_response(response, &request_url, redirects));
        }
        // A 3xx without a Location header is a terminal response.
        let Some((_, location)) = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        else {
            return Ok(finalize_response(response, &request_url, redirects));
        };

        redirects += 1;
        if redirects > MAX_REDIRECTS {
            return Err(SandboxError::Http(format!(
                "redirect limit of {MAX_REDIRECTS} redirects exceeded"
            )));
        }

        // Resolve Location against the current hop's URL, then admit the
        // scheme and authorize the destination host before the next request.
        let destination = current
            .join(location)
            .map_err(|_| SandboxError::Http("invalid redirect location".into()))?;
        if !scheme_is_admitted(&destination) {
            return Err(https_scheme_error());
        }
        let destination_host = destination
            .host_str()
            .map(ToOwned::to_owned)
            .unwrap_or_default();
        check_and_journal_capability(
            || capability.check_network(&destination_host),
            operation_name,
            "network",
            journal,
            agent_id,
        )?;

        hop_headers.retain(|(name, _)| !is_credential_header(name));
        current = destination;
        request_url = current.to_string();
    }
}
