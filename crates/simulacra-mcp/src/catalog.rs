//! Per-agent, skill-activated MCP catalog surfaces (S057).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_types::{CapabilityToken, SkillDependencyActivator, Tool, ToolDefinition, ToolError};
use tokio::sync::Mutex;

use crate::McpManager;

#[derive(Clone, Debug)]
pub struct McpServerDescriptor {
    pub name: String,
    pub url: String,
    pub transport: Option<String>,
}

/// A catalog belongs to one agent/session. Descriptors are inert until a skill
/// activates them; inventories and search publications never cross sessions.
pub struct McpCatalog {
    descriptors: HashMap<String, McpServerDescriptor>,
    manager: Arc<Mutex<McpManager>>,
    state: Mutex<CatalogState>,
    activation_lock: Mutex<()>,
}

#[derive(Default)]
struct CatalogState {
    activated: BTreeMap<String, Vec<ToolDefinition>>,
    published: HashSet<(String, String)>,
}

impl McpCatalog {
    pub fn new(descriptors: Vec<McpServerDescriptor>) -> Result<Arc<Self>, ToolError> {
        let mut by_name = HashMap::new();
        for descriptor in descriptors {
            if descriptor.name.trim().is_empty() || descriptor.url.trim().is_empty() {
                return Err(ToolError::ExecutionFailed(
                    "configured MCP server requires a non-empty name and URL".into(),
                ));
            }
            if by_name
                .insert(descriptor.name.clone(), descriptor)
                .is_some()
            {
                return Err(ToolError::ExecutionFailed(
                    "duplicate configured MCP server name".into(),
                ));
            }
        }
        Ok(Arc::new(Self {
            descriptors: by_name,
            manager: Arc::new(Mutex::new(McpManager::new())),
            state: Mutex::new(CatalogState::default()),
            activation_lock: Mutex::new(()),
        }))
    }

    /// Validate a skill dependency set at bootstrap without opening a network
    /// connection. This keeps bad references out of an agent's catalog.
    pub fn validate_dependencies(
        &self,
        skill: &str,
        servers: &[String],
        capability: &CapabilityToken,
    ) -> Result<(), ToolError> {
        for server in servers {
            if !self.descriptors.contains_key(server) {
                return Err(ToolError::ExecutionFailed(format!(
                    "skill {skill:?} references unknown MCP server {server:?}"
                )));
            }
            if !capability_allows_server(capability, server) {
                return Err(ToolError::ExecutionFailed(format!(
                    "skill {skill:?} is not allowed to activate MCP server {server:?}"
                )));
            }
        }
        Ok(())
    }

    /// Activates all dependencies transactionally. The temporary inventory is
    /// only committed after every new server has completed its handshake.
    pub async fn activate(
        &self,
        skill: &str,
        servers: &[String],
        capability: &CapabilityToken,
    ) -> Result<usize, ToolError> {
        let _activation_guard = self.activation_lock.lock().await;
        let mut unique = Vec::new();
        for server in servers {
            if !unique.iter().any(|existing: &String| existing == server) {
                unique.push(server.clone());
            }
        }
        self.validate_dependencies(skill, &unique, capability)?;
        let already: BTreeSet<String> = self.state.lock().await.activated.keys().cloned().collect();
        let pending: Vec<_> = unique
            .into_iter()
            .filter(|server| !already.contains(server))
            .collect();
        if pending.is_empty() {
            return Ok(0);
        }

        let mut temporary = BTreeMap::new();
        let mut manager = self.manager.lock().await;
        for server in &pending {
            let descriptor = self.descriptors.get(server).ok_or_else(|| {
                ToolError::ExecutionFailed(format!(
                    "skill {skill:?} references unknown MCP server {server:?}"
                ))
            })?;
            manager.connect_named(&descriptor.name, &descriptor.url, descriptor.transport.as_deref()).await
                .map_err(|error| {
                    tracing::warn!(simulacra.skill.name = %skill, simulacra.mcp.activation.outcome = "failure", server = %server, error = %error, "MCP skill activation failed");
                    ToolError::ExecutionFailed(format!("skill {skill:?} could not activate MCP server {server:?}: {error}"))
                })?;
            let tools = manager.list_tools_for_server(server).await
                .map_err(|error| {
                    tracing::warn!(simulacra.skill.name = %skill, simulacra.mcp.activation.outcome = "failure", server = %server, error = %error, "MCP skill activation failed");
                    ToolError::ExecutionFailed(format!("skill {skill:?} could not activate MCP server {server:?}: {error}"))
                })?;
            temporary.insert(server.clone(), tools);
        }
        drop(manager);
        let count = temporary.values().map(Vec::len).sum();
        let mut state = self.state.lock().await;
        state.activated.extend(temporary);
        tracing::info!(simulacra.skill.name = %skill, simulacra.mcp.activated_tool_count = count, simulacra.mcp.activation.outcome = "success", "MCP skill activation");
        Ok(count)
    }

    async fn search(&self, query: &str) -> Vec<Value> {
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        let mut state = self.state.lock().await;
        for (server, tools) in &state.activated {
            for tool in tools {
                let haystack = format!("{} {}", tool.name, tool.description).to_lowercase();
                if needle.is_empty() || haystack.contains(&needle) {
                    matches.push((server.clone(), tool.clone()));
                }
            }
        }
        matches.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
        matches.truncate(5);
        for (server, tool) in &matches {
            state.published.insert((server.clone(), tool.name.clone()));
        }
        tracing::info!(simulacra.mcp.search.query = %query, simulacra.mcp.search.result_count = matches.len(), "MCP catalog search");
        matches.into_iter().map(|(server, tool)| json!({"server": server, "tool": tool.name, "description": tool.description, "input_schema": tool.input_schema})).collect()
    }

    async fn call(
        &self,
        server: String,
        tool: String,
        arguments: Value,
        capability: CapabilityToken,
    ) -> Result<Value, ToolError> {
        let published = self
            .state
            .lock()
            .await
            .published
            .contains(&(server.clone(), tool.clone()));
        if !published {
            return Err(ToolError::ExecutionFailed(format!(
                "MCP tool {server}:{tool} is not activated and search-published for this session"
            )));
        }
        self.manager
            .lock()
            .await
            .call_tool(&server, &tool, arguments, &capability)
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))
    }
}

impl SkillDependencyActivator for McpCatalog {
    fn activate(
        &self,
        skill: String,
        mcp_servers: Vec<String>,
        capability: CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send + '_>> {
        Box::pin(async move {
            self.activate(&skill, &mcp_servers, &capability)
                .await
                .map(|_| ())
        })
    }
}

fn capability_allows_server(capability: &CapabilityToken, server: &str) -> bool {
    capability.mcp_tools.iter().any(|pattern| {
        let mut parts = pattern.split(':');
        matches!((parts.next(), parts.next(), parts.next()), (Some("mcp"), Some(name), Some(_)) if name == server || name == "*")
    })
}

pub struct McpSearchTool {
    catalog: Arc<McpCatalog>,
}
impl McpSearchTool {
    pub fn new(catalog: Arc<McpCatalog>) -> Self {
        Self { catalog }
    }
}
impl Tool for McpSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "mcp_search".into(),
            description: "Search tools from MCP servers activated by loaded skills.".into(),
            input_schema: json!({"type":"object","properties":{"query":{"type":"string","description":"Terms used to rank activated MCP tools"}},"required":["query"],"additionalProperties":false}),
        }
    }
    fn call(
        &self,
        arguments: Value,
        _: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Box::pin(async move {
            let query = query.ok_or_else(|| {
                ToolError::InvalidArguments("mcp_search requires string query".into())
            })?;
            Ok(Value::Array(self.catalog.search(&query).await))
        })
    }
}

pub struct McpCallTool {
    catalog: Arc<McpCatalog>,
}
impl McpCallTool {
    pub fn new(catalog: Arc<McpCatalog>) -> Self {
        Self { catalog }
    }
}
impl Tool for McpCallTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "mcp_call".into(),
            description: "Call a search-published tool from an activated MCP server.".into(),
            input_schema: json!({"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"arguments":{}},"required":["server","tool","arguments"],"additionalProperties":false}),
        }
    }
    fn call(
        &self,
        arguments: Value,
        capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        let server = arguments
            .get("server")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool = arguments
            .get("tool")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let input = arguments.get("arguments").cloned();
        let capability = capability.clone();
        Box::pin(async move {
            self.catalog
                .call(
                    server.ok_or_else(|| {
                        ToolError::InvalidArguments("mcp_call requires string server".into())
                    })?,
                    tool.ok_or_else(|| {
                        ToolError::InvalidArguments("mcp_call requires string tool".into())
                    })?,
                    input.ok_or_else(|| {
                        ToolError::InvalidArguments("mcp_call requires arguments".into())
                    })?,
                    capability,
                )
                .await
        })
    }
}
