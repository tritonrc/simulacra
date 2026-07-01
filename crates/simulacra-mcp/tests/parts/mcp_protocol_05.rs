/// A journal implementation that records the global ordering sequence number
/// at which each append occurs, enabling verification that journal writes
/// happen before subsequent side effects (like HTTP dispatch).
#[derive(Debug)]
struct OrderingJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
    /// Records the sequence number from the shared counter at each append.
    append_sequence_numbers: Mutex<Vec<usize>>,
    /// Shared counter incremented by both journal and HTTP server to track ordering.
    ordering_counter: Arc<AtomicUsize>,
}

impl OrderingJournalStorage {
    fn new(ordering_counter: Arc<AtomicUsize>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            append_sequence_numbers: Mutex::new(Vec::new()),
            ordering_counter,
        }
    }

    fn append_sequence_numbers(&self) -> Vec<usize> {
        self.append_sequence_numbers
            .lock()
            .expect("ordering mutex should not be poisoned")
            .clone()
    }
}

impl JournalStorage for OrderingJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let seq = self.ordering_counter.fetch_add(1, Ordering::SeqCst);
        self.append_sequence_numbers
            .lock()
            .expect("ordering mutex should not be poisoned")
            .push(seq);
        self.entries
            .lock()
            .expect("journal mutex should not be poisoned")
            .push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .expect("journal mutex should not be poisoned")
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

/// Spawns a JSON-RPC server that increments a shared ordering counter when it receives
/// a tools/call request, allowing tests to verify journal-before-dispatch ordering.
fn spawn_ordering_json_rpc_server(
    tools_list_body: &str,
    tool_call_body: &str,
    ordering_counter: Arc<AtomicUsize>,
) -> JsonRpcTestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("JSON-RPC test server should bind");
    listener
        .set_nonblocking(true)
        .expect("JSON-RPC test server should become nonblocking");

    let addr = listener
        .local_addr()
        .expect("JSON-RPC test server should have a local address")
        .to_string();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_for_thread = Arc::clone(&stop);
    let tools_list_body = tools_list_body.to_string();
    let tool_call_body = tool_call_body.to_string();

    let handle = thread::spawn(move || {
        while !stop_for_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));

                    if let Some(request) = read_http_request(&mut stream) {
                        let body = if request.contains("\"method\":\"initialize\"") {
                            json!({
                                "jsonrpc": "2.0",
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "serverInfo": { "name": "fake-mcp", "version": "1.0.0" },
                                    "capabilities": {}
                                }
                            })
                            .to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_body.clone()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            // Record the ordering counter when the server
                            // receives the dispatch — this must be AFTER the
                            // journal append.
                            ordering_counter.fetch_add(1, Ordering::SeqCst);
                            tool_call_body.clone()
                        } else {
                            json!({ "jsonrpc": "2.0", "result": {} }).to_string()
                        };

                        let response = json_http_response(&body);
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    JsonRpcTestServer {
        addr,
        stop,
        handle: Some(handle),
    }
}

// S008 Assertion: MCP tool calls write a ToolCall journal entry BEFORE dispatching the HTTP call.
// The Golden Rule: journal before side effect.
#[tokio::test]
async fn call_tool_records_a_tool_call_journal_entry() {
    let _guard = test_guard().await;

    // Shared ordering counter: journal append and HTTP dispatch each increment it.
    // If journal comes first, its sequence number is lower than the server's.
    let ordering_counter = Arc::new(AtomicUsize::new(0));

    let server = spawn_ordering_json_rpc_server(
        &json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo a payload.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }
                }]
            }
        })
        .to_string(),
        &json!({
            "jsonrpc": "2.0",
            "result": { "echoed": { "query": "simulacra" } }
        })
        .to_string(),
        Arc::clone(&ordering_counter),
    );
    let journal = Arc::new(OrderingJournalStorage::new(Arc::clone(&ordering_counter)));
    let agent_id = AgentId("agent-s008-red".into());
    let mut manager = McpManager::with_journal(
        Arc::clone(&journal) as Arc<dyn JournalStorage>,
        agent_id.clone(),
    );
    let capability = capability_with_mcp_tools(&["mcp:*:echo"]);

    manager
        .connect_http(&server.url("/mcp"))
        .await
        .expect("connect_http should register the MCP server");

    let output = manager
        .call_tool(
            server.server_name(),
            "echo",
            json!({ "query": "simulacra" }),
            &capability,
        )
        .await
        .expect("call_tool should succeed so the journal can be inspected");

    assert_eq!(output["echoed"]["query"], json!("simulacra"));

    let entries = journal
        .read_all(&agent_id)
        .expect("journal entries should be readable");
    assert_eq!(
        entries.len(),
        1,
        "exactly one ToolCall journal entry should be recorded for the MCP invocation"
    );
    assert!(matches!(
        &entries[0].entry,
        JournalEntryKind::ToolCall { tool_name, arguments, .. }
            if tool_name == "echo" && arguments == &json!({ "query": "simulacra" })
    ));

    // Verify ordering: journal append must happen before HTTP dispatch.
    // The ordering counter was 0 initially. Journal append increments it (getting seq 0),
    // then the HTTP server increments it (getting seq 1). Journal seq must be less.
    let journal_seq = journal.append_sequence_numbers();
    assert_eq!(
        journal_seq.len(),
        1,
        "journal should have recorded exactly one append sequence number"
    );
    let total_operations = ordering_counter.load(Ordering::SeqCst);
    assert!(
        total_operations >= 2,
        "both journal append and HTTP dispatch should have incremented the counter, got {total_operations}"
    );
    assert_eq!(
        journal_seq[0], 0,
        "journal append (seq {}) must happen before HTTP dispatch (seq 1) — Golden Rule: journal before side effect",
        journal_seq[0]
    );
}

// S008 O11y Assertion: MCP tool calls produce an execute_tool span with Simulacra MCP attributes.
