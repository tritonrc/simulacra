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
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::TokenUsage;
    use serde_json::json;

    #[test]
    fn token_usage_cache_counters_default_roundtrip_and_do_not_affect_total() {
        let legacy: TokenUsage = serde_json::from_value(json!({
            "input_tokens": 100,
            "output_tokens": 25
        }))
        .expect("legacy TokenUsage JSON should remain readable");

        assert_eq!(legacy.total(), 125);
        let legacy_json = serde_json::to_value(&legacy).expect("TokenUsage should serialize");
        assert_eq!(
            legacy_json["cache_read_input_tokens"], 0,
            "legacy TokenUsage values must default missing cache reads to zero"
        );
        assert_eq!(
            legacy_json["cache_write_input_tokens"], 0,
            "legacy TokenUsage values must default missing cache writes to zero"
        );

        let with_cache: TokenUsage = serde_json::from_value(json!({
            "input_tokens": 100,
            "output_tokens": 25,
            "cache_read_input_tokens": 40,
            "cache_write_input_tokens": 15
        }))
        .expect("S059 TokenUsage JSON should deserialize");

        assert_eq!(
            with_cache.total(),
            125,
            "cache counters are subsets of input_tokens and must not be double-counted"
        );
        let with_cache_json =
            serde_json::to_value(&with_cache).expect("TokenUsage should serialize");
        assert_eq!(
            with_cache_json["cache_read_input_tokens"], 40,
            "cache reads must round-trip when present"
        );
        assert_eq!(
            with_cache_json["cache_write_input_tokens"], 15,
            "cache writes must round-trip when present"
        );
    }
}
