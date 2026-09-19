use super::*;

/// The drop report's own message text. Matching on it — rather than on the
/// presence of fields whose names merely end in "input"/"output" — keeps any
/// other event carrying such a field from standing in for this one.
const DROP_REPORT: &str = "context compaction dropped messages";

fn drop_reports(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|event| event.fields.get("message").map(String::as_str) == Some(DROP_REPORT))
        .collect()
}

/// Drives three provider turns against a strategy that keeps the three most
/// recent messages after the system prompt. The conversation reaches the
/// compaction call site at lengths 2, 4 and 6, so only the last of the three
/// compactions drops anything: 6 in, 4 out, 2 dropped. Those three numbers are
/// deliberately distinct, so a counter fed the wrong one cannot pass.
fn two_tool_turns_then_text() -> FakeProvider {
    FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({"n": 1})),
        tool_call_response("echo", serde_json::json!({"n": 2})),
        text_response("done"),
    ])
}

fn echo_registry() -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");
    tools
}

#[tokio::test]
async fn a_truncating_strategy_reports_the_messages_it_dropped() {
    let (subscriber, _spans, captured_events) = setup_capture();
    let mut agent = build_loop(
        two_tool_turns_then_text(),
        echo_registry(),
        Box::new(TruncatingContext { keep_recent: 3 }),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    agent
        .run("one")
        .await
        .expect("three canned responses should complete the run");

    let events = captured_events.lock().unwrap();
    let drop_reports = drop_reports(&events);

    assert_eq!(
        drop_reports.len(),
        1,
        "only the compaction that dropped messages should report; \
         the two that kept every message must stay silent, got {drop_reports:?}"
    );
    let report = drop_reports[0];

    let input = report
        .fields
        .get("input")
        .map(String::as_str)
        .expect("the drop report must name the pre-compaction message count");
    let output = report
        .fields
        .get("output")
        .map(String::as_str)
        .expect("the drop report must name the post-compaction message count");

    assert_eq!(
        (input, output),
        ("6", "4"),
        "the reported lengths must be the real message counts at the call site"
    );
    assert_eq!(
        report.fields.get("agent_id").map(String::as_str),
        Some("test-agent"),
        "the drop report must name the agent whose history was cut, got {:?}",
        report.fields
    );
    assert_eq!(
        report.level, "WARN",
        "dropping conversation history is a warning, not an info"
    );
    assert_eq!(
        report.current_span.as_deref(),
        Some("invoke_agent"),
        "the drop report must land inside the agent span"
    );
}

#[tokio::test]
async fn a_passthrough_strategy_reports_nothing() {
    let (subscriber, _spans, captured_events) = setup_capture();
    let mut agent = build_loop(
        two_tool_turns_then_text(),
        echo_registry(),
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        default_budget(),
    );

    let (_capture_guard, _guard) = install_capture(subscriber).await;
    agent
        .run("one")
        .await
        .expect("three canned responses should complete the run");

    let events = captured_events.lock().unwrap();
    let reported = drop_reports(&events);

    assert!(
        reported.is_empty(),
        "a strategy that drops nothing must report nothing, got {reported:?}"
    );
}
