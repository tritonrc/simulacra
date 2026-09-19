//! Shared fixtures for the fetch-guard test modules: scripted response
//! builders, capability/budget/agent fixtures, the fetch entry points, and
//! the request-sequence assertions.

use crate::SandboxError;
use crate::http::fetch_http_inner;
use crate::test_support::{NullJournal, ScriptedHttpClient};
use simulacra_http::{HttpClient, HttpResponse};
use simulacra_types::{
    AgentId, CapabilityToken, JournalStorage, NetworkPermission, ResourceBudget,
};
use std::sync::{Arc, Mutex};

pub(super) fn ok(url: &str, body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        status_text: "OK".into(),
        headers: vec![],
        body: body.as_bytes().to_vec(),
        url: url.into(),
        redirected: false,
    }
}

pub(super) fn redirect(url: &str, location: &str) -> HttpResponse {
    HttpResponse {
        status: 302,
        status_text: "Found".into(),
        headers: vec![("Location".into(), location.into())],
        body: vec![],
        url: url.into(),
        redirected: false,
    }
}

pub(super) fn capability(hosts: &[&str]) -> CapabilityToken {
    CapabilityToken {
        network: hosts
            .iter()
            .map(|host| NetworkPermission(format!("net:{host}")))
            .collect(),
        ..Default::default()
    }
}

pub(super) fn budget() -> Arc<Mutex<ResourceBudget>> {
    Arc::new(Mutex::new(ResourceBudget::new(
        0,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )))
}

pub(super) fn journal() -> Arc<dyn JournalStorage> {
    Arc::new(NullJournal)
}

pub(super) fn agent_id() -> AgentId {
    AgentId("fetch-guard-tests".into())
}

pub(super) fn fetch_with(
    client: &dyn HttpClient,
    capability: &CapabilityToken,
    url: &str,
) -> Result<HttpResponse, SandboxError> {
    fetch_full(client, capability, url, "GET", &[])
}

pub(super) fn fetch_full(
    client: &dyn HttpClient,
    capability: &CapabilityToken,
    url: &str,
    method: &str,
    headers: &[(String, String)],
) -> Result<HttpResponse, SandboxError> {
    fetch_http_inner(
        url,
        method,
        &headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
        None,
        capability,
        &budget(),
        &journal(),
        &agent_id(),
        true,
        "fetch_http",
        client,
        None,
    )
}

/// Every request the cell fetch path issues — initial and each redirect
/// hop — must carry `max_redirects: Some(0)`: client-level following off is
/// what makes the per-hop authorization loop the only way forward.
pub(super) fn assert_all_no_follow(client: &ScriptedHttpClient) {
    for request in client.requests() {
        assert_eq!(
            request.max_redirects,
            Some(0),
            "every hop must disable client-level redirect following: {:?}",
            request.url
        );
    }
}

/// The recorded request sequence is exactly the given URLs — no request to
/// any destination outside the expected chain.
pub(super) fn assert_request_sequence(client: &ScriptedHttpClient, expected: &[&str]) {
    assert_eq!(
        client
            .requests()
            .iter()
            .map(|request| request.url.as_str())
            .collect::<Vec<_>>(),
        expected,
        "the recorded request sequence must match exactly"
    );
}
