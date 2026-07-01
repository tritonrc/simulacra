#[test]
fn each_tool_invocation_produces_a_span_with_gen_ai_tool_name_equal_to_the_tool_name() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, spans, _) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        spans.iter().any(|span| {
            span.fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
        }),
        "expected a tool invocation span with gen_ai.tool.name=file_read, got {spans:#?}"
    );
}

#[test]
fn tool_invocation_spans_are_children_of_the_agent_turn_span() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, spans, _) = capture_async(|| {
        let agent_turn = tracing::info_span!("agent_turn");
        let _guard = agent_turn.enter();
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        spans.iter().any(|span| {
            span.fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
                && span.parent_name.as_deref() == Some("agent_turn")
        }),
        "expected a tool span under agent_turn, got {spans:#?}"
    );
}

#[test]
fn tool_errors_are_logged_at_error_level_with_the_tool_name_and_error_message() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), budget_with_turns_exhausted());

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "shell_exec",
            json!({ "command": "echo hello" }),
            &capability,
        ))
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .get("gen_ai.tool.name")
                    .map(|value| value == "shell_exec")
                    .unwrap_or(false)
                && event
                    .fields
                    .values()
                    .any(|value| value.to_ascii_lowercase().contains("turns"))
        }),
        "expected an ERROR log with tool name and message, got {events:#?}"
    );
}

#[test]
fn tool_results_are_captured_as_events_on_the_tool_span_per_gen_ai_tool_message_convention() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        ))
    });

    assert!(
        events.iter().any(|event| {
            event.current_span.is_some()
                && event.fields.contains_key("gen_ai.tool.message")
                && event
                    .fields
                    .get("gen_ai.tool.name")
                    .map(|value| value == "file_read")
                    .unwrap_or(false)
        }),
        "expected a gen_ai.tool.message event on the tool span, got {events:#?}"
    );
}

#[test]
fn tool_result_event_message_is_bounded_but_preserves_full_result_length() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    let large_content = "x".repeat(10_000);
    harness
        .vfs
        .write("/workspace/large.txt", large_content.as_bytes())
        .unwrap();

    let (_, _, events) = capture_async(|| {
        run_async(harness.registry.call(
            "file_read",
            json!({ "path": "/workspace/large.txt" }),
            &capability,
        ))
    });

    let event = events
        .iter()
        .find(|event| {
            event
                .fields
                .get("gen_ai.tool.name")
                .map(|value| value == "file_read")
                .unwrap_or(false)
                && event.fields.contains_key("gen_ai.tool.message")
        })
        .expect("expected file_read tool result event");
    let message = event
        .fields
        .get("gen_ai.tool.message")
        .expect("message should be present");

    assert!(
        message.len() < large_content.len(),
        "telemetry message should be bounded, got {} bytes",
        message.len()
    );
    assert_eq!(
        event.fields.get("gen_ai.tool.message_truncated"),
        Some(&"true".to_string())
    );
    assert!(
        event
            .fields
            .get("gen_ai.tool.result_length")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|len| len > large_content.len()),
        "full serialized result length should be preserved, got {event:?}"
    );
}
