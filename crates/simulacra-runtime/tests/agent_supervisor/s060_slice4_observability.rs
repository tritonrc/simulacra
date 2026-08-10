#[tokio::test]
async fn s060_a41_create_agent_span_and_logs_use_bounded_placement_metadata_only() {
    use tracing::Instrument;

    let (subscriber, captured_spans, captured_events) = setup_capture();
    let skill_marker = "SECRET-S060-SKILL";
    let parent_capability = CapabilityToken {
        spawn_placements: vec!["workspace".into()],
        skill_patterns: vec![format!("skill:{skill_marker}")],
        ..Default::default()
    };
    let parent_budget = Arc::new(Mutex::new(default_budget()));
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        parent_capability.clone(),
        Arc::clone(&parent_budget),
        Arc::new(NoopTaskFactory),
    );
    install_test_journal(&mut supervisor);
    supervisor.set_root_agent_id(AgentId("parent-agent".into()));
    let supervisor = Arc::new(supervisor);

    let instruction_marker = "SECRET-S060-INSTRUCTIONS";
    let task_marker = "SECRET-S060-TASK";
    let raw_instructions = format!("  {instruction_marker} \n");
    let raw_task = format!("  {task_marker} \n");
    let (_capture_guard, _default_guard) = install_capture(subscriber).await;
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(receiver).await })
    };
    let tool = SpawnAgentTool {
        sender,
        allowed_placements: vec!["workspace".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: AgentId("parent-agent".into()),
        parent_budget,
        guidance: None,
    };
    let parent_span = tracing::info_span!("invoke_agent", test_trace = "s060-a41");
    let tool_span = tracing::info_span!(parent: &parent_span, "execute_tool", tool = "spawn_agent");
    let acknowledgement = tool
        .call(
            serde_json::json!({
                "placement": "workspace",
                "instructions": raw_instructions.clone(),
                "task": raw_task.clone(),
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 1,
                    "max_cost": "1",
                    "max_sub_agents": 1
                }
            }),
            &parent_capability,
        )
        .instrument(tool_span)
        .await
        .expect("valid workspace spawn should be accepted");
    let child_id = acknowledgement["child_id"]
        .as_str()
        .expect("spawn acknowledgement child id")
        .to_string();
    drop(tool);
    actor.await.expect("supervisor actor should stop");
    drop(parent_span);

    let spans = captured_spans.lock().expect("captured span lock");
    let create_agents = spans
        .iter()
        .filter(|span| {
            span.name == "create_agent" && span.fields.get("gen_ai.agent.name") == Some(&child_id)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        create_agents.len(),
        1,
        "each accepted child must produce exactly one create_agent span"
    );
    let create_agent = create_agents[0];
    assert_eq!(
        create_agent.fields.get("simulacra.child.placement"),
        Some(&"workspace".to_string())
    );
    assert_eq!(
        create_agent.fields.get("simulacra.child.backend"),
        Some(&"native".to_string())
    );
    assert!(
        !create_agent
            .fields
            .contains_key("simulacra.child.agent_type")
    );
    assert_eq!(
        create_agent.parent_name.as_deref(),
        Some("execute_tool"),
        "the child span must be structurally nested under the spawning tool call"
    );
    let execute_tool = spans
        .iter()
        .find(|span| span.name == "execute_tool")
        .expect("spawning tool trace");
    assert_eq!(execute_tool.parent_name.as_deref(), Some("invoke_agent"));

    let events = captured_events.lock().expect("captured event lock");
    let spawn_event = events
        .iter()
        .find(|event| event.fields.get("child_id") == Some(&child_id))
        .expect("accepted spawn log event");
    assert!(
        create_agent.opened_sequence < spawn_event.sequence,
        "create_agent must open before the accepted-spawn side effect/log"
    );
    assert!(
        create_agent
            .closed_sequence
            .is_some_and(|closed| closed > spawn_event.sequence),
        "create_agent must remain open through the accepted-spawn side effect/log"
    );
    let instruction_length = raw_instructions.len().to_string();
    assert!(
        events.iter().any(|event| {
            event.fields.iter().any(|(key, value)| {
                key.contains("instruction")
                    && key.contains("length")
                    && value == &instruction_length
            })
        }),
        "spawn logs should expose instruction length"
    );

    for (surface, fields) in spans
        .iter()
        .map(|span| ("span", &span.fields))
        .chain(events.iter().map(|event| ("log event", &event.fields)))
    {
        let encoded = format!("{fields:?}");
        for secret in [instruction_marker, task_marker, skill_marker] {
            assert!(
                !encoded.contains(secret),
                "{surface} leaked raw task/instruction/skill text: {encoded}"
            );
        }
    }
}
