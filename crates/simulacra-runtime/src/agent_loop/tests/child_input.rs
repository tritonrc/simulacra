#[tokio::test]
async fn queued_child_input_is_appended_before_next_provider_call_in_fifo_order() {
    struct SteeringProvider {
        handle: ChildInputHandle,
        second_call_messages: std::sync::Arc<std::sync::Mutex<Option<Vec<Message>>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Provider for SteeringProvider {
        fn chat<'a>(
            &'a self,
            messages: &'a [Message],
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
                let call = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    self.handle
                        .enqueue("first steer".into())
                        .expect("first steering message should enqueue");
                    self.handle
                        .enqueue("second steer".into())
                        .expect("second steering message should enqueue");
                    Ok(tool_call_response("echo", serde_json::json!({ "ok": true })))
                } else {
                    *self.second_call_messages.lock().unwrap() = Some(messages.to_vec());
                    Ok(text_response("done"))
                }
            })
        }
    }

    let (input_queue, handle) = AgentInputQueue::new();
    let second_call_messages = std::sync::Arc::new(std::sync::Mutex::new(None));
    let provider = SteeringProvider {
        handle,
        second_call_messages: std::sync::Arc::clone(&second_call_messages),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool)).unwrap();
    let mut agent = AgentLoop::new(
        AgentLoopConfig {
            agent_id: AgentId("child".into()),
            system_prompt: "system".into(),
            model: "model".into(),
            max_turns: 3,
            capability: CapabilityToken::default(),
        },
        Box::new(provider),
        registry,
        Box::new(PassthroughContext),
        Arc::new(InMemoryJournalStorage::new()),
        ResourceBudget::new(0, 0, Decimal::ZERO, 0),
        None,
        None,
    );
    agent.set_input_queue(input_queue);

    let output = agent.run("original task").await.unwrap();
    assert_eq!(output.exit_reason, ExitReason::Complete);

    let captured = second_call_messages
        .lock()
        .unwrap()
        .clone()
        .expect("second provider call should be captured");
    let user_messages: Vec<_> = captured
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.content.as_str())
        .collect();
    assert_eq!(
        user_messages,
        vec!["original task", "first steer", "second steer"]
    );
}
