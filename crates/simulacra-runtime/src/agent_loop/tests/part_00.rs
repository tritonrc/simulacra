use super::*;
use crate::InMemoryJournalStorage;
use rust_decimal::Decimal;
use simulacra_types::{
    FinishReason, JournalEntryKind, ProviderError, ProviderResponse, ToolCallMessage,
    ToolDefinition,
};
use std::sync::Mutex;

// -----------------------------------------------------------------------
// Fakes
// -----------------------------------------------------------------------

/// A fake provider that returns canned responses from a Vec, in order.
struct FakeProvider {
    responses: Mutex<Vec<ProviderResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            let mut responses = self
                .responses
                .lock()
                .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))?;
            if responses.is_empty() {
                return Err(ProviderError::Other(
                    "FakeProvider: no more canned responses".into(),
                ));
            }
            Ok(responses.remove(0))
        })
    }
}

/// A pass-through context strategy that returns messages unchanged.
struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}

/// A context strategy that truncates to only system + last N messages.
struct TruncatingContext {
    keep_recent: usize,
}

impl ContextStrategy for TruncatingContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        if messages.is_empty() {
            return vec![];
        }
        let mut result = vec![];
        if messages[0].role == Role::System {
            result.push(messages[0].clone());
            let rest = &messages[1..];
            let start = rest.len().saturating_sub(self.keep_recent);
            result.extend_from_slice(&rest[start..]);
        } else {
            let start = messages.len().saturating_sub(self.keep_recent);
            result.extend_from_slice(&messages[start..]);
        }
        result
    }
}

/// A fake tool that just echoes its arguments.
struct EchoTool;

impl simulacra_types::Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echoes input".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move { Ok(arguments) })
    }
}

struct DenyShellTool;

impl simulacra_types::Tool for DenyShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "deny_shell".into(),
            description: "Always returns a shell capability denial".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, simulacra_types::ToolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            Err(simulacra_types::ToolError::CapabilityDenied(
                simulacra_types::CapabilityDenied {
                    operation: "shell".into(),
                    reason: "shell capability not granted".into(),
                },
            ))
        })
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn text_response(content: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }
}

fn tool_call_response(tool_name: &str, args: serde_json::Value) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "tc-1".into(),
                name: tool_name.into(),
                arguments: args,
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-2".into()),
        model: "test-model".into(),
    }
}

fn default_budget() -> ResourceBudget {
    ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5)
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

// -----------------------------------------------------------------------
// Test 1: Simple text response — one turn, exits Complete
// -----------------------------------------------------------------------
