use serde::{Deserialize, Serialize};

/// Unique identifier for an agent instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

/// Role in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_calls: Vec<ToolCallMessage>,
    pub tool_call_id: Option<String>,
    /// Provider-native content blocks that must round-trip unchanged.
    ///
    /// Anthropic Fable 5 can return `thinking` and `redacted_thinking` blocks
    /// alongside tool calls. The Messages API requires those blocks to be sent
    /// back unchanged when continuing the same tool-use conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_content: Vec<ProviderContentBlock>,
}

/// A tool call embedded in an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallMessage {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Provider-specific content that is not assistant-visible text or a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderContentBlock {
    pub provider: String,
    pub value: serde_json::Value,
}

/// Token usage from a provider response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}
