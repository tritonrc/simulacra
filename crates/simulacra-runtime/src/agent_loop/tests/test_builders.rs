fn text_response(content: &str) -> ProviderResponse {
    simulacra_types::ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: simulacra_types::TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: simulacra_types::FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }
}

fn tool_call_response(tool_name: &str, args: serde_json::Value) -> ProviderResponse {
    simulacra_types::ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![simulacra_types::ToolCallMessage {
                id: "tc-1".into(),
                name: tool_name.into(),
                arguments: args,
            }],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: simulacra_types::TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: simulacra_types::FinishReason::ToolUse,
        provider_response_id: Some("resp-2".into()),
        model: "test-model".into(),
    }
}

fn default_budget() -> ResourceBudget {
    ResourceBudget::new(100_000, 10, rust_decimal::Decimal::new(100, 0), 5)
}

fn default_config() -> AgentLoopConfig {
    AgentLoopConfig {
        agent_id: AgentId("test-agent".into()),
        system_prompt: "You are a test agent.".into(),
        model: "test-model".into(),
        max_turns: 10,
        capability: CapabilityToken::default(),
    }
}

fn build_loop(
    provider: FakeProvider,
    tools: ToolRegistry,
    context_strategy: Box<dyn ContextStrategy>,
    journal: Arc<dyn JournalStorage>,
    budget: ResourceBudget,
) -> AgentLoop {
    AgentLoop::new(
        default_config(),
        Box::new(provider),
        tools,
        context_strategy,
        journal,
        budget,
        None,
        None,
    )
}

struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}
