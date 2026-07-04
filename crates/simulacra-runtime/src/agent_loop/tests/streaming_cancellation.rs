#[tokio::test]
async fn cancellation_during_provider_stream_discards_partial_assistant_text() {
    let provider = StreamingFakeProvider::blocking(text_response("partial final"), vec!["partial"]);
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );
    agent.set_cancellation_token(token.clone());

    let run = agent.run("cancel stream");
    tokio::pin!(run);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(ActivityEvent::Token { ref text }) if text == "partial") {
                    break;
                }
            }
            result = &mut run => {
                panic!("stream completed before cancellation could be signalled: {result:?}");
            }
        }
    }

    token.signal();
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), run)
        .await
        .expect("stream cancellation should finish promptly")
        .unwrap();

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(output
        .messages
        .iter()
        .all(|message| message.content != "partial final"));

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.entry, JournalEntryKind::LlmRequest { .. })));
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::LlmResponse { .. })));
}

#[tokio::test]
async fn cancellation_after_tool_call_delta_does_not_journal_or_append_partial_tool_state() {
    let provider = StreamingFakeProvider::blocking_with_events(
        text_response("partial final"),
        vec![simulacra_types::ProviderStreamEvent::ToolCallDelta {
            index: 0,
            tool_call_id: Some("call-partial".into()),
            name: Some("file_read".into()),
            arguments_delta: "{\"path\":\"/partial\"}".into(),
        }],
    );
    let token = crate::CancellationToken::new(std::time::Duration::from_millis(50));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = Arc::new(crate::ChannelActivitySink::new(tx));
    let mut agent = AgentLoop::new(
        default_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(PassthroughContext),
        journal.clone(),
        default_budget(),
        Some(sink),
        None,
    );
    agent.set_cancellation_token(token.clone());

    let run = agent.run("cancel stream after tool delta");
    tokio::pin!(run);

    loop {
        tokio::select! {
            event = rx.recv() => {
                if matches!(event, Some(ActivityEvent::ToolCallDelta { ref tool_call_id, .. }) if tool_call_id.as_deref() == Some("call-partial")) {
                    break;
                }
            }
            result = &mut run => {
                panic!("stream completed before cancellation could be signalled: {result:?}");
            }
        }
    }

    token.signal();
    let output = tokio::time::timeout(std::time::Duration::from_secs(1), run)
        .await
        .expect("stream cancellation should finish promptly")
        .unwrap();

    assert_eq!(output.exit_reason, ExitReason::Cancelled);
    assert!(output
        .messages
        .iter()
        .all(|message| message.tool_calls.is_empty()));

    let entries = journal.read_all(&AgentId("test-agent".into())).unwrap();
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::LlmResponse { .. })));
    assert!(entries
        .iter()
        .all(|entry| !matches!(entry.entry, JournalEntryKind::ToolCall { .. })));
}
