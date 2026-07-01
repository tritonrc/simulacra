use serde::{Deserialize, Serialize};
use simulacra_types::ToolDefinition;

/// Raw tool description received from an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpToolSchema {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(alias = "inputSchema")]
    pub(crate) input_schema: serde_json::Value,
}

/// Bridge an MCP tool schema to a Simulacra ToolDefinition.
pub(crate) fn bridge_tool_schema(schema: &McpToolSchema) -> ToolDefinition {
    ToolDefinition {
        name: schema.name.clone(),
        description: schema.description.clone(),
        input_schema: schema.input_schema.clone(),
    }
}
