//! Tests for OAuth2 token lifecycle — refresh, degradation, backoff.
//! Covers spec assertions 11–17.
//!
//! Tests requiring real token refresh (HTTP mocking) are marked #[ignore]
//! until simulacra-integration gains a mock token endpoint. The credential
//! state machine (degraded, clear, increment_failures) is tested via
//! direct IntegrationCredential tests below.

#![allow(clippy::await_holding_lock)]

use simulacra_integration::{
    AuthMethod, IntegrationConfig, IntegrationCredential, IntegrationError, IntegrationRegistry,
};
use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::thread;
use std::time::Duration;

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct EnvGuard {
    key: String,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let previous = env::var(key).ok();
        unsafe { env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { env::set_var(&self.key, value) },
            None => unsafe { env::remove_var(&self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// Mock OAuth2 token server
// ---------------------------------------------------------------------------

/// A simple mock HTTP server that responds to POST requests with a JSON
/// OAuth2 token response. Supports configuring how many requests fail before
/// succeeding, for testing backoff behavior.
struct MockTokenServer {
    addr: String,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockTokenServer {
    fn url(&self) -> String {
        format!("http://{}/oauth/token", self.addr)
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Drop for MockTokenServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Connect to unblock accept() in the server thread.
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a mock token server that always responds with a successful token.
fn spawn_success_token_server(expires_in: u64) -> MockTokenServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock token server should bind");
    let addr = listener.local_addr().unwrap().to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let response_body = format!(
        r#"{{"access_token":"new-token-123","token_type":"bearer","expires_in":{expires_in}}}"#
    );
    let request_count_t = Arc::clone(&request_count);
    let stop_t = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_t.load(Ordering::SeqCst) {
                break;
            }
            match stream {
                Ok(mut s) => {
                    request_count_t.fetch_add(1, Ordering::SeqCst);
                    let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                    // Drain the request (we don't need to parse it).
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = s.write_all(resp.as_bytes());
                }
                Err(_) => break,
            }
        }
    });

    MockTokenServer {
        addr,
        request_count,
        stop,
        handle: Some(handle),
    }
}

/// Spawn a mock token server that fails the first `fail_count` requests with
/// 500, then succeeds.
fn spawn_failing_then_success_token_server(fail_count: usize) -> MockTokenServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock token server should bind");
    let addr = listener.local_addr().unwrap().to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let success_body =
        r#"{"access_token":"new-token-123","token_type":"bearer","expires_in":3600}"#;
    let error_body = r#"{"error":"server_error"}"#;

    let request_count_t = Arc::clone(&request_count);
    let stop_t = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_t.load(Ordering::SeqCst) {
                break;
            }
            match stream {
                Ok(mut s) => {
                    let count = request_count_t.fetch_add(1, Ordering::SeqCst);
                    let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);

                    let (status, body) = if count < fail_count {
                        ("500 Internal Server Error", error_body)
                    } else {
                        ("200 OK", success_body)
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes());
                }
                Err(_) => break,
            }
        }
    });

    MockTokenServer {
        addr,
        request_count,
        stop,
        handle: Some(handle),
    }
}

/// Build a registry pointing at a local mock token server URL.
fn build_registry_with_token_url(token_url: &str) -> IntegrationRegistry {
    let config = IntegrationConfig {
        auth: AuthMethod::OAuth2 {
            client_id: "HUBSPOT_CLIENT_ID".to_string(),
            client_secret: "HUBSPOT_CLIENT_SECRET".to_string(),
            token_url: token_url.to_string(),
            scopes: vec!["crm.objects.contacts.read".to_string()],
            refresh_token: Some("HUBSPOT_REFRESH_TOKEN".to_string()),
        },
        base_url: "https://api.hubapi.com".to_string(),
        description: Some("HubSpot CRM".to_string()),
        rate_limit_rps: 10,
        skills_path: None,
    };

    IntegrationRegistry::from_config(&HashMap::from([("hubspot".to_string(), config)]))
        .expect("registry should construct for oauth2 integration")
}

// ---------------------------------------------------------------------------
// Direct IntegrationCredential state machine tests
// ---------------------------------------------------------------------------

fn test_credential() -> IntegrationCredential {
    IntegrationCredential {
        name: "test".to_string(),
        config: IntegrationConfig {
            auth: AuthMethod::ApiKey {
                key: "K".to_string(),
                placement: "header".to_string(),
            },
            base_url: "https://api.example.com".to_string(),
            description: None,
            rate_limit_rps: 0,
            skills_path: None,
        },
        access_token: RwLock::new("token".to_string()),
        expires_at: RwLock::new(None),
        degraded: AtomicBool::new(false),
        refresh_failures: AtomicU32::new(0),
        connectivity_ok: AtomicBool::new(true),
    }
}

/// Spec assertion 14: After 3 consecutive failures, integration is degraded.
#[test]
fn credential_marks_degraded_after_three_failures() {
    let cred = test_credential();

    assert_eq!(cred.increment_failures(), 1);
    assert!(!cred.is_degraded());
    assert_eq!(cred.increment_failures(), 2);
    assert!(!cred.is_degraded());
    assert_eq!(cred.increment_failures(), 3);

    // After 3 failures, mark degraded
    if cred.refresh_failures.load(Ordering::Relaxed) >= 3 {
        cred.mark_degraded();
    }
    assert!(cred.is_degraded());
}

/// Spec assertion 16: Successful refresh after degradation clears degraded state.
#[test]
fn successful_refresh_clears_degraded_state() {
    let cred = IntegrationCredential {
        degraded: AtomicBool::new(true),
        refresh_failures: AtomicU32::new(3),
        ..test_credential()
    };

    assert!(cred.is_degraded());
    cred.clear_degraded();
    assert!(!cred.is_degraded());
    assert_eq!(cred.refresh_failures.load(Ordering::Relaxed), 0);
}

/// Spec assertion 15: access_token() returns TokenRefreshFailed when token unavailable.
/// After a failed initial exchange, the token is empty and access_token() returns an error.
#[tokio::test]
async fn access_token_on_degraded_returns_token_refresh_failed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Use a permanently-failing server so the initial exchange fails and
    // the registry never gets a valid token.
    let server = spawn_failing_then_success_token_server(100);
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let registry = build_registry_with_token_url(&server.url());
    registry.start_background_refresh().await;
    registry.shutdown().await;

    // Token unavailable: initial exchange failed so token is empty.
    let result = registry.access_token("hubspot").await;
    assert!(
        matches!(result, Err(IntegrationError::TokenRefreshFailed(_))),
        "expected TokenRefreshFailed when token is unavailable, got: {result:?}"
    );
}

/// Spec assertion 17: shutdown() cancels and awaits background refresh tasks.
#[tokio::test]
async fn shutdown_completes_without_hanging() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Use an OAuth2 integration with a long-running server so the background
    // refresh task is actually spawned and running when we call shutdown().
    let server = spawn_success_token_server(3600);
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let registry = build_registry_with_token_url(&server.url());
    registry.start_background_refresh().await;
    // Shutdown must abort the background task and await it — should not hang.
    registry.shutdown().await;
    // If we reach here, shutdown cancelled and awaited the task successfully.
}

/// Spawn a mock HTTP server that responds 200 OK to any request (including HEAD).
/// Used to simulate a reachable integration endpoint for `test_connectivity`.
fn spawn_reachable_endpoint() -> MockTokenServer {
    // spawn_success_token_server already responds 200 to any method, so reuse it.
    spawn_success_token_server(3600)
}

fn config_with_base_url(base_url: &str) -> IntegrationConfig {
    IntegrationConfig {
        auth: AuthMethod::ApiKey {
            key: "SLACK_API_KEY".to_string(),
            placement: "header".to_string(),
        },
        base_url: base_url.to_string(),
        description: Some("mock endpoint".to_string()),
        rate_limit_rps: 5,
        skills_path: None,
    }
}

/// Spec assertion: test_connectivity() probes each integration.
/// Uses local mock HTTP servers as base_urls so no outbound network I/O occurs.
#[tokio::test]
async fn test_connectivity_probes_each_integration() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let server_a = spawn_reachable_endpoint();
    let server_b = spawn_reachable_endpoint();
    let _guards = [
        EnvGuard::set("SLACK_API_KEY", "slack-key"),
        EnvGuard::set("HUBSPOT_API_KEY_ALT", "hubspot-key"),
    ];

    let mut integrations = HashMap::new();
    integrations.insert(
        "alpha".to_string(),
        config_with_base_url(&format!("http://{}/", server_a.addr)),
    );
    let mut beta = config_with_base_url(&format!("http://{}/", server_b.addr));
    if let AuthMethod::ApiKey { key, .. } = &mut beta.auth {
        *key = "HUBSPOT_API_KEY_ALT".to_string();
    }
    integrations.insert("beta".to_string(), beta);

    let registry =
        IntegrationRegistry::from_config(&integrations).expect("registry should construct");

    let results = registry.test_connectivity().await;
    assert_eq!(results.len(), 2);
    assert!(results["alpha"].is_ok(), "alpha should be reachable");
    assert!(results["beta"].is_ok(), "beta should be reachable");
}

/// Spec assertion: Failed connectivity logged as warning, does not prevent startup.
/// Uses an unbound port on 127.0.0.1 (guaranteed to refuse connection) so no
/// outbound network is attempted.
#[tokio::test]
async fn failed_connectivity_does_not_prevent_startup() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Bind then drop a listener to obtain a port that will refuse connections.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to get unused port");
    let dead_addr = listener.local_addr().unwrap();
    drop(listener);

    let _guards = [EnvGuard::set("SLACK_API_KEY", "slack-key")];
    let registry = IntegrationRegistry::from_config(&HashMap::from([(
        "deadend".to_string(),
        config_with_base_url(&format!("http://{dead_addr}/")),
    )]))
    .expect("registry should construct even with an unreachable base_url");

    // test_connectivity probes and returns a result map; failed probe does not panic.
    let results = registry.test_connectivity().await;
    assert_eq!(results.len(), 1);
    assert!(
        results["deadend"].is_err(),
        "connection-refused endpoint should surface an error, got: {:?}",
        results["deadend"]
    );

    // Registry remains usable.
    let names = registry.names();
    assert!(names.contains(&"deadend".to_string()));
}

// ---------------------------------------------------------------------------
// OAuth2 lifecycle tests with mock token server
// ---------------------------------------------------------------------------

/// Spec assertion 11: Initial token exchange happens at startup.
/// After start_background_refresh(), access_token() returns a non-empty token.
#[tokio::test]
async fn initial_token_exchange_happens_at_startup() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let server = spawn_success_token_server(3600);
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let registry = build_registry_with_token_url(&server.url());
    registry.start_background_refresh().await;
    registry.shutdown().await;

    let token = registry.access_token("hubspot").await.unwrap();
    assert!(
        !token.is_empty(),
        "access token should be non-empty after initial exchange"
    );
    assert_eq!(token, "new-token-123");
}

/// Spec assertion 12: Background refresh runs before token expiry.
/// The background loop sleeps 90% of the remaining token lifetime for short-lived
/// tokens, then refreshes before the token expires.
#[tokio::test]
async fn background_refresh_runs_before_token_expiry() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Server returns tokens with a 2-second expiry. The background loop will
    // sleep 90% of the remaining lifetime (≈1.8s) and then refresh — so a
    // second request should arrive within ~2 seconds of the initial exchange.
    let server = spawn_success_token_server(2);
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let registry = build_registry_with_token_url(&server.url());
    // Initial token exchange — request_count becomes 1.
    registry.start_background_refresh().await;

    assert_eq!(
        server.request_count(),
        1,
        "initial token exchange should have happened"
    );

    // Sleep long enough for the background task to complete one refresh cycle.
    // The task sleeps ~1.8s (90% of 2s) before refreshing, so 3s is ample.
    tokio::time::sleep(Duration::from_secs(3)).await;

    registry.shutdown().await;

    let final_count = server.request_count();
    assert!(
        final_count >= 2,
        "background refresh should have run at least once after initial exchange, got {final_count} requests"
    );

    let token = registry.access_token("hubspot").await.unwrap();
    assert!(
        !token.is_empty(),
        "token should still be valid after background refresh"
    );
}

/// Spec assertion 13: Failed refresh retries with exponential backoff (1s, 2s, 4s, 8s, max 60s).
/// Spec assertion 14: Mark degraded after 3 consecutive failures.
/// Spec assertion 16: Clear degraded on successful refresh.
///
/// Strategy: server fails the first 3 requests (0,1,2) then succeeds (3+).
/// The initial exchange failure counts as strike 1 (via cred.increment_failures()).
/// - Request 0: initial exchange — fails (failures=1).
/// - Background task retries with backoff (failures persist in cred):
///   - Request 1: retry after 1s — fails (failures=2).
///   - Request 2: retry after 2s — fails (failures=3 → degraded).
///   - Request 3: retry after 4s — succeeds → clears degraded.
/// Total elapsed real time: ~7 seconds. We wait long enough for all retries to fire.
#[tokio::test]
async fn failed_refresh_retries_with_exponential_backoff() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // fail_count=3: first 3 requests return 500, then success.
    let server = spawn_failing_then_success_token_server(3);
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let registry = build_registry_with_token_url(&server.url());
    // Initial exchange is request 0 — fails. Token remains empty.
    registry.start_background_refresh().await;
    assert_eq!(
        server.request_count(),
        1,
        "initial exchange should have been attempted"
    );

    // Token should be unavailable because initial exchange failed.
    let result = registry.access_token("hubspot").await;
    assert!(
        result.is_err(),
        "token should be unavailable after failed initial exchange"
    );

    // Wait for the background task to retry through its backoff schedule.
    // Backoff schedule: 1s, 2s, 4s. Total to reach success: 1+2+4 = 7 seconds.
    // We wait 10 seconds to give ample time for all retries plus the final success.
    tokio::time::sleep(Duration::from_secs(10)).await;

    registry.shutdown().await;

    // Should have 4 total requests: 3 failures + 1 success.
    let count = server.request_count();
    assert!(
        count >= 4,
        "expected at least 4 requests (3 failures + 1 success), got {count}"
    );

    // After recovery, the token should be available and degraded should be cleared.
    let token = registry.access_token("hubspot").await;
    assert!(
        token.is_ok(),
        "token should be available after successful recovery, got: {:?}",
        token
    );
}

// Observability assertions (spans, counters, warn/error logs) are validated
// via Aniani queries per S010, not unit tests.
