use super::*;
use crate::InMemoryJournalStorage;
use rust_decimal::Decimal;
use simulacra_types::{
    FinishReason, JournalEntryKind, ProviderError, ProviderResponse, ToolCallMessage,
    ToolDefinition,
};
use std::sync::Mutex;

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

struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}

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

struct LegacyErrorFieldTool;

impl simulacra_types::Tool for LegacyErrorFieldTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "legacy_error_field".into(),
            description: "Returns a legacy object with an error field".into(),
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
        Box::pin(async move { Ok(serde_json::json!({ "error": "legacy failure" })) })
    }
}

struct ExplicitErrorOutputTool;

impl simulacra_types::Tool for ExplicitErrorOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "explicit_error_output".into(),
            description: "Returns an explicit typed error output".into(),
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
            Ok(simulacra_types::ToolOutput::error("explicit failure").to_value())
        })
    }
}

struct NamedErrorOutputTool {
    name: &'static str,
    content: &'static str,
}

impl simulacra_types::Tool for NamedErrorOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Returns an explicit typed error output".into(),
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
        let content = self.content;
        Box::pin(async move { Ok(simulacra_types::ToolOutput::error(content).to_value()) })
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
