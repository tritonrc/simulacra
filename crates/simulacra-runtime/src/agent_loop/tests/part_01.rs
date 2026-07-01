#[tokio::test]
async fn simple_text_response_exits_complete() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("Hello, world!")]);
    let mut agent = build_loop(
        provider,
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent.run("Say hello").await.expect("run should succeed");

    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(output.token_usage.input_tokens, 10);
    assert_eq!(output.token_usage.output_tokens, 5);

    // Messages: system + user + assistant
    assert_eq!(output.messages.len(), 3);
    assert_eq!(output.messages[0].role, Role::System);
    assert_eq!(output.messages[1].role, Role::User);
    assert_eq!(output.messages[2].role, Role::Assistant);
    assert_eq!(output.messages[2].content, "Hello, world!");

    // Journal: TurnStart, LlmRequest, LlmResponse
    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("read_all should succeed");
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));
    assert!(matches!(
        entries[1].entry,
        JournalEntryKind::LlmRequest { .. }
    ));
    assert!(matches!(
        entries[2].entry,
        JournalEntryKind::LlmResponse { .. }
    ));
}

#[tokio::test]
async fn proc_budget_mirror_tracks_loop_owned_turn_and_token_updates() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("mirror update")]);
    let initial_budget = default_budget();
    let budget_mirror = Arc::new(Mutex::new(initial_budget.clone()));
    let turn_mirror = Arc::new(AtomicU64::new(0));
    let mut agent = build_loop(
        provider,
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal,
        initial_budget,
    );
    agent.set_proc_budget_mirror(Arc::clone(&budget_mirror), Arc::clone(&turn_mirror));

    let output = agent.run("sync /proc").await.expect("run should succeed");

    assert_eq!(output.used_turns, 1);
    assert_eq!(turn_mirror.load(Ordering::Relaxed), 1);
    let mirrored = budget_mirror.lock().unwrap().clone();
    assert_eq!(mirrored.used_turns, 1);
    assert_eq!(mirrored.used_tokens, 15);
}

// -----------------------------------------------------------------------
// Test 2: Tool call + response — two turns
// -----------------------------------------------------------------------
#[tokio::test]
async fn tool_call_then_text_response() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({"msg": "hi"})),
        text_response("Done!"),
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));

    let mut agent = build_loop(
        provider,
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
    );

    let output = agent
        .run("Use the echo tool")
        .await
        .expect("run should succeed");

    assert_eq!(output.exit_reason, ExitReason::Complete);
    // Usage: turn1 (20+10) + turn2 (10+5) = 30 input, 15 output
    assert_eq!(output.token_usage.input_tokens, 30);
    assert_eq!(output.token_usage.output_tokens, 15);

    // Messages: system + user + assistant(tool_call) + tool_result + assistant(text)
    assert_eq!(output.messages.len(), 5);
    assert_eq!(output.messages[2].role, Role::Assistant);
    assert!(!output.messages[2].tool_calls.is_empty());
    assert_eq!(output.messages[3].role, Role::Tool);
    assert_eq!(output.messages[4].role, Role::Assistant);
    assert_eq!(output.messages[4].content, "Done!");

    // Journal: TurnStart, LlmRequest, LlmResponse, ToolCall, ToolResult, TurnStart, LlmRequest, LlmResponse
    let entries = journal
        .read_all(&AgentId("test-agent".into()))
        .expect("read_all should succeed");
    assert_eq!(entries.len(), 8);
    assert!(matches!(
        entries[3].entry,
        JournalEntryKind::ToolCall { .. }
    ));
    assert!(matches!(
        entries[4].entry,
        JournalEntryKind::ToolResult { .. }
    ));
}

// -----------------------------------------------------------------------
// Test 3: Budget exhaustion — max_turns=1 with tool call
// -----------------------------------------------------------------------
#[tokio::test]
async fn max_turns_exits_max_turns() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    // Provider returns tool calls endlessly, but we cap at 1 turn
    let provider = FakeProvider::new(vec![tool_call_response(
        "echo",
        serde_json::json!({"msg": "loop"}),
    )]);
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));

    let mut config = default_config();
    config.max_turns = 1;

    let mut agent = AgentLoop::new(
        config,
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        None,
        None,
    );

    let output = agent.run("Loop forever").await.expect("run should succeed");
    assert_eq!(output.exit_reason, ExitReason::MaxTurns);
}

// -----------------------------------------------------------------------
// Test 4: Journal entries written before return
// -----------------------------------------------------------------------

/// A provider that captures journal state at the moment `chat()` is called,
/// proving temporal ordering: entries that should be journaled *before*
/// the provider call will be visible in the snapshot.
struct JournalCapturingProvider {
    responses: Mutex<Vec<ProviderResponse>>,
    journal: Arc<dyn JournalStorage>,
    agent_id: AgentId,
    /// Journal entries captured at the moment chat() is called.
    captured: Arc<Mutex<Option<Vec<JournalEntry>>>>,
}

impl Provider for JournalCapturingProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        // Snapshot the journal at the moment the provider is called.
        let snapshot = self.journal.read_all(&self.agent_id).unwrap_or_default();
        *self.captured.lock().unwrap() = Some(snapshot);

        Box::pin(async {
            let mut responses = self
                .responses
                .lock()
                .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))?;
            if responses.is_empty() {
                return Err(ProviderError::Other(
                    "JournalCapturingProvider: no more canned responses".into(),
                ));
            }
            Ok(responses.remove(0))
        })
    }
}

#[tokio::test]
async fn journal_entries_written_before_return() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let agent_id = AgentId("test-agent".into());
    let journal_at_chat_time: Arc<Mutex<Option<Vec<JournalEntry>>>> = Arc::new(Mutex::new(None));

    let provider = JournalCapturingProvider {
        responses: Mutex::new(vec![text_response("Result")]),
        journal: journal.clone(),
        agent_id: agent_id.clone(),
        captured: journal_at_chat_time.clone(),
    };

    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        None,
        None,
    );

    let _ = agent.run("test").await.expect("run should succeed");

    // Verify temporal ordering: at the moment the provider's chat() was called,
    // TurnStart and LlmRequest must already be in the journal.
    let snapshot = journal_at_chat_time
        .lock()
        .unwrap()
        .take()
        .expect("provider should have captured journal state");

    let kinds_at_chat: Vec<&str> = snapshot
        .iter()
        .map(|e| match &e.entry {
            JournalEntryKind::TurnStart => "TurnStart",
            JournalEntryKind::LlmRequest { .. } => "LlmRequest",
            JournalEntryKind::LlmResponse { .. } => "LlmResponse",
            _ => "Other",
        })
        .collect();
    assert_eq!(
        kinds_at_chat,
        vec!["TurnStart", "LlmRequest"],
        "TurnStart and LlmRequest must be journaled BEFORE the provider call — \
             this proves journal-before-return ordering, not just post-hoc entry existence"
    );

    // Also verify the final journal state has all three entries in order.
    let final_entries = journal
        .read_all(&agent_id)
        .expect("read_all should succeed");
    let final_kinds: Vec<&str> = final_entries
        .iter()
        .map(|e| match &e.entry {
            JournalEntryKind::TurnStart => "TurnStart",
            JournalEntryKind::LlmRequest { .. } => "LlmRequest",
            JournalEntryKind::LlmResponse { .. } => "LlmResponse",
            _ => "Other",
        })
        .collect();
    assert_eq!(final_kinds, vec!["TurnStart", "LlmRequest", "LlmResponse"]);
}

// -----------------------------------------------------------------------
// Test 5: Budget check before inference
// -----------------------------------------------------------------------
