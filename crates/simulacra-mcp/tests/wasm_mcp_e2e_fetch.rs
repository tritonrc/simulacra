//! End-to-end test that drives the full WASM → host fetch seam:
//! a WASM MCP module (`fetcher-mcp.wasm`) calls `simulacra:mcp/http.fetch`
//! through `wit_server::Server::call_call_tool`, which dispatches into
//! the host-side `wasm_mcp_fetch`. This exercises the real allowlist
//! enforcement, the real `simulacra_hooks::HookPipeline` (`Phase::Before` /
//! `Phase::After`), and the real journal append path — all driven by
//! a real WASIp2 component, not by host-side helpers.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_mcp::{FetchRequest, FetchResponse, McpManager, WasmMcpModule, load_wasm_mcp_module};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use tempfile::NamedTempFile;

static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[derive(Default, Debug)]
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
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect())
    }
    fn query_token_usage(&self, _: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }
    fn save_checkpoint(
        &self,
        _: &AgentId,
        _: usize,
        _: CheckpointData,
    ) -> Result<(), JournalError> {
        Ok(())
    }
    fn fork_from(&self, agent_id: &AgentId, idx: usize) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        Ok(entries[..=idx.min(entries.len().saturating_sub(1))].to_vec())
    }
    fn read_from(
        &self,
        agent_id: &AgentId,
        start: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        Ok(entries[start.min(entries.len())..].to_vec())
    }
}

impl RecordingJournal {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().expect("journal mutex").clone()
    }
}

struct CapturingHook {
    captured: Arc<Mutex<Vec<(Operation, Phase, String)>>>,
}

impl HookModule for CapturingHook {
    fn name(&self) -> &str {
        "capturing"
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
        Ok(Verdict::continue_unchanged())
    }
}

struct RedactingHook;

impl HookModule for RedactingHook {
    fn name(&self) -> &str {
        "redactor"
    }
    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        match phase {
            Phase::Before => {
                let mut req: FetchRequest = serde_json::from_str(context)
                    .expect("before-phase context should be FetchRequest");
                for (k, v) in &mut req.headers {
                    if k.eq_ignore_ascii_case("authorization") && v == "secret" {
                        *v = "redacted".to_string();
                    }
                }
                let modified = serde_json::to_string(&req).expect("FetchRequest should serialize");
                Ok(Verdict::Continue(Some(modified)))
            }
            Phase::After => {
                let mut resp: FetchResponse = serde_json::from_str(context)
                    .expect("after-phase context should be FetchResponse");
                for (k, v) in &mut resp.headers {
                    if k.eq_ignore_ascii_case("x-secret") && v == "leak" {
                        *v = "scrubbed".to_string();
                    }
                }
                let modified =
                    serde_json::to_string(&resp).expect("FetchResponse should serialize");
                Ok(Verdict::Continue(Some(modified)))
            }
        }
    }
}

struct DenyingHook;

impl HookModule for DenyingHook {
    fn name(&self) -> &str {
        "denier"
    }
    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        match phase {
            Phase::Before => Ok(Verdict::Deny("blocked by policy".into())),
            Phase::After => Ok(Verdict::continue_unchanged()),
        }
    }
}

fn fetcher_module_path() -> NamedTempFile {
    let path = format!(
        "{}/tests/fixtures/fetcher-mcp.wasm",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).expect("fetcher-mcp.wasm should exist");
    let mut tmp = NamedTempFile::new().expect("temp module should be created");
    tmp.write_all(&bytes).expect("temp write");
    tmp
}

struct RecordingHttpServer {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
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
    fn host_port(&self) -> String {
        self.addr.clone()
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

fn spawn_http_server(
    response_body: &'static str,
    response_headers: Vec<(&'static str, &'static str)>,
) -> RecordingHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local addr").to_string();
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let requests_th = Arc::clone(&requests);
    let stop_th = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut header_block = String::new();
        for (k, v) in &response_headers {
            header_block.push_str(&format!("{k}: {v}\r\n"));
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            header_block,
            response_body.len(),
            response_body
        );
        while !stop_th.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Read longer than reqwest's default request budget so
                    // a heavily-loaded test runner doesn't trip the
                    // mock server into a hung connection.
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n > 0 {
                        requests_th
                            .lock()
                            .expect("requests mutex")
                            .push(String::from_utf8_lossy(&buf[..n]).to_string());
                    }
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    RecordingHttpServer {
        addr,
        requests,
        stop,
        handle: Some(handle),
    }
}

fn capability(server: &str) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: vec![format!("mcp:{server}:fetch")],
        ..Default::default()
    }
}

fn build_module(
    server: &RecordingHttpServer,
    hook: Arc<dyn HookModule>,
    journal: Arc<dyn JournalStorage>,
) -> WasmMcpModule {
    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(journal)
}

#[tokio::test]
async fn wasm_module_fetch_dispatches_through_host_pipeline() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let captured: Arc<Mutex<Vec<(Operation, Phase, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::clone(&captured),
    });

    let module = build_module(&server, hook, Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let result = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed end-to-end");

    let status = result
        .get("status")
        .and_then(Value::as_u64)
        .expect("status");
    assert_eq!(
        status, 200,
        "WASM module should observe HTTP 200 from host fetch"
    );

    // Real HTTP server actually saw the WASM module's request.
    let server_requests = server.requests();
    assert_eq!(
        server_requests.len(),
        1,
        "host fetch should reach the recording server exactly once"
    );
    assert!(
        server_requests[0].contains("GET /data"),
        "WASM module's GET /data should land on the recording server, got: {:?}",
        server_requests[0]
    );

    // Hook pipeline ran for both Before and After phases through the
    // real `simulacra_hooks` machinery (not a parallel test trait).
    let captured_events = captured.lock().expect("capture mutex").clone();
    assert!(
        captured_events
            .iter()
            .any(|(op, phase, _)| *op == Operation::HttpRequest && *phase == Phase::Before),
        "Phase::Before HttpRequest hook should fire for WASM-driven fetch"
    );
    assert!(
        captured_events
            .iter()
            .any(|(op, phase, _)| *op == Operation::HttpRequest && *phase == Phase::After),
        "Phase::After HttpRequest hook should fire for WASM-driven fetch"
    );
}

#[tokio::test]
async fn wasm_module_fetch_to_unallowed_host_returns_capability_denied() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    // Allowlist only an unrelated host so the WASM module's fetch is
    // denied at the host's `check_network_allowlist` gate.
    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec!["api.github.com:443".into()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let err = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect_err("denied fetch should surface as execution failure inside WASM module");

    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("capability_denied")
            || msg.to_lowercase().contains("capability denied"),
        "denied fetch should surface FetchError::CapabilityDenied, got: {msg}"
    );

    // The real HTTP server should never have been contacted.
    assert!(
        server.requests().is_empty(),
        "denied fetch must NOT reach the network"
    );
}

#[tokio::test]
async fn wasm_module_fetch_blocked_by_before_phase_hook_returns_hook_denied() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, Arc::new(DenyingHook));
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let err = manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect_err("hook-denied fetch should surface as execution failure");

    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("hook_denied") || msg.to_lowercase().contains("hook denied"),
        "hook-denied fetch should surface FetchError::HookDenied, got: {msg}"
    );

    // Hook denied before dispatch — recording server never saw a request.
    assert!(
        server.requests().is_empty(),
        "hook-denied fetch must NOT reach the network"
    );
}

#[tokio::test]
async fn wasm_module_fetch_request_redaction_reaches_remote_server() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, Arc::new(RedactingHook));
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let mut last_err = None;
    for _ in 0..3 {
        match manager
            .call_tool(
                "github",
                "fetch",
                json!({ "url": server.url("/data") }),
                &capability("github"),
            )
            .await
        {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    if let Some(err) = last_err {
        panic!("redacted fetch should succeed: {err:?}");
    }

    let server_requests = server.requests();
    assert_eq!(server_requests.len(), 1);
    let req = &server_requests[0];
    assert!(
        req.to_lowercase().contains("authorization: redacted"),
        "before-phase redaction of `authorization: secret -> redacted` should land on remote server, got: {req:?}"
    );
    assert!(
        !req.to_lowercase().contains("authorization: secret"),
        "original `authorization: secret` must NOT reach the network, got: {req:?}"
    );
}

#[tokio::test]
async fn wasm_module_fetch_writes_journal_entry_per_call() {
    let _guard = test_guard().await;

    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    let module = build_module(&server, hook, journal);

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed");

    // Golden Rule: an HttpRequest journal entry was written for the
    // fetch driven by the WASM module — not by the host helper.
    let entries = journal_arc.entries();
    assert!(
        entries
            .iter()
            .any(|e| matches!(e.entry, JournalEntryKind::HttpRequest { .. })),
        "WASM-driven fetch must write an HttpRequest journal entry"
    );
}

#[tokio::test]
async fn wasm_module_fetch_journal_entry_carries_configured_agent_id() {
    let _guard = test_guard().await;

    // Spec §Journal: every fetch entry must be attributed to the agent
    // that drove it. The module is configured with a specific AgentId;
    // entries written by `wasm_mcp_fetch` (via the `simulacra:mcp/http`
    // host import) must carry that AgentId so per-agent replay/audit
    // can read them back via `JournalStorage::read_all(agent_id)`.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    let agent_id = AgentId("agent-007".into());

    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal))
        .with_agent_id(agent_id.clone());

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed");

    // `read_all(agent_id)` only returns entries attributed to this
    // agent. If `wasm_mcp_fetch` had stamped an empty AgentId the
    // entry would be filtered out and this assertion would fail.
    let attributed = journal_arc
        .read_all(&agent_id)
        .expect("read_all should succeed");
    assert!(
        attributed
            .iter()
            .any(|e| matches!(e.entry, JournalEntryKind::HttpRequest { .. })),
        "fetch journal entry should be attributed to the configured agent_id, got entries: {attributed:?}"
    );
}

#[tokio::test]
async fn shared_mcp_manager_attributes_each_fetch_to_calling_agent() {
    let _guard = test_guard().await;

    // Per-agent journal attribution (server mode): one shared
    // `McpManager` + one shared `WasmMcpModule` is reused across many
    // agents. Each agent's outbound `simulacra:mcp/http.fetch` journal
    // entry must carry that agent's `AgentId`, not the module's
    // construction-time bake-in.
    //
    // This is the property `simulacra-server` needs: a single per-process
    // MCP manager (so HTTP/SSE connection pools, cached components,
    // and capability checks are shared) but per-agent audit on the
    // way out. Today's `with_agent_id` baker-in is preserved as a
    // back-compat default for the CLI single-agent path; this test
    // proves the per-call override wins when present.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal_arc = Arc::new(RecordingJournal::default());
    let journal: Arc<dyn JournalStorage> = journal_arc.clone();
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::new(Mutex::new(Vec::new())),
    });

    // The module's bake-in agent_id is "default-cli-agent" (the
    // single-agent process value). Per-call attribution must override
    // it for each of the calling agents below.
    let module_file = fetcher_module_path();
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::HttpRequest, hook);
    let module = load_wasm_mcp_module(module_file.path())
        .expect("fetcher-mcp should load")
        .with_network_allowlist(vec![server.host_port()])
        .with_hooks(Arc::new(pipeline))
        .with_journal(Arc::clone(&journal))
        .with_agent_id(AgentId("default-cli-agent".into()));

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    let alice = AgentId("alice".into());
    let bob = AgentId("bob".into());

    manager
        .call_tool_for_agent(
            &alice,
            "github",
            "fetch",
            json!({ "url": server.url("/alice") }),
            &capability("github"),
        )
        .await
        .expect("alice's call_tool should succeed");
    manager
        .call_tool_for_agent(
            &bob,
            "github",
            "fetch",
            json!({ "url": server.url("/bob") }),
            &capability("github"),
        )
        .await
        .expect("bob's call_tool should succeed");

    let alice_entries = journal_arc
        .read_all(&alice)
        .expect("read_all alice should succeed");
    let bob_entries = journal_arc
        .read_all(&bob)
        .expect("read_all bob should succeed");
    let cli_default = AgentId("default-cli-agent".into());
    let default_entries = journal_arc
        .read_all(&cli_default)
        .expect("read_all default should succeed");

    let alice_http: Vec<_> = alice_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();
    let bob_http: Vec<_> = bob_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();
    let default_http: Vec<_> = default_entries
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntryKind::HttpRequest { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect();

    assert!(
        alice_http.iter().any(|u| u.ends_with("/alice")),
        "alice's fetch must be journaled under her agent_id, got: alice={alice_http:?} bob={bob_http:?} default={default_http:?}"
    );
    assert!(
        bob_http.iter().any(|u| u.ends_with("/bob")),
        "bob's fetch must be journaled under his agent_id, got: alice={alice_http:?} bob={bob_http:?} default={default_http:?}"
    );
    assert!(
        !alice_http.iter().any(|u| u.ends_with("/bob")),
        "alice must not see bob's fetch — per-agent attribution is broken"
    );
    assert!(
        !bob_http.iter().any(|u| u.ends_with("/alice")),
        "bob must not see alice's fetch — per-agent attribution is broken"
    );
    assert!(
        default_http.is_empty(),
        "the module's bake-in agent_id default must NOT receive entries when the per-call agent_id is non-empty, got: {default_http:?}"
    );
}

#[tokio::test]
async fn wasm_module_uses_configured_http_client_for_outbound_fetches() {
    let _guard = test_guard().await;

    // W4: a custom `reqwest::Client` installed via
    // `WasmMcpModule::with_http_client` must be the one that issues
    // outbound `simulacra:mcp/http.fetch` calls. We prove this by injecting
    // a client whose default `User-Agent` is set to a sentinel string,
    // then asserting the recording HTTP server saw it on the wire.
    let server = spawn_http_server(r#"{"ok":true}"#, vec![]);
    let journal: Arc<dyn JournalStorage> = Arc::new(RecordingJournal::default());
    let captured: Arc<Mutex<Vec<(Operation, Phase, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn HookModule> = Arc::new(CapturingHook {
        captured: Arc::clone(&captured),
    });

    let custom_client = reqwest::Client::builder()
        // The recording fixture echoes raw bytes from a single read — the
        // sentinel header (rather than `.user_agent(...)`, which on some
        // hyper paths splits the initial write differently) is the
        // safest knob for proving the configured client issues the
        // request.
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "x-simulacra-w4-sentinel",
                reqwest::header::HeaderValue::from_static("present"),
            );
            headers
        })
        // Match the default-client constraints so the recording fixture
        // (single-read, HTTP/1.1) keeps working.
        .tcp_nodelay(false)
        .http1_only()
        .pool_max_idle_per_host(0)
        .build()
        .expect("custom client should build");

    let module = build_module(&server, hook, Arc::clone(&journal)).with_http_client(custom_client);

    let mut manager = McpManager::new();
    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake");

    manager
        .call_tool(
            "github",
            "fetch",
            json!({ "url": server.url("/data") }),
            &capability("github"),
        )
        .await
        .expect("call_tool should succeed with custom client");

    let requests = server.requests();
    assert!(
        !requests.is_empty(),
        "recording server should have observed the outbound request"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.contains("x-simulacra-w4-sentinel")),
        "configured custom client must issue the wire request — sentinel header missing from {requests:?}"
    );
}
