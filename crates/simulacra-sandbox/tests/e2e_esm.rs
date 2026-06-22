//! End-to-end integration tests for the complete ESM module system
//! flowing through the AgentCell proxy stack.
//!
//! These tests spin up real HTTP servers, make real network requests
//! through the AgentCell proxy, and verify the full Golden Rule chain
//! fires for every operation.

use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_sandbox::AgentCell;
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, NetworkPermission, PathPattern, ResourceBudget,
    TokenUsage, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Test HTTP server — serves multiple requests with path-based routing
// ---------------------------------------------------------------------------

struct TestHttpServer {
    addr: String,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestHttpServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a test HTTP server that routes requests by path.
/// Each entry in `routes` maps a path to a (status, content-type, body).
fn spawn_routing_server(routes: Vec<(&str, u16, &str, &str)>) -> TestHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr").to_string();
    let request_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let rc = Arc::clone(&request_count);
    let st = Arc::clone(&stop);
    let routes: Vec<(String, u16, String, String)> = routes
        .into_iter()
        .map(|(p, s, ct, b)| (p.to_string(), s, ct.to_string(), b.to_string()))
        .collect();

    let handle = thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    rc.fetch_add(1, Ordering::SeqCst);
                    stream.set_nonblocking(false).expect("stream blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("read timeout");

                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);

                    // Extract path from "GET /path HTTP/1.1"
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");

                    let (status, content_type, body) = routes
                        .iter()
                        .find(|(p, _, _, _)| p == path)
                        .map(|(_, s, ct, b)| (*s, ct.as_str(), b.as_str()))
                        .unwrap_or((404, "text/plain", "not found"));

                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "OK",
                    };

                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    TestHttpServer {
        addr,
        request_count,
        stop,
        handle: Some(handle),
    }
}

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl FakeJournalStorage {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| &e.agent_id == agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|e| JournalError::Storage(e.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
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

struct Harness {
    vfs: Arc<MemoryFs>,
    cell: AgentCell,
    journal: Arc<FakeJournalStorage>,
    budget: Arc<Mutex<ResourceBudget>>,
}

impl Harness {
    fn new(capability: CapabilityToken) -> Self {
        let journal = Arc::new(FakeJournalStorage::default());
        // Budget 0 = unlimited
        let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(
            vfs_dyn,
            capability,
            Arc::clone(&budget),
            journal_dyn,
            http_client,
        );

        Self {
            vfs,
            cell,
            journal,
            budget,
        }
    }

    fn budget_field(&self, field: &str) -> u64 {
        serde_json::to_value(&*self.budget.lock().unwrap())
            .expect("budget should serialize")
            .get(field)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }
}

fn capability(reads: &[&str], writes: &[&str], network: &[&str]) -> CapabilityToken {
    CapabilityToken {
        javascript: true,
        paths_read: reads
            .iter()
            .map(|p| PathPattern((*p).to_string()))
            .collect(),
        paths_write: writes
            .iter()
            .map(|p| PathPattern((*p).to_string()))
            .collect(),
        network: network
            .iter()
            .map(|n| NetworkPermission((*n).to_string()))
            .collect(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Test: full ESM pipeline with a real HTTP server
//
// An agent script imports simulacra:fs (built-in) and a remote module served
// by a real local HTTP server. It writes a file, reads it back, transforms
// the content through the remote module, and prints the result.
//
// Verifies: JS output, VFS roundtrip, budget consumption, journal entries
// (FileWrite, HttpRequest, CodeExecution), and real HTTP request count.
// ---------------------------------------------------------------------------

#[test]
fn esm_full_pipeline_with_real_http_server() {
    let server = spawn_routing_server(vec![(
        "/strings.js",
        200,
        "application/javascript",
        r#"export function shout(s) { return s.toUpperCase() + "!"; }"#,
    )]);

    let h = Harness::new(capability(
        &["/workspace/**"],
        &["/workspace/**"],
        &["net:127.0.0.1"],
    ));

    let turns_before = h.budget_field("used_turns");

    let output = h
        .cell
        .execute_js(&format!(
            r#"
            import {{ readFile, writeFile }} from "simulacra:fs";
            import {{ shout }} from "{url}";

            writeFile("/workspace/greeting.txt", "hello simulacra");
            const content = readFile("/workspace/greeting.txt");
            const result = shout(content);
            console.log(result);
            result;
            "#,
            url = server.url("/strings.js")
        ))
        .expect("ESM pipeline should succeed");

    // -- JS output --
    assert_eq!(output.stdout.trim(), "HELLO SIMULACRA!");
    assert_eq!(output.result.as_deref(), Some("HELLO SIMULACRA!"));

    // -- VFS roundtrip --
    let stored = h.vfs.read("/workspace/greeting.txt").expect("VFS read");
    assert_eq!(String::from_utf8(stored).unwrap(), "hello simulacra");

    // -- Real HTTP request was made --
    assert_eq!(
        server.request_count(),
        1,
        "expected exactly 1 HTTP request to the module server"
    );

    // -- Budget: execute_js consumes 1 turn (module fetches share that turn) --
    let turns_after = h.budget_field("used_turns");
    assert!(
        turns_after > turns_before,
        "expected at least 1 turn consumed, before={turns_before}, after={turns_after}"
    );

    // -- Journal entries --
    let entries = h.journal.entries();

    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::FileWrite { path, .. } if path == "/workspace/greeting.txt"
        )),
        "expected FileWrite journal entry"
    );

    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::HttpRequest { url, method, .. }
                if url.contains("/strings.js") && method == "GET"
        )),
        "expected HttpRequest journal entry for the real module fetch"
    );

    assert!(
        entries.iter().any(|e| matches!(
            &e.entry,
            JournalEntryKind::CodeExecution { language } if language == "javascript"
        )),
        "expected CodeExecution journal entry"
    );
}

// ---------------------------------------------------------------------------
// Test: capability enforcement through ESM simulacra:fs imports
// ---------------------------------------------------------------------------

#[test]
fn esm_simulacra_fs_write_to_denied_path_is_rejected() {
    let h = Harness::new(capability(&["/workspace/**"], &["/workspace/**"], &[]));

    let err = h
        .cell
        .execute_js(
            r#"
            import { writeFile } from "simulacra:fs";
            writeFile("/etc/shadow", "pwned");
            "#,
        )
        .expect_err("writing to /etc/shadow should be denied");

    let msg = err.to_string();
    assert!(
        msg.contains("denied") || msg.contains("/etc/shadow"),
        "error should mention the denial, got: {msg}"
    );
    assert!(
        !h.vfs.exists("/etc/shadow"),
        "denied write must not mutate VFS"
    );
}

// ---------------------------------------------------------------------------
// Test: remote module code is also subject to capability checks
// ---------------------------------------------------------------------------

#[test]
fn remote_module_code_cannot_bypass_capability_checks() {
    // Serve a malicious module that tries to write outside allowed paths
    let server = spawn_routing_server(vec![(
        "/backdoor.js",
        200,
        "application/javascript",
        r#"
        import { writeFile } from "simulacra:fs";
        export function plant() { writeFile("/tmp/backdoor.txt", "payload"); }
        "#,
    )]);

    // Grant network to the server but NO write paths
    let h = Harness::new(capability(&[], &[], &["net:127.0.0.1"]));

    let err = h
        .cell
        .execute_js(&format!(
            r#"
            import {{ plant }} from "{url}";
            plant();
            "#,
            url = server.url("/backdoor.js")
        ))
        .expect_err("remote module fs write should be denied");

    let msg = err.to_string();
    assert!(
        msg.contains("denied") || msg.contains("/tmp/backdoor.txt"),
        "error should surface the denied path, got: {msg}"
    );
    assert!(
        !h.vfs.exists("/tmp/backdoor.txt"),
        "denied remote-module write must not mutate VFS"
    );

    // The server was still hit (module was fetched successfully)
    assert_eq!(server.request_count(), 1, "module should have been fetched");
}
