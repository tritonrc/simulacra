#[tokio::test]
async fn replay_frontier_after_recorded_request_transitions_to_live_provider() {
    // Edge case: when replay stops exactly at the provider boundary, the loop should switch
    // cleanly from recorded TurnStart/LlmRequest entries to a live provider call.
    struct CountingProvider {
        calls: Arc<Mutex<u32>>,
        response: ProviderResponse,
    }

    impl Provider for CountingProvider {
        fn chat<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
            _budget: &'a mut ResourceBudget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                *self
                    .calls
                    .lock()
                    .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))? += 1;
                Ok(self.response.clone())
            })
        }
    }

    let calls = Arc::new(Mutex::new(0));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1000,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 1001,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
    ];

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(CountingProvider {
            calls: Arc::clone(&calls),
            response: text_response("live frontier response"),
        }),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal,
        default_budget(),
        Box::new(FixedClock(2001)),
        Some(replay_entries),
    );

    let output = agent.run("cross the frontier").await.unwrap();

    assert_eq!(output.exit_reason, ExitReason::Complete);
    assert_eq!(output.messages[2].content, "live frontier response");
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn replay_tool_result_preserves_error_state() {
    // Edge case: replay of ToolResult entries must preserve is_error so resumed runs do not
    // silently reinterpret tool failures as successful tool outputs.
    let (content, is_error) = replay_tool_result(&JournalEntryKind::ToolResult {
        tool_call_id: Some("tc-1".into()),

        tool_name: "echo".into(),
        content: "tool exploded".into(),
        is_error: true,
    })
    .expect("tool results should replay");

    assert_eq!(content, "tool exploded");
    assert!(is_error);
}

#[tokio::test]
async fn injected_clock_stays_deterministic_during_replay_resume() {
    // Edge case: resumed runs should timestamp newly appended entries from the injected clock,
    // even while earlier steps are being consumed from the replay journal.
    let journal = Arc::new(InMemoryJournalStorage::new());
    let replay_entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 10,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId("test-agent".into()),
            timestamp_ms: 11,
            entry: JournalEntryKind::LlmRequest {
                model: "test-model".into(),
                message_count: 2,
            },
        },
    ];

    let mut agent = AgentLoop::with_clock_and_replay(
        default_config(),
        Box::new(FakeProvider::new(vec![text_response(
            "deterministic replay",
        )])),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Box::new(FixedClock(42_424)),
        Some(replay_entries),
    );

    let _ = agent.run("resume").await.unwrap();

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|entry| entry.timestamp_ms == 42_424));
}

// -----------------------------------------------------------------------
// S005: Replay iterator frontier behavior
// -----------------------------------------------------------------------
#[test]
fn replay_iterator_frontier_behavior() {
    use crate::replay::JournalReplayIterator;

    let agent_id = AgentId("test-agent".into());
    let entries = vec![
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 1000,
            entry: JournalEntryKind::TurnStart,
        },
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 1001,
            entry: JournalEntryKind::LlmRequest {
                model: "m".into(),
                message_count: 1,
            },
        },
    ];

    let mut iter = JournalReplayIterator::new(entries);

    // Before consuming: not at frontier, 2 remaining
    assert!(!iter.at_frontier());
    assert_eq!(iter.remaining(), 2);
    assert_eq!(iter.position(), 0);

    // Peek doesn't advance
    assert!(iter.peek().is_some());
    assert_eq!(iter.position(), 0);

    // Consume first
    let first = iter.next_recorded();
    assert!(matches!(first, Some(JournalEntryKind::TurnStart)));
    assert_eq!(iter.position(), 1);
    assert_eq!(iter.remaining(), 1);

    // Consume second
    let second = iter.next_recorded();
    assert!(matches!(second, Some(JournalEntryKind::LlmRequest { .. })));
    assert_eq!(iter.position(), 2);

    // Now at frontier
    assert!(iter.at_frontier());
    assert_eq!(iter.remaining(), 0);
    assert!(iter.next_recorded().is_none());
}

#[test]
fn s060_replay_iterator_never_yields_a_typed_wrong_version_kind() {
    use crate::replay::JournalReplayIterator;

    for version in [2, 4] {
        let mut replay = JournalReplayIterator::new(vec![JournalEntry {
            schema_version: version,
            agent_id: AgentId("wrong-version-replay".into()),
            timestamp_ms: 1,
            entry: JournalEntryKind::TurnStart,
        }]);

        assert!(
            replay.peek().is_none(),
            "v{version} replay entry must be rejected before its typed kind is exposed"
        );
        assert!(
            replay.next_recorded().is_none(),
            "v{version} replay entry must never advance or yield its typed kind"
        );
        assert_eq!(replay.position(), 0);
    }
}

#[tokio::test]
async fn s060_with_clock_and_replay_rejects_typed_v2_and_v4_with_exact_mismatch() {
    for version in [2, 4] {
        let journal = Arc::new(InMemoryJournalStorage::new());
        let mut agent = AgentLoop::with_clock_and_replay(
            default_config(),
            Box::new(FakeProvider::new(vec![text_response(
                "wrong-version replay must not reach the provider",
            )])),
            ToolRegistry::new(),
            Box::new(PassthroughContext),
            journal,
            default_budget(),
            Box::new(FixedClock(2_000)),
            Some(vec![JournalEntry {
                schema_version: version,
                agent_id: AgentId("test-agent".into()),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            }]),
        );

        let result = agent.run("reject wrong-version replay").await;
        assert!(
            matches!(
                result,
                Err(RuntimeError::Journal(
                    simulacra_types::JournalError::SchemaVersionMismatch {
                        expected: 3,
                        got
                    }
                )) if got == version
            ),
            "with_clock_and_replay must reject v{version} before using its kind; got {result:?}"
        );
    }
}

// -----------------------------------------------------------------------
// S010: OTel GenAI Semantic Convention Tests
// -----------------------------------------------------------------------
