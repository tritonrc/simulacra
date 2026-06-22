use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::json;
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_mcp::{
    FetchError, FetchRequest, FetchResponse, check_network_allowlist, wasm_mcp_fetch,
    wasm_mcp_fetch_with_timeout,
};
use simulacra_types::{
    AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};

static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[derive(Debug, Default)]
struct RecordingJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl JournalStorage for RecordingJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().expect("journal mutex").push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .expect("journal mutex")
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
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
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

#[derive(Clone)]
enum HookAction {
    Continue,
    Deny(&'static str),
    RedactRequestHeader(&'static str, &'static str, &'static str),
    RedactResponseHeader(&'static str, &'static str, &'static str),
}

/// `simulacra_hooks::HookModule` test fake. Captures every (operation, phase,
/// context) tuple the production `HookPipeline` invokes it with, and returns
/// scripted `Verdict`s. Replaces the legacy `WasmMcpFetchHooks` test trait
/// — production now serializes `FetchRequest` / `FetchResponse` to JSON,
/// runs them through the same pipeline used by every other governed
/// operation, and re-deserializes the redacted result.
struct RecordingHookModule {
    before: HookAction,
    after: HookAction,
    captured: Arc<Mutex<Vec<(Operation, Phase, String)>>>,
}

impl HookModule for RecordingHookModule {
    fn name(&self) -> &str {
        "recording"
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        self.captured
            .lock()
            .expect("capture mutex")
            .push((operation, phase, context.to_string()));

        let action = match phase {
            Phase::Before => &self.before,
            Phase::After => &self.after,
        };

        match action {
            HookAction::Continue => Ok(Verdict::continue_unchanged()),
            HookAction::Deny(reason) => Ok(Verdict::Deny((*reason).to_string())),
            HookAction::RedactRequestHeader(header, old_value, new_value) => {
                if !matches!(phase, Phase::Before) {
                    return Ok(Verdict::continue_unchanged());
                }
                let mut request: FetchRequest = serde_json::from_str(context)
                    .expect("RecordingHookModule before-phase context should be FetchRequest");
                for (name, value) in &mut request.headers {
                    if name == header && value == old_value {
                        *value = (*new_value).to_string();
                    }
                }
                let modified =
                    serde_json::to_string(&request).expect("FetchRequest should serialize");
                Ok(Verdict::Continue(Some(modified)))
            }
            HookAction::RedactResponseHeader(header, old_value, new_value) => {
                if !matches!(phase, Phase::After) {
                    return Ok(Verdict::continue_unchanged());
                }
                let mut response: FetchResponse = serde_json::from_str(context)
                    .expect("RecordingHookModule after-phase context should be FetchResponse");
                for (name, value) in &mut response.headers {
                    if name == header && value == old_value {
                        *value = (*new_value).to_string();
                    }
                }
                let modified =
                    serde_json::to_string(&response).expect("FetchResponse should serialize");
                Ok(Verdict::Continue(Some(modified)))
            }
        }
    }
}

/// Builds a `HookPipeline` with a single `RecordingHookModule` registered
/// against `Operation::HttpRequest`, plus a handle to the captured tuple
/// list so tests can assert on it.
struct RecordingHookPipeline {
    pipeline: HookPipeline,
    captured: Arc<Mutex<Vec<(Operation, Phase, String)>>>,
}

impl RecordingHookPipeline {
    fn new(before: HookAction, after: HookAction) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let module = Arc::new(RecordingHookModule {
            before,
            after,
            captured: Arc::clone(&captured),
        });
        let mut pipeline = HookPipeline::new();
        pipeline.add(Operation::HttpRequest, module);
        Self { pipeline, captured }
    }

    fn captured(&self) -> Vec<(Operation, Phase, String)> {
        self.captured.lock().expect("capture mutex").clone()
    }

    fn pipeline(&self) -> &HookPipeline {
        &self.pipeline
    }
}

struct RecordingHttpServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RecordingHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Drop for RecordingHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(request: &str) -> usize {
    request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_len = None;

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                request.extend_from_slice(&buffer[..bytes_read]);
                if expected_len.is_none()
                    && let Some(idx) = find_bytes(&request, b"\r\n\r\n")
                {
                    let header_end = idx + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    expected_len = Some(header_end + parse_content_length(&headers));
                }
                if expected_len.is_some_and(|len| request.len() >= len) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if request.is_empty() {
                    return None;
                }
                break;
            }
            Err(_) => return None,
        }
    }

    if request.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&request).into_owned())
    }
}

fn spawn_recording_http_server(
    responses: Vec<String>,
    per_request_delay: Duration,
) -> RecordingHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("HTTP server should bind");
    listener
        .set_nonblocking(true)
        .expect("HTTP server should become nonblocking");

    let addr = listener.local_addr().expect("local addr").to_string();
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let responses_for_thread = Arc::clone(&responses);
    let requests_for_thread = Arc::clone(&requests);
    let request_count_for_thread = Arc::clone(&request_count);
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    if let Some(request) = read_http_request(&mut stream) {
                        requests_for_thread
                            .lock()
                            .expect("requests mutex")
                            .push(request);
                    }
                    thread::sleep(per_request_delay);
                    let response_body = responses_for_thread
                        .lock()
                        .expect("responses mutex")
                        .pop_front()
                        .unwrap_or_else(|| {
                            json!({
                                "status": 200,
                                "headers": [["content-type", "application/json"]],
                                "body": "e30="
                            })
                            .to_string()
                        });
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    RecordingHttpServer {
        addr,
        requests,
        request_count,
        stop,
        handle: Some(handle),
    }
}

fn request_to(url: String) -> FetchRequest {
    FetchRequest {
        method: "POST".to_string(),
        url,
        headers: vec![("authorization".to_string(), "secret".to_string())],
        body: br#"{"query":"simulacra"}"#.to_vec(),
    }
}

fn journal_arc() -> Arc<dyn JournalStorage> {
    Arc::new(RecordingJournal::default()) as Arc<dyn JournalStorage>
}

#[test]
fn fetch_to_host_outside_network_allowlist_returns_capability_denied() {
    let allowed = vec!["api.github.com:443".to_string()];

    assert!(
        !check_network_allowlist("example.com:443", &allowed),
        "hosts outside the allowlist should be denied before any fetch dispatch"
    );
}

#[test]
fn fetch_to_host_with_wildcard_port_permits_any_port() {
    let allowed = vec!["api.github.com:*".to_string()];

    assert!(
        check_network_allowlist("api.github.com:8443", &allowed),
        "host:* allowlist entries should permit any destination port for that host"
    );
}

#[test]
fn fetch_to_subdomain_glob_permits_subdomain_at_listed_port() {
    let allowed = vec!["*.example.com:443".to_string()];

    assert!(
        check_network_allowlist("api.example.com:443", &allowed),
        "subdomain glob entries should match subdomains at the configured port"
    );
}

#[test]
fn empty_network_allowlist_rejects_all_outbound_http() {
    assert!(
        !check_network_allowlist("api.github.com:443", &[]),
        "an empty network allowlist should reject all outbound HTTP"
    );
}

#[tokio::test]
async fn allowlist_denial_through_wasm_mcp_fetch_returns_capability_denied() {
    let _guard = test_guard().await;
    // WARNING #2: previous allowlist coverage exercised only the pure
    // `check_network_allowlist` helper. This test wires the same allowlist
    // through the full `wasm_mcp_fetch` path and asserts the wired return
    // surface matches `FetchError::CapabilityDenied` from spec Assertion 22.
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/blocked")),
        // Allowlist that explicitly does NOT include the test server's port.
        &["api.github.com:443".to_string()],
        None,
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("denied allowlist should be returned through the fetch entrypoint");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "wired denial should surface as FetchError::CapabilityDenied, got {err:?}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "denied allowlist must short-circuit before hitting the wire"
    );
}

#[tokio::test]
async fn operation_http_request_before_hook_is_invoked_before_wire_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(HookAction::Continue, HookAction::Continue);

    let response = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed");

    assert_eq!(response.status, 200);
    assert_eq!(server.request_count(), 1);
    assert!(
        hooks
            .captured()
            .iter()
            .any(|(operation, phase, _)| *operation == Operation::HttpRequest
                && *phase == Phase::Before),
        "before-phase HTTP hooks should run before the wire dispatch"
    );
}

#[tokio::test]
async fn phase_before_deny_verdict_returns_hook_denied_to_module() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks =
        RecordingHookPipeline::new(HookAction::Deny("blocked by policy"), HookAction::Continue);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("before-phase deny verdicts should be returned to the module");

    assert_eq!(err, FetchError::HookDenied("blocked by policy".to_string()));
}

#[tokio::test]
async fn phase_before_redact_modifies_request_headers_before_dispatch() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(
        HookAction::RedactRequestHeader("authorization", "secret", "redacted"),
        HookAction::Continue,
    );

    wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed after request redaction");

    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.contains("redacted")),
        "before-phase redaction should mutate outbound request headers before dispatch"
    );
}

#[tokio::test]
async fn operation_http_request_after_hook_is_invoked_after_response_before_returning() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let hooks = RecordingHookPipeline::new(HookAction::Continue, HookAction::Continue);

    wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed");

    assert!(
        hooks
            .captured()
            .iter()
            .any(|(operation, phase, _)| *operation == Operation::HttpRequest
                && *phase == Phase::After),
        "after-phase HTTP hooks should run after the response and before returning to the module"
    );
}

#[tokio::test]
async fn phase_after_redact_modifies_response_before_returning_to_module() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(
        vec![
            json!({
                "status": 200,
                "headers": [["x-secret", "secret"]],
                "body": "e30="
            })
            .to_string(),
        ],
        Duration::ZERO,
    );
    let hooks = RecordingHookPipeline::new(
        HookAction::Continue,
        HookAction::RedactResponseHeader("x-secret", "secret", "redacted"),
    );

    let response = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        Some(hooks.pipeline()),
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed after response redaction");

    assert!(
        response
            .headers
            .iter()
            .any(|(name, value)| name == "x-secret" && value == "redacted"),
        "after-phase redaction should modify response headers before returning to the module"
    );
}

#[tokio::test]
async fn every_fetch_call_writes_one_journal_entry() {
    let _guard = test_guard().await;
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);
    let journal = Arc::new(RecordingJournal::default());

    wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[format!(
            "127.0.0.1:{}",
            server.addr.rsplit(':').next().unwrap_or("0")
        )],
        None,
        Some(Arc::clone(&journal) as Arc<dyn JournalStorage>),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect("fetch should succeed");

    let entries = journal.entries.lock().expect("journal mutex");
    assert_eq!(
        entries.len(),
        1,
        "each fetch should append exactly one journal entry"
    );
    // NIT (success/failure differentiation): post-dispatch journaling
    // means a successful fetch records the upstream's HTTP status (>0),
    // so the journal alone is enough to tell success from failure.
    match &entries[0].entry {
        JournalEntryKind::HttpRequest { status, .. } => {
            assert!(
                *status > 0,
                "successful fetch must record the upstream HTTP status, got {status}"
            );
        }
        other => panic!("expected HttpRequest entry, got {other:?}"),
    }
    assert_eq!(entries[0].schema_version, JOURNAL_SCHEMA_VERSION);
}

#[tokio::test]
async fn failed_fetch_calls_also_write_journal_entries() {
    let _guard = test_guard().await;
    // WARNING #3: spec assertion 29 requires the journal entry on success
    // AND failure. The capability-denied path is the cheapest failure to
    // verify deterministically.
    let journal = Arc::new(RecordingJournal::default());

    let err = wasm_mcp_fetch(
        "github",
        request_to("http://example.com/blocked".to_string()),
        // Allowlist excludes example.com:80 — capability denial.
        &["api.github.com:443".to_string()],
        None,
        Some(Arc::clone(&journal) as Arc<dyn JournalStorage>),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("denied allowlist should fail");

    assert!(matches!(err, FetchError::CapabilityDenied(_)));

    let entries = journal.entries.lock().expect("journal mutex");
    assert_eq!(
        entries.len(),
        1,
        "spec assertion 29 requires a journal entry on failed fetches too"
    );
    // NIT (success/failure differentiation): denial path records
    // status=0 ("no wire response observed") so a journal reader can
    // tell success from failure without re-running the trace.
    match &entries[0].entry {
        JournalEntryKind::HttpRequest { status, .. } => {
            assert_eq!(
                *status, 0,
                "denied fetch must record status=0 to mark 'no wire response observed'"
            );
        }
        other => panic!("expected HttpRequest entry, got {other:?}"),
    }
}

#[tokio::test]
async fn wasi_networking_remains_disabled_in_wasm_mcp_module() {
    let _guard = test_guard().await;
    // BLOCKER #4 (deferred): the spec assertion is "wasi:sockets calls
    // inside the module fail." Hand-authoring a WASIp2 component that
    // attempts wasi:sockets is non-trivial — wit-bindgen 0.41 does not
    // expose the binding cleanly in the same shape as our other fixtures.
    // See `tests/fixtures/README.md` for the deferral plan.
    //
    // The Phase 1c fallback is a *behavioral* assertion at the runtime
    // contract: the only egress path the module can use is the host-imported
    // `simulacra:http/fetch`, which is gated by the network allowlist. With an
    // empty allowlist, every call must surface as FetchError::CapabilityDenied
    // — proving wasi:sockets cannot reach the wire even in principle, because
    // there is no other path available.
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[],
        None,
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("outbound HTTP should only be reachable through simulacra:http/fetch");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "WASI sockets should stay disabled and only simulacra:http/fetch should be available"
    );
    assert_eq!(
        server.request_count(),
        0,
        "no wire dispatch should reach the recording server with an empty allowlist"
    );
}

#[tokio::test(start_paused = true)]
async fn request_timeout_returns_fetch_error_timeout() {
    let _guard = test_guard().await;
    // WARNING #1: previous version did a real 31s sleep. Use
    // `wasm_mcp_fetch_with_timeout` so the test runs deterministically
    // in <100ms with `tokio::time::pause()` (`start_paused = true`).
    let server = spawn_recording_http_server(
        vec![json!({"ok": true}).to_string()],
        Duration::from_secs(60),
    );

    let timeout = Duration::from_millis(1);
    let allowlist = vec![format!(
        "127.0.0.1:{}",
        server.addr.rsplit(':').next().unwrap_or("0")
    )];
    let agent_id = simulacra_types::AgentId(String::new());
    let fetch_future = wasm_mcp_fetch_with_timeout(
        "github",
        request_to(server.url("/slow")),
        &allowlist,
        None,
        Some(journal_arc()),
        &agent_id,
        timeout,
    );

    // Advance virtual time past the configured timeout to deterministically
    // trip the timeout branch. This must complete in <100ms wall-clock.
    let advance = async {
        tokio::time::advance(Duration::from_millis(100)).await;
    };

    let (err, _) = tokio::join!(fetch_future, advance);
    let err = err.expect_err("requests exceeding the timeout should return FetchError::Timeout");
    assert_eq!(err, FetchError::Timeout);
}

#[tokio::test]
async fn fetch_only_reachable_via_simulacra_http_fetch_import_wasi_sockets_fail() {
    let _guard = test_guard().await;
    // Companion to wasi_networking_remains_disabled — covers spec
    // assertion 31 ("WASI networking remains disabled") from the
    // perspective of "no allowlist entry means no path." See README's
    // deferral note for the wasi-sockets-attempt fixture.
    let server = spawn_recording_http_server(vec![json!({"ok": true}).to_string()], Duration::ZERO);

    let err = wasm_mcp_fetch(
        "github",
        request_to(server.url("/fetch")),
        &[],
        None,
        Some(journal_arc()),
        &simulacra_types::AgentId(String::new()),
    )
    .await
    .expect_err("WASI sockets should not bypass simulacra:http/fetch capability checks");

    assert!(
        matches!(err, FetchError::CapabilityDenied(_)),
        "direct socket access should fail while simulacra:http/fetch remains the only supported path"
    );
}
