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

#[derive(Debug, Default)]
struct FailingJournal;

impl JournalStorage for FailingJournal {
    fn append(&self, _entry: JournalEntry) -> Result<(), JournalError> {
        Err(JournalError::Storage("journal unavailable".to_string()))
    }

    fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(Vec::new())
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
        Err(JournalError::Storage("journal unavailable".to_string()))
    }

    fn fork_from(
        &self,
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Err(JournalError::Storage("journal unavailable".to_string()))
    }

    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
enum HookAction {
    Continue,
    Deny(&'static str),
    RedactRequestHeader(&'static str, &'static str, &'static str),
    RedactResponseHeader(&'static str, &'static str, &'static str),
    RewriteRequestUrl(String),
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
            HookAction::RewriteRequestUrl(url) => {
                if !matches!(phase, Phase::Before) {
                    return Ok(Verdict::continue_unchanged());
                }
                let mut request: FetchRequest = serde_json::from_str(context)
                    .expect("RecordingHookModule before-phase context should be FetchRequest");
                request.url = url.clone();
                let modified =
                    serde_json::to_string(&request).expect("FetchRequest should serialize");
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
