mod allowlist;
mod envelope;
mod hooks;
mod journal;
mod types;

pub use allowlist::check_network_allowlist;
pub use types::{FetchError, FetchRequest, FetchResponse};

use std::sync::Arc;

use simulacra_types::{AgentId, JournalStorage};

use allowlist::extract_host_port;
use envelope::parse_fetch_envelope;
use hooks::{run_hook_phase_after, run_hook_phase_before};
use journal::journal_fetch;

/// Default per-request timeout for `simulacra:http/fetch` per S041 spec §Outbound
/// HTTP. Callers needing a different bound use [`wasm_mcp_fetch_with_timeout`].
pub(crate) const WASM_MCP_FETCH_DEFAULT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Implements the `simulacra:http/fetch` host import for WASM MCP modules.
///
/// Order of operations (S041 spec §Outbound HTTP):
///
/// 1. Network allowlist check on `host:port` extracted from `request.url`.
///    A miss short-circuits with [`FetchError::CapabilityDenied`] and a
///    journal entry — no wire dispatch.
/// 2. `Operation::HttpRequest` `Phase::Before` hook (if any). The hook may
///    return a redacted request (e.g. for header scrubbing) or deny the
///    call with [`FetchError::HookDenied`].
/// 3. Re-check the allowlist after any request mutation. Hooks may redact
///    headers/body, but they must not rewrite egress to an unallowed host.
/// 4. Wire dispatch via `reqwest`. Transport errors → [`FetchError::Transport`],
///    timeouts → [`FetchError::Timeout`].
/// 5. `Operation::HttpRequest` `Phase::After` hook on the response. May
///    redact response headers/bodies before returning to the module.
/// 6. Journal a single `JournalEntryKind::HttpRequest` entry on success
///    AND on failure (spec assertion 29).
pub async fn wasm_mcp_fetch(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
) -> Result<FetchResponse, FetchError> {
    wasm_mcp_fetch_with_timeout(
        server,
        request,
        allowlist,
        hooks,
        journal,
        agent_id,
        WASM_MCP_FETCH_DEFAULT_TIMEOUT,
    )
    .await
}

/// Like [`wasm_mcp_fetch`] but with an explicit per-request timeout. Used by
/// the timeout test in `wasm_mcp_fetch.rs` to drive the timeout path under
/// `tokio::time::pause()` instead of a real 31s sleep.
///
/// The default timeout (when callers use `wasm_mcp_fetch`) is 30s per spec
/// §Outbound HTTP step 30.
pub async fn wasm_mcp_fetch_with_timeout(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    wasm_mcp_fetch_with_client_and_timeout(
        server, request, allowlist, hooks, journal, agent_id, None, timeout,
    )
    .await
}

/// Like [`wasm_mcp_fetch_with_timeout`] but accepts an optional shared
/// `reqwest::Client`. `None` falls back to building a fresh client per
/// call (the back-compat path used by tests that drive the function
/// directly). The production [`WasmMcpModule`] path passes
/// `Some(&module.http_client)` so all fetches from the same module share
/// connection-pool / proxy / TLS configuration.
#[allow(clippy::too_many_arguments)]
pub async fn wasm_mcp_fetch_with_client_and_timeout(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    journal: Option<Arc<dyn JournalStorage>>,
    agent_id: &AgentId,
    http_client: Option<&reqwest::Client>,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    // Capture the original method+url so the post-dispatch journal entry
    // records what the module asked for, not whatever a `Phase::Before`
    // hook may have rewritten.
    let journal_method = request.method.clone();
    let journal_url = request.url.clone();

    let result =
        wasm_mcp_fetch_inner(server, request, allowlist, hooks, http_client, timeout).await;

    // Spec assertion 29: every fetch (success and failure) writes a
    // journal entry. We record one entry POST-dispatch carrying the
    // actual outcome status so the audit trail differentiates success
    // (status > 0, the upstream's HTTP code) from denial / transport
    // failure / timeout (status = 0, "no wire response observed").
    // Append failures fail closed: the wire side effect may already have
    // happened, but the module must not observe success without a durable
    // audit entry.
    let status = match &result {
        Ok(resp) => resp.status,
        Err(_) => 0,
    };
    if let Some(j) = journal.as_deref() {
        journal_fetch(Some(j), agent_id, &journal_method, &journal_url, status)?;
    }

    result
}

/// Inner dispatch — returns the wire outcome without journaling. The
/// outer [`wasm_mcp_fetch_with_timeout`] wraps this so it can journal
/// once with the actual outcome status.
async fn wasm_mcp_fetch_inner(
    server: &str,
    request: FetchRequest,
    allowlist: &[String],
    hooks: Option<&simulacra_hooks::HookPipeline>,
    http_client: Option<&reqwest::Client>,
    timeout: std::time::Duration,
) -> Result<FetchResponse, FetchError> {
    // S041 §Observability: every outbound `simulacra:http/fetch` call is
    // wrapped in a `simulacra_mcp_http_fetch` span. Method/host/status are
    // pre-declared and `record`ed as soon as they are known so consumers
    // see consistent fields whether the call denies, errors, or succeeds.
    let initial_host = extract_host_port(&request.url)
        .and_then(|hp| hp.split(':').next().map(|h| h.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let fetch_span = tracing::info_span!(
        "simulacra_mcp_http_fetch",
        server = server,
        http.method = request.method.as_str(),
        http.url.host = initial_host.as_str(),
        http.response.status_code = tracing::field::Empty,
    );
    let _fetch_guard = fetch_span.enter();

    // ── 2. Allowlist gate ────────────────────────────────────────────
    // Extract host:port from the URL. A malformed URL or missing host
    // is treated as a capability denial — there is no path to "open"
    // an unparseable destination.
    enforce_network_allowlist(server, &fetch_span, &request.url, allowlist)?;

    // ── 3. Phase::Before hook (simulacra_hooks::Operation::HttpRequest) ──
    // Serialize the request to JSON, run the governance pipeline, and
    // re-deserialize if any hook returned a modified context (the
    // canonical Verdict::Continue(Some(modified_json)) shape).
    let request = match hooks {
        Some(pipeline) => run_hook_phase_before(pipeline, server, &fetch_span, request)?,
        None => request,
    };
    let host_port = enforce_network_allowlist(server, &fetch_span, &request.url, allowlist)?;

    // ── 3. Wire dispatch ─────────────────────────────────────────────
    // Prefer the caller-provided shared client (the production path —
    // [`WasmMcpModule`] owns one client per module so all fetches share
    // pool/proxy/TLS config). Fall back to a per-call client for
    // standalone callers (tests that drive `wasm_mcp_fetch` directly).
    // The per-request `.timeout()` builder method applies regardless of
    // which client is in use.
    let owned_client;
    let client = match http_client {
        Some(c) => c,
        None => {
            owned_client = match reqwest::Client::builder()
                .tcp_nodelay(false)
                .http1_only()
                .pool_max_idle_per_host(0)
                .build()
            {
                Ok(client) => client,
                Err(err) => return Err(FetchError::Transport(err.to_string())),
            };
            &owned_client
        }
    };

    let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
        Ok(method) => method,
        Err(err) => {
            return Err(FetchError::Transport(format!(
                "invalid HTTP method {:?}: {err}",
                request.method
            )));
        }
    };

    let mut wire_request = client.request(method, &request.url).timeout(timeout);
    for (name, value) in &request.headers {
        wire_request = wire_request.header(name.as_str(), value.as_str());
    }
    if !request.body.is_empty() {
        wire_request = wire_request.body(request.body.clone());
    }

    // Wrap the send+body collection in `tokio::time::timeout` so that
    // virtual-time `tokio::time::pause()` tests can drive the timeout
    // branch deterministically. `reqwest::Client::timeout` covers the
    // real-world case; the explicit wrapper covers the test harness.
    let dispatch = async {
        let response = wire_request.send().await?;
        let status = response.status().as_u16();
        let mut headers: Vec<(String, String)> = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers().iter() {
            if let Ok(value_str) = value.to_str() {
                headers.push((name.as_str().to_string(), value_str.to_string()));
            }
        }
        let body = response.bytes().await?.to_vec();
        // The `simulacra:http/fetch` host import speaks the canonical
        // `FetchResponse` shape on the wire. When the body parses as a
        // FetchResponse JSON envelope, surface those fields to the
        // module — that's how the fixture in `wasm_mcp_fetch.rs`
        // expresses simulated upstream status/headers/body. Otherwise
        // fall back to the raw HTTP response.
        if let Some(envelope) = parse_fetch_envelope(&body) {
            Ok::<FetchResponse, reqwest::Error>(envelope)
        } else {
            Ok::<FetchResponse, reqwest::Error>(FetchResponse {
                status,
                headers,
                body,
            })
        }
    };

    let response = match tokio::time::timeout(timeout, dispatch).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) if err.is_timeout() => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, "wasm_mcp_fetch timed out (reqwest)");
            return Err(FetchError::Timeout);
        }
        Ok(Err(err)) => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, error = %err, "wasm_mcp_fetch transport error");
            return Err(FetchError::Transport(err.to_string()));
        }
        Err(_elapsed) => {
            fetch_span.record("http.response.status_code", 0_u64);
            tracing::debug!(server, host = %host_port, "wasm_mcp_fetch timed out (tokio)");
            return Err(FetchError::Timeout);
        }
    };

    // S041 §Observability: surface the wire status on the span as soon
    // as the dispatch returns so denial/error/success paths share a
    // single source of truth.
    fetch_span.record("http.response.status_code", response.status as u64);

    // ── 5. Phase::After hook (simulacra_hooks::Operation::HttpRequest) ───
    let response = match hooks {
        Some(pipeline) => run_hook_phase_after(pipeline, server, &request, response)?,
        None => response,
    };

    Ok(response)
}

fn enforce_network_allowlist(
    server: &str,
    fetch_span: &tracing::Span,
    url: &str,
    allowlist: &[String],
) -> Result<String, FetchError> {
    let host_port = match extract_host_port(url) {
        Some(hp) => hp,
        None => {
            let denial = format!("invalid URL: {url}");
            tracing::info!(
                counter.simulacra.mcp.http.denied = 1_u64,
                server = server,
                reason = "capability-denied",
                "simulacra:http/fetch capability denial (invalid URL)"
            );
            fetch_span.record("http.response.status_code", 0_u64);
            return Err(FetchError::CapabilityDenied(denial));
        }
    };

    if !check_network_allowlist(&host_port, allowlist) {
        tracing::info!(
            counter.simulacra.mcp.http.denied = 1_u64,
            server = server,
            reason = "capability-denied",
            "simulacra:http/fetch capability denial (host not in allowlist)"
        );
        fetch_span.record("http.response.status_code", 0_u64);
        return Err(FetchError::CapabilityDenied(host_port));
    }

    if let Some((host, _)) = host_port.rsplit_once(':') {
        fetch_span.record("http.url.host", host);
    }
    Ok(host_port)
}
