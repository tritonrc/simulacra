use crate::CapabilityToken;
use serde::{Deserialize, Serialize};

/// Schema definition for a tool that can be offered to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A concrete tool call from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Result of executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// Tool trait. Object-safe.
pub trait Tool: Send + Sync + 'static {
    fn definition(&self) -> ToolDefinition;

    fn call(
        &self,
        arguments: serde_json::Value,
        capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    >;

    /// When true, the `ToolRegistry` skips its generic `tool_call` before/after
    /// hook invocation — the tool owns its own hook lifecycle. Defaults to false.
    fn handles_own_hooks(&self) -> bool {
        false
    }
}

/// Errors from tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("capability denied: {0}")]
    CapabilityDenied(#[from] crate::CapabilityDenied),
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}
