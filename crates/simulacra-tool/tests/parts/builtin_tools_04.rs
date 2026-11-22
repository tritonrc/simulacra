#[test]
fn each_tool_invocation_produces_a_span_with_gen_ai_tool_name_equal_to_the_tool_name() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, spans, _) = capture_async(|| {
        call_tool(
            &harness,
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        )
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
        call_tool(
            &harness,
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        )
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
fn tool_failures_with_continuing_hooks_log_a_generic_tool_error() {
    let capability = full_capability();
    let mut harness = Harness::new(capability.clone(), unlimited_budget());
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(
        simulacra_hooks::Operation::ToolCall,
        Arc::new(RecordingRewriteHook {
            contexts: contexts.clone(),
            before_arguments: None,
        }),
    );
    harness.registry.set_pipeline(Arc::new(pipeline));

    let (result, _, events) =
        capture_async(|| call_tool(&harness, "file_read", json!({}), &capability));
    let error = result.expect_err("file_read should reject missing arguments");
    assert!(matches!(error, ToolError::InvalidArguments(_)));
    let phases: Vec<_> = contexts
        .lock()
        .unwrap()
        .iter()
        .map(|(phase, _)| *phase)
        .collect();
    assert_eq!(
        phases,
        [simulacra_hooks::Phase::Before, simulacra_hooks::Phase::After]
    );

    let error_message = error.to_string();
    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event.fields.get("message").map(String::as_str) == Some("tool error")
                && event.fields.get("gen_ai.tool.name").map(String::as_str) == Some("file_read")
                && event.fields.get("error") == Some(&error_message)
        }),
        "expected a generic ERROR event for file_read, got {events:#?}"
    );
    assert!(
        events.iter().all(|event| {
            event.fields.get("message").map(String::as_str) != Some("hook denied tool call")
                && !event.fields.contains_key("simulacra.hook.name")
        }),
        "tool failure events should have no hook denial message or hook name, got {events:#?}"
    );
}

#[test]
fn unknown_tool_errors_are_logged_at_error_level_with_the_requested_name() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());

    let (_, _, events) = capture_async(|| {
        call_tool(&harness, "missing_tool", json!({}), &capability)
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .get("gen_ai.tool.name")
                    .map(|value| value == "missing_tool")
                    .unwrap_or(false)
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("unknown tool"))
        }),
        "expected an ERROR log for unknown tool, got {events:#?}"
    );
}

#[test]
fn before_hook_denial_logs_one_error_with_tool_name_hook_name_and_error() {
    let capability = full_capability();
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(ArgumentEchoTool))
        .expect("test tool registration should succeed");
    let mut pipeline = simulacra_hooks::HookPipeline::new();
    pipeline.add(simulacra_hooks::Operation::ToolCall, Arc::new(DenyHook));
    registry.set_pipeline(Arc::new(pipeline));

    let (_, _, events) =
        capture_async(|| call_registry(&registry, "arg_echo", json!({}), &capability));

    let errors: Vec<_> = events
        .iter()
        .filter(|event| event.level == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1, "expected one ERROR event, got {events:#?}");
    let fields = &errors[0].fields;
    assert_eq!(
        fields.get("message").map(String::as_str),
        Some("hook denied tool call")
    );
    assert_eq!(
        fields.get("gen_ai.tool.name").map(String::as_str),
        Some("arg_echo")
    );
    assert_eq!(
        fields.get("simulacra.hook.name").map(String::as_str),
        Some("deny")
    );
    assert_eq!(
        fields.get("error").map(String::as_str),
        Some("execution failed: blocked by test")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.fields.get("message").map(String::as_str) == Some("hook denied tool call")
            })
            .count(),
        1,
        "expected one hook denial event across all levels, got {events:#?}"
    );
    assert!(
        events
            .iter()
            .all(|event| event.fields.get("message").map(String::as_str) != Some("tool error")),
        "hook denial should have no generic tool error event, got {events:#?}"
    );
}

#[test]
fn tool_results_are_captured_as_events_on_the_tool_span_per_gen_ai_tool_message_convention() {
    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/hello.txt", b"hello").unwrap();

    let (_, _, events) = capture_async(|| {
        call_tool(
            &harness,
            "file_read",
            json!({ "path": "/workspace/hello.txt" }),
            &capability,
        )
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
        call_tool(
            &harness,
            "file_read",
            json!({ "path": "/workspace/large.txt" }),
            &capability,
        )
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
            .is_some_and(|len| len == large_content.len()),
        "full model-visible content length should be preserved, got {event:?}"
    );
}
