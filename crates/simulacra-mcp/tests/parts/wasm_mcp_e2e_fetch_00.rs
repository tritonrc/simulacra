use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut request = Vec::new();
    let mut buf = [0u8; 2048];

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|window| window == b"\r\n\r\n")
                    || request.len() >= 64 * 1024
                {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    request
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
    let ready = Arc::new(AtomicBool::new(false));
    let requests_th = Arc::clone(&requests);
    let stop_th = Arc::clone(&stop);
    let ready_th = Arc::clone(&ready);
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
        ready_th.store(true, Ordering::SeqCst);
        while !stop_th.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_http_request(&mut stream);
                    if !request.is_empty() {
                        requests_th
                            .lock()
                            .expect("requests mutex")
                            .push(String::from_utf8_lossy(&request).to_string());
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
    while !ready.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }
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
