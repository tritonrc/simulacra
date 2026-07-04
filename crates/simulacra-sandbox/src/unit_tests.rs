use crate::*;
use simulacra_http::{HttpError, HttpRequest, HttpResponse as FhHttpResponse};
use simulacra_types::{
    AgentId, CheckpointData, JournalEntry, JournalEntryKind, JournalError, JournalStorage,
    NetworkPermission, PathPattern, TokenUsage, VirtualFs,
};
use simulacra_vfs::MemoryFs;

// ── W2: telemetry attribution for memory denials ─────────────────────

#[test]
fn cap_name_for_read_routes_memory_paths_to_memory_search_scopes() {
    // Memory paths must be labeled as memory denials so the OTel counter
    // `simulacra.sandbox.capability.denials{operation="memory_search_scopes"}`
    // attributes correctly. Without this, memory denials would land under
    // `paths_read` and mask the real cause.
    assert_eq!(
        cap_name_for_read("/var/memory/self/note.md"),
        "memory_search_scopes"
    );
    assert_eq!(
        cap_name_for_read("/mnt/policies/hr.pdf"),
        "memory_search_scopes"
    );
    assert_eq!(cap_name_for_read("/var/memory"), "memory_search_scopes");
    assert_eq!(cap_name_for_read("/mnt"), "memory_search_scopes");
}

#[test]
fn cap_name_for_read_routes_non_memory_paths_to_paths_read() {
    assert_eq!(cap_name_for_read("/workspace/file.md"), "paths_read");
    assert_eq!(cap_name_for_read("/etc/passwd"), "paths_read");
    // Lookalikes must NOT be classified as memory.
    assert_eq!(cap_name_for_read("/var/memory.bak/x"), "paths_read");
    assert_eq!(cap_name_for_read("/mntfoo/x"), "paths_read");
    assert_eq!(cap_name_for_read("/Var/Memory/x"), "paths_read");
}

#[test]
fn cap_name_for_write_routes_memory_paths_to_memory_write_scopes() {
    assert_eq!(
        cap_name_for_write("/var/memory/self/note.md"),
        "memory_write_scopes"
    );
    assert_eq!(
        cap_name_for_write("/mnt/policies/hr.pdf"),
        "memory_write_scopes"
    );
    assert_eq!(cap_name_for_write("/var/memory"), "memory_write_scopes");
}

#[test]
fn cap_name_for_write_routes_non_memory_paths_to_paths_write() {
    assert_eq!(cap_name_for_write("/workspace/file.md"), "paths_write");
    assert_eq!(cap_name_for_write("/var/memory.bak/x"), "paths_write");
    assert_eq!(cap_name_for_write("/mntfoo/x"), "paths_write");
}

struct NullJournal;

#[derive(Default)]
struct CapturingJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl CapturingJournal {
    fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl JournalStorage for NullJournal {
    fn append(&self, _entry: JournalEntry) -> Result<(), JournalError> {
        Ok(())
    }
    fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
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
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
}

impl JournalStorage for CapturingJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self.entries())
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
        _agent_id: &AgentId,
        _checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }

    fn read_from(
        &self,
        _agent_id: &AgentId,
        _start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(vec![])
    }
}

fn make_cell(capability: CapabilityToken) -> AgentCell {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        0,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let journal: Arc<dyn JournalStorage> = Arc::new(NullJournal);
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(vfs, capability, budget, journal, http_client)
}

fn make_cell_with_journal(
    capability: CapabilityToken,
    journal: Arc<dyn JournalStorage>,
) -> AgentCell {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        0,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(vfs, capability, budget, journal, http_client)
}

#[test]
fn shell_denied_without_capability() {
    let cell = make_cell(CapabilityToken {
        shell: false,
        ..Default::default()
    });
    let err = cell.execute_shell("echo hello").unwrap_err();
    assert!(matches!(err, SandboxError::CapabilityDenied(_)));
}

#[test]
fn shell_allowed_with_capability() {
    let cell = make_cell(CapabilityToken {
        shell: true,
        ..Default::default()
    });
    let result = cell.execute_shell("echo hello").unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "hello");
}

#[test]
fn js_denied_without_capability() {
    let cell = make_cell(CapabilityToken {
        javascript: false,
        ..Default::default()
    });
    let err = cell.execute_js("1+1").unwrap_err();
    assert!(matches!(err, SandboxError::CapabilityDenied(_)));
}

#[test]
fn shell_denial_surfaces_operation_and_reason_to_agent() {
    let cell = make_cell(CapabilityToken {
        shell: false,
        ..Default::default()
    });

    let err = cell.execute_shell("echo hello").unwrap_err();
    let SandboxError::CapabilityDenied(denied) = err else {
        panic!("expected a capability denial");
    };

    assert_eq!(denied.operation, "shell");
    assert_eq!(denied.reason, "shell capability not granted");
}

#[test]
fn shell_execution_records_shell_command_entry_before_return() {
    let journal = Arc::new(CapturingJournal::default());
    let cell = make_cell_with_journal(
        CapabilityToken {
            shell: true,
            ..Default::default()
        },
        journal.clone(),
    );

    let result = cell.execute_shell("echo hello").unwrap();
    let entries = journal.entries();

    assert_eq!(result.exit_code, 0);
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::ShellCommand { command, exit_code }
                if command == "echo hello" && *exit_code == 0
        )
    }));
}

#[test]
fn file_write_records_file_write_entry_before_return() {
    let journal = Arc::new(CapturingJournal::default());
    let cell = make_cell_with_journal(
        CapabilityToken {
            paths_write: vec![PathPattern("/**".into())],
            ..Default::default()
        },
        journal.clone(),
    );

    cell.write_file("/tmp/output.txt", b"hello journal")
        .unwrap();

    let entries = journal.entries();
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::FileWrite { path, size_bytes }
                if path == "/tmp/output.txt" && *size_bytes == 13
        )
    }));
}

#[test]
fn js_execution_failure_still_records_code_execution_entry_before_return() {
    let journal = Arc::new(CapturingJournal::default());
    let cell = make_cell_with_journal(
        CapabilityToken {
            javascript: true,
            ..Default::default()
        },
        journal.clone(),
    );

    let err = cell
        .execute_js("function broken(")
        .expect_err("invalid JavaScript should fail");
    assert!(matches!(err, SandboxError::Js(_)));

    let entries = journal.entries();
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::CodeExecution { language } if language == "javascript"
        )
    }));
}

#[test]
fn http_failure_still_records_http_request_entry_before_return() {
    let journal = Arc::new(CapturingJournal::default());
    let cell = make_cell_with_journal(
        CapabilityToken {
            network: vec![NetworkPermission("net:127.0.0.1".into())],
            ..Default::default()
        },
        journal.clone(),
    );

    let err = cell
        .fetch_http("http://127.0.0.1:9/journal-red", "GET", &[], None, None)
        .expect_err("connection-refused HTTP request should fail");
    assert!(matches!(err, SandboxError::Http(_)));

    let entries = journal.entries();
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry.entry,
            JournalEntryKind::HttpRequest { method, url, status }
                if method == "GET"
                    && url == "http://127.0.0.1:9/journal-red"
                    && *status == 0
        )
    }));
}

// ── Mock HttpClient for shell HTTP proxy tests ─────────────────────

/// A mock [`HttpClient`] that returns a canned response or error.
struct MockHttpClient {
    response: Mutex<Option<Result<FhHttpResponse, HttpError>>>,
}

impl MockHttpClient {
    fn with_ok(status: u16, body: &[u8]) -> Self {
        Self {
            response: Mutex::new(Some(Ok(FhHttpResponse {
                status,
                status_text: "OK".into(),
                headers: vec![],
                body: body.to_vec(),
                url: String::new(),
                redirected: false,
            }))),
        }
    }
}

impl simulacra_http::HttpClient for MockHttpClient {
    fn execute(&self, _request: &HttpRequest) -> Result<FhHttpResponse, HttpError> {
        let slot = self.response.lock().unwrap();
        // Re-create the response each time so the mock is reusable
        match slot.as_ref() {
            Some(Ok(resp)) => Ok(resp.clone()),
            Some(Err(e)) => Err(HttpError::Network(e.to_string())),
            None => panic!("MockHttpClient: no response configured"),
        }
    }
}

fn make_cell_full(
    capability: CapabilityToken,
    budget: Arc<Mutex<ResourceBudget>>,
    journal: Arc<dyn JournalStorage>,
    http_client: Arc<dyn simulacra_http::HttpClient>,
) -> AgentCell {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    AgentCell::new(vfs, capability, budget, journal, http_client)
}

// ── Task 5: shell curl routes through AgentCellShellHttpProxy ─────

#[test]
fn shell_curl_denied_when_network_capability_missing() {
    let journal = Arc::new(CapturingJournal::default());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        0,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(MockHttpClient::with_ok(200, b"should not reach"));
    let cell = make_cell_full(
        CapabilityToken {
            shell: true,
            network: vec![], // no network permission
            ..Default::default()
        },
        budget,
        journal,
        http_client,
    );

    let result = cell
        .execute_shell("curl http://denied.example.com")
        .unwrap();
    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("capability denied"),
        "stderr should contain 'capability denied', got: {}",
        result.stderr
    );
}

// ── Task 6: Budget verification ─────────────────────────────────────

#[test]
fn shell_curl_increments_used_turns_for_both_shell_and_http() {
    let journal = Arc::new(CapturingJournal::default());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        10,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(MockHttpClient::with_ok(200, b"hello"));
    let cell = make_cell_full(
        CapabilityToken {
            shell: true,
            network: vec![NetworkPermission("net:allowed.example.com".into())],
            ..Default::default()
        },
        Arc::clone(&budget),
        journal,
        http_client,
    );

    let result = cell
        .execute_shell("curl http://allowed.example.com/data")
        .unwrap();
    assert_eq!(result.exit_code, 0);

    let b = budget.lock().unwrap();
    // 1 turn for the shell command + 1 turn for the HTTP request = 2
    assert_eq!(
        b.used_turns, 2,
        "expected 2 used_turns (1 shell + 1 HTTP), got {}",
        b.used_turns
    );
}

#[test]
fn shell_curl_records_both_shell_and_http_journal_entries() {
    let journal = Arc::new(CapturingJournal::default());
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        10,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(MockHttpClient::with_ok(200, b"response body"));
    let cell = make_cell_full(
        CapabilityToken {
            shell: true,
            network: vec![NetworkPermission("net:api.example.com".into())],
            ..Default::default()
        },
        budget,
        journal.clone(),
        http_client,
    );

    let result = cell
        .execute_shell("curl http://api.example.com/endpoint")
        .unwrap();
    assert_eq!(result.exit_code, 0);

    let entries = journal.entries();

    // Verify an HttpRequest entry was journaled
    let has_http_entry = entries.iter().any(|e| {
        matches!(
            &e.entry,
            JournalEntryKind::HttpRequest { method, url, status }
                if method == "GET"
                    && url == "http://api.example.com/endpoint"
                    && *status == 200
        )
    });
    assert!(
        has_http_entry,
        "journal should contain an HttpRequest entry for the curl call"
    );

    // Verify a ShellCommand entry was journaled
    let has_shell_entry = entries.iter().any(|e| {
        matches!(
            &e.entry,
            JournalEntryKind::ShellCommand { command, exit_code }
                if command == "curl http://api.example.com/endpoint" && *exit_code == 0
        )
    });
    assert!(
        has_shell_entry,
        "journal should contain a ShellCommand entry for the curl call"
    );
}
