//! Fake OpenAI-compatible upstream over a real local TCP socket.
#![allow(dead_code)]

use rust_decimal::Decimal;
use serde_json::json;
use simulacra_provider::ResourceBudget;
use simulacra_types::{Message, ProviderStreamEvent, ProviderStreamSink, Role};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CannedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl CannedResponse {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(&body).expect("response JSON should serialize"),
        }
    }

    pub fn sse(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }
}

pub struct FakeHttpClient {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<CapturedRequest>>>,
    pub shutdown: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
}

impl FakeHttpClient {
    pub fn new(response: CannedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake upstream should bind");
        listener
            .set_nonblocking(true)
            .expect("fake upstream should become nonblocking");
        let addr = listener
            .local_addr()
            .expect("listener should have a local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let response = Arc::new(response);

        let requests_for_thread = Arc::clone(&requests);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let response_for_thread = Arc::clone(&response);
        let handle = thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        if shutdown_for_thread.load(Ordering::SeqCst) {
                            break;
                        }
                        let request = read_http_request(&mut stream)
                            .expect("fake upstream should read a complete HTTP request");
                        requests_for_thread
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(request);
                        write_http_response(&mut stream, &response_for_thread)
                            .expect("fake upstream should write a response");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("fake upstream accept failed: {err}"),
                }
            }
        });

        Self {
            addr,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn first_request(&self) -> CapturedRequest {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .first()
            .cloned()
            .expect("expected at least one captured request")
    }
}

impl Drop for FakeHttpClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("fixture thread should join cleanly");
        }
    }
}

pub fn read_http_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    let mut buffer = Vec::new();
    let mut header_end = None;
    while header_end.is_none() {
        let mut chunk = [0_u8; 1024];
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => return Err(err),
        };
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n");
    }
    let header_end = header_end.expect("HTTP request should include header terminator");
    let header_text = std::str::from_utf8(&buffer[..header_end])
        .expect("HTTP request headers should be valid UTF-8");
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().expect("request line should be present");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request line should include a path")
        .to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => return Err(err),
        };
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(CapturedRequest {
        path,
        headers,
        body,
    })
}

pub fn write_http_response(
    stream: &mut TcpStream,
    response: &CannedResponse,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format!("HTTP/1.1 {} OK\r\n", response.status).as_bytes());
    let mut has_length = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if !has_length {
        bytes.extend_from_slice(format!("content-length: {}\r\n", response.body.len()).as_bytes());
    }
    bytes.extend_from_slice(b"connection: close\r\n\r\n");
    bytes.extend_from_slice(&response.body);
    stream.write_all(&bytes)?;
    stream.flush()
}

pub fn fresh_budget() -> ResourceBudget {
    ResourceBudget::new(100_000, 100, Decimal::new(100, 0), 10)
}

pub fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_content: vec![],
    }
}

pub struct NullStreamSink;

impl ProviderStreamSink for NullStreamSink {
    fn emit(&self, _event: ProviderStreamEvent) {}
}

pub fn success_response_json(finish_reason: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl_compat",
        "object": "chat.completion",
        "created": 1_726_000_000_u64,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hi" },
            "finish_reason": finish_reason
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })
}

// ── OpenAiConfig / with_config over a real HTTP round trip ─────────
