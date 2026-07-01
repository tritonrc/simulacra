use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use simulacra_types::{CapabilityToken, ToolDefinition, ToolError};
use tokio::sync::Mutex;

use crate::manager::McpManager;

/// A wrapper that presents a single MCP server tool as a Simulacra `Tool`.
///
/// Each `McpTool` holds a shared reference to the `McpManager` (behind
/// `Arc<Mutex<..>>`) and the server name + tool definition needed to
/// route `call_tool` requests to the correct MCP server.
pub struct McpTool {
    manager: Arc<Mutex<McpManager>>,
    server_name: String,
    tool_def: ToolDefinition,
}

impl McpTool {
    pub fn new(
        manager: Arc<Mutex<McpManager>>,
        server_name: String,
        tool_def: ToolDefinition,
    ) -> Self {
        Self {
            manager,
            server_name,
            tool_def,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_def.name
    }
}

impl simulacra_types::Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.tool_def.clone()
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        let server = self.server_name.clone();
        let tool_name = self.tool_def.name.clone();
        let cap = capability.clone();
        Box::pin(async move {
            let mut manager = self.manager.lock().await;
            manager
                .call_tool(&server, &tool_name, arguments, &cap)
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))
        })
    }
}
