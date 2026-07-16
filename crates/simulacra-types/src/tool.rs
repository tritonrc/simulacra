use crate::CapabilityToken;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const TOOL_PREVIEW_MAX_CHARS: usize = 4096;

/// Schema definition for a tool that can be offered to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Small JSON Schema helper surface for tool definitions.
///
/// These builders intentionally return `serde_json::Value` so provider adapters
/// can keep using the existing `ToolDefinition` wire shape.
pub struct ToolSchema;

impl ToolSchema {
    pub fn object<N, R, I, J>(properties: I, required: J) -> Value
    where
        N: Into<String>,
        R: Into<String>,
        I: IntoIterator<Item = (N, Value)>,
        J: IntoIterator<Item = R>,
    {
        let mut property_map = Map::new();
        for (name, schema) in properties {
            property_map.insert(name.into(), schema);
        }
        serde_json::json!({
            "type": "object",
            "properties": property_map,
            "required": required.into_iter().map(Into::into).collect::<Vec<String>>(),
            "additionalProperties": false
        })
    }

    pub fn string(description: impl Into<String>) -> Value {
        described_schema("string", description.into())
    }

    pub fn number(description: impl Into<String>) -> Value {
        described_schema("number", description.into())
    }

    pub fn integer(description: impl Into<String>) -> Value {
        described_schema("integer", description.into())
    }

    pub fn boolean(description: impl Into<String>) -> Value {
        described_schema("boolean", description.into())
    }
}

fn described_schema(schema_type: &str, description: String) -> Value {
    serde_json::json!({
        "type": schema_type,
        "description": description
    })
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

/// Typed output contract for tool execution.
///
/// `content` is the provider/model-visible text. `is_error` is authoritative:
/// callers must not infer error state from arbitrary structured fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub log_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_output: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            log_preview: preview(&content),
            content,
            is_error: false,
            structured: None,
            hook_input: None,
            hook_output: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            log_preview: preview(&content),
            content,
            is_error: true,
            structured: None,
            hook_input: None,
            hook_output: None,
        }
    }

    pub fn with_structured(mut self, structured: serde_json::Value) -> Self {
        self.structured = Some(structured);
        self
    }

    pub fn with_hook_input(mut self, hook_input: serde_json::Value) -> Self {
        self.hook_input = Some(hook_input);
        self
    }

    pub fn with_hook_output(mut self, hook_output: serde_json::Value) -> Self {
        self.hook_output = Some(hook_output);
        self
    }

    /// Convert legacy/raw tool values into the typed contract.
    ///
    /// Objects with both `content` and `is_error` are treated as explicit typed
    /// payloads. Other values are successful legacy values; an `error` field by
    /// itself is preserved as structured data but does not set `is_error`.
    pub fn from_value(value: serde_json::Value) -> Self {
        if let serde_json::Value::Object(map) = &value
            && let (Some(content), Some(is_error)) = (
                map.get("content").and_then(serde_json::Value::as_str),
                map.get("is_error").and_then(serde_json::Value::as_bool),
            )
        {
            let log_preview = map
                .get("log_preview")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| preview(content));
            return Self {
                content: content.to_string(),
                is_error,
                log_preview,
                structured: map.get("structured").cloned(),
                hook_input: map.get("hook_input").cloned(),
                hook_output: map.get("hook_output").cloned(),
            };
        }

        match value {
            serde_json::Value::String(content) => Self::success(content),
            other => Self::success(other.to_string()).with_structured(other),
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "content": self.content,
            "is_error": self.is_error,
            "log_preview": self.log_preview,
        });
        if let serde_json::Value::Object(ref mut map) = value {
            if let Some(structured) = &self.structured {
                map.insert("structured".into(), structured.clone());
            }
            if let Some(hook_input) = &self.hook_input {
                map.insert("hook_input".into(), hook_input.clone());
            }
            if let Some(hook_output) = &self.hook_output {
                map.insert("hook_output".into(), hook_output.clone());
            }
        }
        value
    }
}

fn preview(content: &str) -> String {
    truncate_chars(content, TOOL_PREVIEW_MAX_CHARS).0
}

pub fn truncate_chars(content: &str, max_chars: usize) -> (String, bool) {
    if content.chars().count() <= max_chars {
        return (content.to_string(), false);
    }
    (content.chars().take(max_chars).collect(), true)
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

    /// Optional structured output schema for internal metadata/catalog users.
    fn output_schema(&self) -> Option<serde_json::Value> {
        None
    }

    /// Whether the runtime may safely execute this tool in parallel with other
    /// tool calls. Defaults to conservative serial execution.
    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    /// Whether runtime cancellation should wait for this tool to finish its own
    /// cleanup. Defaults to no special wait behavior.
    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }

    /// Stable payload used by the generic pre-tool hook.
    fn hook_input_payload(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "tool": tool_name,
            "arguments": arguments,
        })
    }

    /// Extract possibly rewritten arguments from a modified pre-hook payload.
    fn arguments_from_hook_input(
        &self,
        original: serde_json::Value,
        payload: &serde_json::Value,
    ) -> serde_json::Value {
        payload.get("arguments").cloned().unwrap_or(original)
    }

    /// Stable payload used by the generic post-tool hook.
    fn hook_output_payload(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        result: &serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "tool": tool_name,
            "arguments": arguments,
            "result": result,
        })
    }

    /// Extract possibly rewritten result from a modified post-hook payload.
    fn result_from_hook_output(
        &self,
        original: serde_json::Value,
        payload: &serde_json::Value,
    ) -> serde_json::Value {
        payload.get("result").cloned().unwrap_or(original)
    }

    /// Convert this tool's raw result value into the typed output contract.
    fn output_from_value(&self, value: serde_json::Value) -> ToolOutput {
        ToolOutput::from_value(value)
    }
}

/// Host-side dependency activation invoked by the `Skill` tool before its body
/// becomes visible. Implementations own any network-backed catalog state.
pub trait SkillDependencyActivator: Send + Sync + 'static {
    fn activate(
        &self,
        skill: String,
        mcp_servers: Vec<String>,
        capability: CapabilityToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + '_>>;
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
