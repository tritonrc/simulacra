//! End-to-end tests against the real Anthropic API.
//!
//! These are `#[ignore]` by default. Run with:
//!
//! ```sh
//! source .env.local && cargo test -p simulacra-runtime --test e2e -- --ignored
//! ```

use std::sync::Arc;

use rust_decimal::Decimal;
use simulacra_context::SlidingWindowStrategy;
use simulacra_provider::AnthropicProvider;
use simulacra_runtime::{AgentLoop, AgentLoopConfig, InMemoryJournalStorage};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ExitReason, JournalEntryKind, JournalStorage, ResourceBudget,
    ToolDefinition, ToolError,
};

const MODEL: &str = "claude-sonnet-4-20250514";

fn api_key() -> String {
    std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set for e2e tests")
}

fn e2e_budget() -> ResourceBudget {
    // Generous but bounded: 50k tokens, 10 turns, $1 max, 0 = unlimited sub-agents
    ResourceBudget::new(50_000, 10, Decimal::new(1, 0), 0)
}

fn e2e_config() -> AgentLoopConfig {
    AgentLoopConfig {
        agent_id: AgentId("e2e-test".into()),
        system_prompt: "You are a concise test assistant. Keep all responses under 50 words."
            .into(),
        model: MODEL.into(),
        max_turns: 5,
        capability: CapabilityToken::default(),
    }
}

// ---------------------------------------------------------------------------
// Test 1: Minimal — one turn, no tools, text response
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn e2e_minimal_one_turn_no_tools() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = AnthropicProvider::new(api_key(), MODEL);

    let mut agent = AgentLoop::new(
        e2e_config(),
        Box::new(provider),
        ToolRegistry::new(),
        Box::new(SlidingWindowStrategy::new()),
        journal.clone(),
        e2e_budget(),
        None,
        None,
    );

    let output = agent
        .run("What is 2 + 2? Reply with just the number.")
        .await
        .expect("e2e run should succeed");

    // Should complete in one turn with no tool calls
    assert_eq!(output.exit_reason, ExitReason::Complete);

    // Messages: system + user + assistant
    assert_eq!(output.messages.len(), 3);
    assert!(output.messages[2].content.contains('4'));

    // Token usage should be non-zero
    assert!(output.token_usage.input_tokens > 0);
    assert!(output.token_usage.output_tokens > 0);

    // Journal should have TurnStart, LlmRequest, LlmResponse
    let entries = journal
        .read_all(&AgentId("e2e-test".into()))
        .expect("journal read");
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

// ---------------------------------------------------------------------------
// Test 2: Full loop — tool call, tool result, then completion
// ---------------------------------------------------------------------------

/// A simple tool that returns the current (fake) time.
struct GetTimeTool;

impl simulacra_types::Tool for GetTimeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "get_current_time".into(),
            description: "Returns the current UTC time.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn call(
        &self,
        _arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(serde_json::json!("2026-03-11T15:30:00Z")) })
    }
}

#[tokio::test]
#[ignore]
async fn e2e_full_loop_with_tool_call() {
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = AnthropicProvider::new(api_key(), MODEL);

    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(GetTimeTool))
        .expect("test tool registration should succeed");

    let mut config = e2e_config();
    config.system_prompt =
        "You are a test assistant. When asked about the time, use the get_current_time tool. \
         After getting the result, respond with the time. Keep responses under 50 words."
            .into();

    let mut agent = AgentLoop::new(
        config,
        Box::new(provider),
        tools,
        Box::new(SlidingWindowStrategy::new()),
        journal.clone(),
        e2e_budget(),
        None,
        None,
    );

    let output = agent
        .run("What time is it right now?")
        .await
        .expect("e2e run should succeed");

    assert_eq!(output.exit_reason, ExitReason::Complete);

    // Should have: system + user + assistant(tool_call) + tool_result + assistant(text)
    assert!(
        output.messages.len() >= 5,
        "expected at least 5 messages (tool loop), got {}",
        output.messages.len()
    );

    // The response should mention the time from our fake tool
    let last_msg = output.messages.last().unwrap();
    assert!(
        last_msg.content.contains("15:30") || last_msg.content.contains("3:30"),
        "expected response to contain the time, got: {}",
        last_msg.content
    );

    // Token usage across both turns
    assert!(output.token_usage.input_tokens > 0);
    assert!(output.token_usage.output_tokens > 0);

    // Journal should have entries for both turns including ToolCall + ToolResult
    let entries = journal
        .read_all(&AgentId("e2e-test".into()))
        .expect("journal read");

    let tool_calls: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.entry, JournalEntryKind::ToolCall { .. }))
        .collect();
    assert!(
        !tool_calls.is_empty(),
        "expected at least one ToolCall journal entry"
    );

    let tool_results: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.entry, JournalEntryKind::ToolResult { .. }))
        .collect();
    assert!(
        !tool_results.is_empty(),
        "expected at least one ToolResult journal entry"
    );
}
