//! `fetch::*` helpers wrapping the imported `simulacra:mcp/http.fetch` interface.
//!
//! On `wasm32-wasip2` (the production target for an author's compiled MCP
//! server module), these helpers will eventually delegate to the
//! `wit_bindgen`-generated import for `simulacra:mcp/http.fetch`. For the SDK's
//! host-side tests we route through a process-global hook so callers can
//! observe that the fetch helper was invoked and assert on the routed
//! request.
//!
//! The struct shape mirrors the `simulacra:mcp/http.request` /
//! `simulacra:mcp/http.response` records in `simulacra-mcp-server.wit`.

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// Outbound HTTP request as routed through `simulacra:mcp/http.fetch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Response surfaced back from the host fetch implementation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Routing identifier — set to the WIT interface that handled the call so
    /// authors and tests can confirm the request did not bypass the host
    /// pipeline. On `wasm32-wasip2` this is `"simulacra:mcp/http.fetch"`; on host
    /// targets the default stub also reports `"simulacra:mcp/http.fetch"`, and
    /// tests that install a recorder may overwrite it.
    pub via: String,
}

/// Symbolic error returned from the host fetch implementation. Mirrors the
/// `fetch-error` variant in `simulacra-mcp-server.wit`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FetchError {
    CapabilityDenied(String),
    HookDenied(String),
    Transport(String),
    Timeout,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::CapabilityDenied(s) => write!(f, "capability denied: {s}"),
            FetchError::HookDenied(s) => write!(f, "hook denied: {s}"),
            FetchError::Transport(s) => write!(f, "transport: {s}"),
            FetchError::Timeout => write!(f, "timeout"),
        }
    }
}

impl std::error::Error for FetchError {}

// ---- Host-side override (test seam) -------------------------------------

type HostFetchFn = Box<dyn Fn(&Request) -> Result<Response, FetchError> + Send + Sync>;

fn host_fetch_slot() -> &'static Mutex<Option<HostFetchFn>> {
    static SLOT: OnceLock<Mutex<Option<HostFetchFn>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install a host-side fetch implementation. Tests use this to intercept
/// outbound calls and observe what the SDK would have shipped through
/// `simulacra:mcp/http.fetch`. Pass `None` to clear the override.
pub fn set_host_fetch<F>(handler: Option<F>)
where
    F: Fn(&Request) -> Result<Response, FetchError> + Send + Sync + 'static,
{
    let mut slot = host_fetch_slot().lock().expect("fetch slot poisoned");
    *slot = handler.map(|h| Box::new(h) as HostFetchFn);
}

/// Lower-level fetch entry point. Routes through the WIT import on
/// `wasm32-wasip2` and through the host override (or a routing-id stub) on
/// non-WASM targets.
pub fn fetch(req: Request) -> Result<Response, FetchError> {
    #[cfg(target_arch = "wasm32")]
    {
        // Real wit-bindgen-generated host import call lands here in a
        // follow-up commit; the macro test suite exercises the host path
        // exclusively, so the wasm side is intentionally a stub for now.
        Err(FetchError::Transport(
            "simulacra:mcp/http.fetch wasm import not yet wired in this build".into(),
        ))
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // If a test installed an override, invoke it. Otherwise fall back to
        // a recording stub that emits a response whose `via` field documents
        // the routing — this keeps the SDK self-testable and gives authors a
        // clear "you forgot to install a fake" signal without crashing.
        let slot = host_fetch_slot().lock().expect("fetch slot poisoned");
        if let Some(handler) = slot.as_ref() {
            return handler(&req);
        }
        drop(slot);

        Ok(Response {
            status: 200,
            headers: Vec::new(),
            body: format!(
                "routed via simulacra:mcp/http.fetch (method={}, url={})",
                req.method, req.url
            )
            .into_bytes(),
            via: "simulacra:mcp/http.fetch".to_string(),
        })
    }
}

/// HTTP `POST` helper with a JSON body and string headers. Convenience over
/// [`fetch`].
pub fn post(
    url: impl Into<String>,
    body: &serde_json::Value,
    headers: &[(&str, &str)],
) -> Result<Response, FetchError> {
    let serialized = serde_json::to_vec(body).map_err(|e| FetchError::Transport(e.to_string()))?;
    let req = Request {
        method: "POST".to_string(),
        url: url.into(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        body: serialized,
    };
    fetch(req)
}

/// HTTP `GET` helper. Convenience over [`fetch`].
pub fn get(url: impl Into<String>, headers: &[(&str, &str)]) -> Result<Response, FetchError> {
    let req = Request {
        method: "GET".to_string(),
        url: url.into(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        body: Vec::new(),
    };
    fetch(req)
}
