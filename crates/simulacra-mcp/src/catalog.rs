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
    pub kind: McpServerKind,
}

#[derive(Clone)]
pub enum McpServerKind {
    Network {
        url: String,
        transport: Option<String>,
    },
    Wasm(WasmMcpServerDescriptor),
}

impl std::fmt::Debug for McpServerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network { transport, .. } => f
                .debug_struct("Network")
                .field("transport", transport)
                .finish(),
            Self::Wasm(descriptor) => f.debug_tuple("Wasm").field(descriptor).finish(),
        }
    }
}

#[derive(Clone)]
pub struct WasmMcpServerDescriptor {
    pub module_path: std::path::PathBuf,
    pub network_allowlist: Vec<String>,
    pub hooks: Option<Arc<simulacra_hooks::HookPipeline>>,
    pub journal: Option<Arc<dyn simulacra_types::JournalStorage>>,
    pub agent_id: simulacra_types::AgentId,
}

impl std::fmt::Debug for WasmMcpServerDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmMcpServerDescriptor")
            .field("module_path", &self.module_path)
            .field("network_allowlist", &self.network_allowlist)
            .finish_non_exhaustive()
    }
}

impl McpServerDescriptor {
    pub fn network(name: String, url: String, transport: Option<String>) -> Self {
        Self {
            name,
            kind: McpServerKind::Network { url, transport },
        }
    }
    pub fn wasm(name: String, descriptor: WasmMcpServerDescriptor) -> Self {
        Self {
            name,
            kind: McpServerKind::Wasm(descriptor),
        }
    }
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
        Self::build(descriptors, McpManager::new())
    }

    /// Construct a session catalog with its audit sink and agent attribution.
    /// Activation and search bookkeeping use this journal before any network
    /// side effect or publication becomes visible.
    pub fn with_journal(
        descriptors: Vec<McpServerDescriptor>,
        journal: Arc<dyn simulacra_types::JournalStorage>,
        agent_id: simulacra_types::AgentId,
    ) -> Result<Arc<Self>, ToolError> {
        Self::build(descriptors, McpManager::with_journal(journal, agent_id))
    }

    fn build(
        descriptors: Vec<McpServerDescriptor>,
        manager: McpManager,
    ) -> Result<Arc<Self>, ToolError> {
        let mut by_name = HashMap::new();
        for descriptor in descriptors {
            if descriptor.name.trim().is_empty() {
                return Err(ToolError::ExecutionFailed(
                    "configured MCP server requires a non-empty name".into(),
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
            manager: Arc::new(Mutex::new(manager)),
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
        let declared_servers = unique.clone();
        let mut manager = self.manager.lock().await;
        if let Err(error) = manager.append_journal_tool_call(
            "mcp_activation",
            &json!({"skill": skill, "servers": declared_servers}),
        ) {
            emit_activation_telemetry(skill, &declared_servers, 0, "failure");
            tracing::warn!(
                simulacra.skill.name = %skill,
                simulacra.mcp.activation.outcome = "failure",
                stage = "audit",
                "MCP skill activation failed"
            );
            let _ = error;
            return Err(ToolError::ExecutionFailed(format!(
                "skill {skill:?} MCP activation could not be audited"
            )));
        }
        if let Err(error) = self.validate_dependencies(skill, &unique, capability) {
            emit_activation_telemetry(skill, &declared_servers, 0, "failure");
            return Err(error);
        }
        let already: BTreeSet<String> = self.state.lock().await.activated.keys().cloned().collect();
        let pending: Vec<_> = unique
            .into_iter()
            .filter(|server| !already.contains(server))
            .collect();
        if pending.is_empty() {
            emit_activation_telemetry(skill, &declared_servers, 0, "success");
            return Ok(0);
        }

        let mut temporary = BTreeMap::new();
        let result: Result<(), ToolError> = async {
            for server in &pending {
                let descriptor = self.descriptors.get(server).ok_or_else(|| {
                    ToolError::ExecutionFailed(format!(
                        "skill {skill:?} references unknown MCP server {server:?}"
                    ))
                })?;
                match &descriptor.kind {
                    McpServerKind::Network { url, transport } => {
                        manager
                            .connect_named(&descriptor.name, url, transport.as_deref())
                            .await
                    }
                    McpServerKind::Wasm(descriptor) => {
                        let mut module = crate::load_wasm_mcp_module(&descriptor.module_path)
                            .map_err(|_| {
                                sanitized_activation_error(skill, server, "module_load")
                            })?;
                        module = module
                            .with_network_allowlist(descriptor.network_allowlist.clone())
                            .with_agent_id(descriptor.agent_id.clone());
                        if let Some(hooks) = &descriptor.hooks {
                            module = module.with_hooks(Arc::clone(hooks));
                        }
                        if let Some(journal) = &descriptor.journal {
                            module = module.with_journal(Arc::clone(journal));
                        }
                        manager.connect_wasm_module(server, module).await
                    }
                }
                .map_err(|_| sanitized_activation_error(skill, server, "connect"))?;
                let tools = manager
                    .list_tools_for_server(server)
                    .await
                    .map_err(|_| sanitized_activation_error(skill, server, "inventory"))?;
                temporary.insert(server.clone(), tools);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            for server in &pending {
                manager.discard_server(server);
            }
            emit_activation_telemetry(skill, &declared_servers, 0, "failure");
            return Err(error);
        }
        drop(manager);
        let count = temporary.values().map(Vec::len).sum();
        let mut state = self.state.lock().await;
        state.activated.extend(temporary);
        emit_activation_telemetry(skill, &declared_servers, count, "success");
        Ok(count)
    }

    async fn search(&self, query: &str) -> Result<Vec<Value>, ToolError> {
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        let state = self.state.lock().await;
        for (server, tools) in &state.activated {
            for tool in tools {
                let haystack = format!("{} {}", tool.name, tool.description).to_lowercase();
                if needle.is_empty() || haystack.contains(&needle) {
                    matches.push((server.clone(), tool.clone()));
                }
            }
        }
        drop(state);
        matches.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
        matches.truncate(5);
        self.manager
            .lock()
            .await
            .append_journal_tool_call("mcp_search", &json!({"query": query}))
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let mut state = self.state.lock().await;
        for (server, tool) in &matches {
            state.published.insert((server.clone(), tool.name.clone()));
        }
        tracing::info!(simulacra.mcp.search.query = %query, simulacra.mcp.search.result_count = matches.len(), "MCP catalog search");
        Ok(matches.into_iter().map(|(server, tool)| json!({"server": server, "tool": tool.name, "description": tool.description, "input_schema": tool.input_schema})).collect())
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

fn emit_activation_telemetry(skill: &str, servers: &[String], tool_count: usize, outcome: &str) {
    let server_set = serde_json::to_string(servers).unwrap_or_else(|_| "[]".to_string());
    tracing::info!(
        simulacra.skill.name = %skill,
        simulacra.mcp.servers = %server_set,
        simulacra.mcp.activated_tool_count = tool_count,
        simulacra.mcp.activation.outcome = %outcome,
        "MCP skill activation"
    );
}

fn sanitized_activation_error(skill: &str, server: &str, stage: &str) -> ToolError {
    tracing::warn!(
        simulacra.skill.name = %skill,
        simulacra.mcp.activation.outcome = "failure",
        server = %server,
        stage = %stage,
        "MCP skill activation failed"
    );
    ToolError::ExecutionFailed(format!(
        "skill {skill:?} could not activate MCP server {server:?} during {stage}"
    ))
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
            Ok(Value::Array(self.catalog.search(&query).await?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use simulacra_types::{
        AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
        JournalError, JournalStorage, TokenUsage,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default)]
    struct RecordingJournal(Mutex<Vec<JournalEntry>>);

    struct SecretFailingJournal;

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        fields: HashMap<String, String>,
        current_span: Option<String>,
    }

    struct EventCapture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for EventCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.events
                .lock()
                .expect("event capture mutex")
                .push(CapturedEvent {
                    fields,
                    current_span: ctx.lookup_current().map(|span| span.name().to_string()),
                });
        }
    }

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    fn activation_events(events: &Arc<Mutex<Vec<CapturedEvent>>>) -> Vec<CapturedEvent> {
        events
            .lock()
            .expect("event capture mutex")
            .iter()
            .filter(|event| {
                event
                    .fields
                    .contains_key("simulacra.mcp.activated_tool_count")
            })
            .cloned()
            .collect()
    }

    fn assert_activation_event(
        event: &CapturedEvent,
        skill: &str,
        servers: &str,
        tool_count: &str,
        outcome: &str,
    ) {
        assert_eq!(
            event.fields.get("simulacra.skill.name"),
            Some(&skill.into())
        );
        assert_eq!(
            event.fields.get("simulacra.mcp.servers"),
            Some(&servers.into())
        );
        assert_eq!(
            event.fields.get("simulacra.mcp.activated_tool_count"),
            Some(&tool_count.into())
        );
        assert_eq!(
            event.fields.get("simulacra.mcp.activation.outcome"),
            Some(&outcome.into())
        );
    }

    impl JournalStorage for RecordingJournal {
        fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
            self.0.lock().expect("journal mutex").push(entry);
            Ok(())
        }

        fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(self
                .0
                .lock()
                .expect("journal mutex")
                .iter()
                .filter(|entry| entry.agent_id == *agent_id)
                .cloned()
                .collect())
        }

        fn query_token_usage(&self, _: &AgentId) -> Result<TokenUsage, JournalError> {
            Ok(TokenUsage::default())
        }

        fn save_checkpoint(
            &self,
            _: &AgentId,
            _: usize,
            _: CheckpointData,
        ) -> Result<(), JournalError> {
            Ok(())
        }

        fn fork_from(
            &self,
            agent_id: &AgentId,
            _: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            self.read_all(agent_id)
        }

        fn read_from(
            &self,
            agent_id: &AgentId,
            _: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            self.read_all(agent_id)
        }
    }

    impl JournalStorage for SecretFailingJournal {
        fn append(&self, _: JournalEntry) -> Result<(), JournalError> {
            Err(JournalError::Storage(
                "https://JOURNALUSER:JOURNALPASS@example.invalid/a?token=JOURNALQUERY Authorization: Bearer JOURNALAUTH /private/JOURNALMODULE.wasm".into(),
            ))
        }

        fn read_all(&self, _: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(Vec::new())
        }
        fn query_token_usage(&self, _: &AgentId) -> Result<TokenUsage, JournalError> {
            Ok(TokenUsage::default())
        }
        fn save_checkpoint(
            &self,
            _: &AgentId,
            _: usize,
            _: CheckpointData,
        ) -> Result<(), JournalError> {
            Ok(())
        }
        fn fork_from(&self, _: &AgentId, _: usize) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(Vec::new())
        }
        fn read_from(&self, _: &AgentId, _: usize) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(Vec::new())
        }
    }

    struct JsonRpcServer {
        url: String,
        initialize_requests: Arc<AtomicUsize>,
        tools_list_requests: Arc<AtomicUsize>,
        tool_call_requests: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl JsonRpcServer {
        async fn new(tool_name: &'static str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test MCP server should bind");
            let url = format!(
                "http://{}",
                listener.local_addr().expect("test server address")
            );
            let initialize_requests = Arc::new(AtomicUsize::new(0));
            let tools_list_requests = Arc::new(AtomicUsize::new(0));
            let tool_call_requests = Arc::new(AtomicUsize::new(0));
            let initialize_for_thread = Arc::clone(&initialize_requests);
            let list_for_thread = Arc::clone(&tools_list_requests);
            let calls_for_thread = Arc::clone(&tool_call_requests);
            let task = tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let initialize_requests = Arc::clone(&initialize_for_thread);
                    let tools_list_requests = Arc::clone(&list_for_thread);
                    let tool_call_requests = Arc::clone(&calls_for_thread);
                    tokio::spawn(async move {
                        let Some(request) = read_json_rpc_request(&mut stream).await else {
                            return;
                        };
                        let body = if request.contains("\"method\":\"initialize\"") {
                            initialize_requests.fetch_add(1, Ordering::SeqCst);
                            json!({"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"test","version":"1"},"capabilities":{}}}).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_requests.fetch_add(1, Ordering::SeqCst);
                            json!({"jsonrpc":"2.0","result":{"tools":[{"name":tool_name,"description":"A catalog test tool","inputSchema":{"type":"object"}}]}}).to_string()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_requests.fetch_add(1, Ordering::SeqCst);
                            if tool_name == "secret_error_tool" {
                                json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"https://REMOTEUSER:REMOTEPASS@example.invalid/mcp?token=REMOTEQUERY Authorization: Bearer REMOTEAUTH /private/REMOTEMODULE.wasm"}}).to_string()
                            } else {
                                json!({"jsonrpc":"2.0","result":{"ok":true}}).to_string()
                            }
                        } else {
                            json!({"jsonrpc":"2.0","result":{}}).to_string()
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Self {
                url,
                initialize_requests,
                tools_list_requests,
                tool_call_requests,
                task,
            }
        }
    }

    impl Drop for JsonRpcServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn mcp_capability() -> CapabilityToken {
        CapabilityToken {
            mcp_tools: vec!["mcp:*:*".into()],
            ..Default::default()
        }
    }

    async fn catalog_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    async fn read_json_rpc_request(stream: &mut tokio::net::TcpStream) -> Option<String> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut expected_len = None;
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    request.extend_from_slice(&buffer[..read]);
                    if expected_len.is_none()
                        && let Some(end) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = end + 4;
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let body_len = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        expected_len = Some(header_end + body_len);
                    }
                    if expected_len.is_some_and(|length| request.len() >= length) {
                        break;
                    }
                }
                Err(_) => return None,
            }
        }
        (!request.is_empty()).then(|| String::from_utf8_lossy(&request).into_owned())
    }

    fn echo_descriptor(name: &str, agent: &str) -> McpServerDescriptor {
        McpServerDescriptor::wasm(
            name.into(),
            WasmMcpServerDescriptor {
                module_path: std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/echo-mcp.wasm"
                )),
                network_allowlist: Vec::new(),
                hooks: None,
                journal: None,
                agent_id: AgentId(agent.into()),
            },
        )
    }

    #[tokio::test]
    async fn meta_tool_definitions_are_byte_stable_across_catalog_changes() {
        let _guard = catalog_test_guard().await;
        let catalog = McpCatalog::new(vec![echo_descriptor("github", "schema-agent")])
            .expect("catalog should construct");
        let search = McpSearchTool::new(Arc::clone(&catalog));
        let call = McpCallTool::new(Arc::clone(&catalog));
        let before = serde_json::to_vec(&(search.definition(), call.definition())).unwrap();
        catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");
        search
            .call(json!({"query":"echo"}), &mcp_capability())
            .await
            .expect("search should succeed");
        let after_success = serde_json::to_vec(&(search.definition(), call.definition())).unwrap();
        catalog
            .activate("broken", &["unknown".into()], &mcp_capability())
            .await
            .expect_err("unknown dependency should fail");
        let after_failure = serde_json::to_vec(&(search.definition(), call.definition())).unwrap();
        assert_eq!(before, after_success);
        assert_eq!(before, after_failure);
    }

    #[tokio::test]
    async fn search_publishes_only_the_bounded_five_returned_pairs() {
        let catalog = McpCatalog::new(Vec::new()).expect("catalog should construct");
        catalog.state.lock().await.activated.insert(
            "github".into(),
            (0..6)
                .map(|index| ToolDefinition {
                    name: format!("tool_{index}"),
                    description: "bounded fixture".into(),
                    input_schema: json!({"type":"object"}),
                })
                .collect(),
        );
        let search = McpSearchTool::new(Arc::clone(&catalog));
        let call = McpCallTool::new(Arc::clone(&catalog));
        let results = search
            .call(json!({"query":""}), &mcp_capability())
            .await
            .expect("search should succeed");
        assert_eq!(results.as_array().unwrap().len(), 5);
        let returned_error = call
            .call(
                json!({"server":"github","tool":"tool_0","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect_err("fixture has no dispatcher");
        assert!(
            !returned_error
                .to_string()
                .contains("not activated and search-published")
        );
        let omitted_error = call
            .call(
                json!({"server":"github","tool":"tool_5","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect_err("omitted sixth tool must be rejected");
        assert!(
            omitted_error
                .to_string()
                .contains("not activated and search-published")
        );
        assert!(catalog.manager.lock().await.connections.is_empty());
    }

    #[tokio::test]
    async fn capability_denied_search_published_call_is_actionable_and_never_dispatched() {
        let _guard = catalog_test_guard().await;
        let server = JsonRpcServer::new("issues").await;
        let catalog = McpCatalog::new(vec![McpServerDescriptor::network(
            "github".into(),
            server.url.clone(),
            Some("http".into()),
        )])
        .expect("catalog should construct");
        catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");
        McpSearchTool::new(Arc::clone(&catalog))
            .call(json!({"query":"issues"}), &mcp_capability())
            .await
            .expect("search should publish the tool");

        let error = McpCallTool::new(catalog)
            .call(
                json!({"server":"github","tool":"issues","arguments":{}}),
                &CapabilityToken::default(),
            )
            .await
            .expect_err("current call capability must be enforced after publication");

        let message = error.to_string();
        assert!(message.contains("github") && message.contains("issues"));
        assert!(message.contains("not in granted mcp_tools"));
        assert_eq!(server.tool_call_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_later_activation_preserves_earlier_inventory_and_publication() {
        let _guard = catalog_test_guard().await;
        let unavailable = TcpListener::bind("127.0.0.1:0").expect("reserve unavailable port");
        let unavailable_url = format!("http://{}", unavailable.local_addr().unwrap());
        drop(unavailable);
        let catalog = McpCatalog::new(vec![
            echo_descriptor("github", "preserve-agent"),
            echo_descriptor("linear", "preserve-agent"),
            McpServerDescriptor::network(
                "unavailable".into(),
                unavailable_url,
                Some("http".into()),
            ),
        ])
        .expect("catalog should construct");
        let search = McpSearchTool::new(Arc::clone(&catalog));
        let call = McpCallTool::new(Arc::clone(&catalog));
        catalog
            .activate("first", &["github".into()], &mcp_capability())
            .await
            .unwrap();
        search
            .call(json!({"query":"echo"}), &mcp_capability())
            .await
            .unwrap();
        catalog
            .activate(
                "later",
                &["linear".into(), "unavailable".into()],
                &mcp_capability(),
            )
            .await
            .expect_err("later activation should fail atomically");
        assert_eq!(
            search
                .call(json!({"query":"echo"}), &mcp_capability())
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        call.call(
            json!({"server":"github","tool":"echo","arguments":{"query":"still-live"}}),
            &mcp_capability(),
        )
        .await
        .expect("earlier publication should remain callable");
    }

    #[tokio::test]
    async fn separate_catalogs_isolate_activation_and_publication() {
        let _guard = catalog_test_guard().await;
        let first = McpCatalog::new(vec![echo_descriptor("github", "first-agent")]).unwrap();
        let second = McpCatalog::new(vec![echo_descriptor("github", "second-agent")]).unwrap();
        let first_search = McpSearchTool::new(Arc::clone(&first));
        let second_search = McpSearchTool::new(Arc::clone(&second));
        let second_call = McpCallTool::new(Arc::clone(&second));
        first
            .activate("skill", &["github".into()], &mcp_capability())
            .await
            .unwrap();
        first_search
            .call(json!({"query":"echo"}), &mcp_capability())
            .await
            .unwrap();
        assert!(
            second_search
                .call(json!({"query":"echo"}), &mcp_capability())
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        second
            .activate("skill", &["github".into()], &mcp_capability())
            .await
            .unwrap();
        let error = second_call
            .call(
                json!({"server":"github","tool":"echo","arguments":{"query":"isolated"}}),
                &mcp_capability(),
            )
            .await
            .expect_err("first publication must not authorize second catalog");
        assert!(
            error
                .to_string()
                .contains("not activated and search-published")
        );
    }

    #[tokio::test]
    async fn activation_inventories_exactly_two_declared_servers_and_never_touches_configured_third()
     {
        let _guard = catalog_test_guard().await;
        let github = JsonRpcServer::new("github_issues").await;
        let linear = JsonRpcServer::new("linear_issues").await;
        let dormant = JsonRpcServer::new("dormant_tool").await;
        let catalog = McpCatalog::new(vec![
            McpServerDescriptor::network("github".into(), github.url.clone(), Some("http".into())),
            McpServerDescriptor::network("linear".into(), linear.url.clone(), Some("http".into())),
            McpServerDescriptor::network(
                "dormant".into(),
                dormant.url.clone(),
                Some("http".into()),
            ),
        ])
        .expect("catalog should construct");

        assert_eq!(
            catalog
                .activate(
                    "repo-work",
                    &["github".into(), "linear".into()],
                    &mcp_capability(),
                )
                .await
                .expect("declared dependencies should activate"),
            2
        );

        for server in [&github, &linear] {
            assert_eq!(server.initialize_requests.load(Ordering::SeqCst), 1);
            assert_eq!(server.tools_list_requests.load(Ordering::SeqCst), 1);
        }
        assert_eq!(dormant.initialize_requests.load(Ordering::SeqCst), 0);
        assert_eq!(dormant.tools_list_requests.load(Ordering::SeqCst), 0);
        assert_eq!(dormant.tool_call_requests.load(Ordering::SeqCst), 0);

        let results = McpSearchTool::new(catalog)
            .call(json!({"query":""}), &mcp_capability())
            .await
            .expect("search should expose only activated inventories");
        let servers = results
            .as_array()
            .expect("search result array")
            .iter()
            .map(|result| result["server"].as_str().expect("server name"))
            .collect::<BTreeSet<_>>();
        assert_eq!(servers, BTreeSet::from(["github", "linear"]));
    }

    #[tokio::test]
    async fn concurrent_agent_catalogs_remain_isolated_and_reactivation_reuses_each_inventory_once()
    {
        let _guard = catalog_test_guard().await;
        let first_server = JsonRpcServer::new("first_tool").await;
        let second_server = JsonRpcServer::new("second_tool").await;
        let first = McpCatalog::new(vec![McpServerDescriptor::network(
            "shared-name".into(),
            first_server.url.clone(),
            Some("http".into()),
        )])
        .expect("first catalog should construct");
        let second = McpCatalog::new(vec![McpServerDescriptor::network(
            "shared-name".into(),
            second_server.url.clone(),
            Some("http".into()),
        )])
        .expect("second catalog should construct");

        let dependencies = vec!["shared-name".into()];
        let capability = mcp_capability();
        let (first_activation, second_activation) = tokio::join!(
            first.activate("first-skill", &dependencies, &capability),
            second.activate("second-skill", &dependencies, &capability),
        );
        assert_eq!(first_activation.expect("first activation"), 1);
        assert_eq!(second_activation.expect("second activation"), 1);

        let first_results = McpSearchTool::new(Arc::clone(&first))
            .call(json!({"query":""}), &mcp_capability())
            .await
            .expect("first search");
        assert_eq!(first_results[0]["tool"], "first_tool");
        let unpublished_second_error = McpCallTool::new(Arc::clone(&second))
            .call(
                json!({"server":"shared-name","tool":"second_tool","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect_err("first catalog publication must not authorize second catalog");
        assert!(
            unpublished_second_error
                .to_string()
                .contains("not activated and search-published")
        );

        let (first_cached, second_cached) = tokio::join!(
            first.activate("first-skill", &dependencies, &capability),
            second.activate("second-skill", &dependencies, &capability),
        );
        assert_eq!(first_cached.expect("first cached activation"), 0);
        assert_eq!(second_cached.expect("second cached activation"), 0);
        for server in [&first_server, &second_server] {
            assert_eq!(server.initialize_requests.load(Ordering::SeqCst), 1);
            assert_eq!(server.tools_list_requests.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            McpSearchTool::new(first)
                .call(json!({"query":""}), &mcp_capability())
                .await
                .expect("repeat first search")
                .as_array()
                .expect("search result array")
                .len(),
            1,
            "cached activation must not duplicate indexed tools"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn activation_telemetry_covers_prevalidation_and_connection_failures_without_secrets() {
        let _guard = catalog_test_guard().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let unknown = McpCatalog::new(Vec::new()).expect("empty catalog should construct");
        unknown
            .activate("preflight-skill", &["github".into()], &mcp_capability())
            .await
            .expect_err("unknown dependency should fail prevalidation");

        let secret_endpoint =
            "http://catalog-user:SUPERSECRET@127.0.0.1:not-a-port/mcp?credential=QUERYSECRET";
        let disconnected = McpCatalog::new(vec![McpServerDescriptor::network(
            "linear".into(),
            secret_endpoint.into(),
            Some("http".into()),
        )])
        .expect("catalog should construct");
        let activation_error = disconnected
            .activate("connection-skill", &["linear".into()], &mcp_capability())
            .await
            .expect_err("unavailable dependency should fail activation");
        let activation_error = activation_error.to_string();
        assert!(
            !activation_error.contains("SUPERSECRET")
                && !activation_error.contains("QUERYSECRET")
                && !activation_error.contains(secret_endpoint),
            "activation errors must redact endpoint credentials and URLs, got: {activation_error}"
        );

        let captured = activation_events(&events);
        assert_eq!(
            captured.len(),
            2,
            "one summary event per attempt: {captured:?}"
        );
        assert_activation_event(
            &captured[0],
            "preflight-skill",
            "[\"github\"]",
            "0",
            "failure",
        );
        assert_activation_event(
            &captured[1],
            "connection-skill",
            "[\"linear\"]",
            "0",
            "failure",
        );
        let rendered = format!("{:?}", events.lock().expect("event capture mutex"));
        assert!(
            !rendered.contains("SUPERSECRET")
                && !rendered.contains("QUERYSECRET")
                && !rendered.contains(secret_endpoint),
            "activation logs must redact endpoint credentials and URLs, got: {rendered}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn activation_telemetry_reports_committed_and_cached_success_with_declared_set() {
        let _guard = catalog_test_guard().await;
        let secret_endpoint = "https://user:SUPERSECRET@example.invalid/mcp".to_string();
        let catalog = McpCatalog::new(vec![McpServerDescriptor::wasm(
            "github".into(),
            WasmMcpServerDescriptor {
                module_path: std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/echo-mcp.wasm"
                )),
                network_allowlist: vec![secret_endpoint.clone()],
                hooks: None,
                journal: None,
                agent_id: AgentId("telemetry-agent".into()),
            },
        )])
        .expect("catalog should construct");
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        assert_eq!(
            catalog
                .activate("repo-work", &["github".into()], &mcp_capability())
                .await
                .expect("initial activation should commit inventory"),
            1
        );
        assert_eq!(
            catalog
                .activate("repo-work", &["github".into()], &mcp_capability())
                .await
                .expect("repeat activation should use cached inventory"),
            0
        );

        let captured = activation_events(&events);
        assert_eq!(
            captured.len(),
            2,
            "one summary event per attempt: {captured:?}"
        );
        assert_activation_event(&captured[0], "repo-work", "[\"github\"]", "1", "success");
        assert_activation_event(&captured[1], "repo-work", "[\"github\"]", "0", "success");
        let rendered = format!("{captured:?}");
        assert!(!rendered.contains(&secret_endpoint));
        assert!(!rendered.contains("SUPERSECRET"));
        assert!(!rendered.to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_skill_activation_telemetry_is_attributed_to_triggering_tool_span() {
        let _guard = catalog_test_guard().await;
        let catalog = McpCatalog::new(vec![echo_descriptor("github", "model-agent")])
            .expect("catalog should construct");
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let tool_span = tracing::info_span!(
            "tool_invoke",
            gen_ai.tool.name = "Skill",
            simulacra.skill.source = "model"
        );
        let _entered = tool_span.enter();
        catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");

        let captured = activation_events(&events);
        assert_eq!(captured.len(), 1);
        let activation = &captured[0];
        assert_eq!(activation.current_span.as_deref(), Some("tool_invoke"));
        assert_eq!(
            activation
                .fields
                .get("simulacra.skill.source")
                .map(String::as_str),
            Some("model"),
            "activation event must carry explicit model attribution rather than relying only on an ambient span: {activation:?}"
        );
        assert_eq!(
            activation
                .fields
                .get("simulacra.mcp.activation.link")
                .map(String::as_str),
            Some("tool_invoke"),
            "activation event must retain an explicit trigger link for exported telemetry: {activation:?}"
        );
    }

    #[tokio::test]
    async fn failed_multi_server_activation_discards_manager_state_and_retry_restarts_cleanly() {
        let _guard = catalog_test_guard().await;
        let healthy = JsonRpcServer::new("healthy_tool").await;
        let unavailable = TcpListener::bind("127.0.0.1:0").expect("reserve unavailable port");
        let unavailable_url = format!(
            "http://{}",
            unavailable.local_addr().expect("unavailable address")
        );
        drop(unavailable);

        let catalog = McpCatalog::new(vec![
            McpServerDescriptor::network(
                "healthy".into(),
                healthy.url.clone(),
                Some("http".into()),
            ),
            McpServerDescriptor::network(
                "unavailable".into(),
                unavailable_url,
                Some("http".into()),
            ),
        ])
        .expect("catalog should construct");

        catalog
            .activate(
                "repo-work",
                &["healthy".into(), "unavailable".into()],
                &mcp_capability(),
            )
            .await
            .expect_err("one unavailable dependency must fail the whole activation");

        assert_eq!(
            healthy.initialize_requests.load(Ordering::SeqCst),
            1,
            "the healthy sibling must complete initialize before the unavailable sibling fails"
        );
        assert_eq!(
            healthy.tools_list_requests.load(Ordering::SeqCst),
            1,
            "the healthy sibling must complete tools/list before the unavailable sibling fails"
        );
        assert!(
            catalog.state.lock().await.activated.is_empty(),
            "failed activation must not publish inventory"
        );
        assert!(
            catalog.manager.lock().await.connections.is_empty(),
            "failed activation must discard provisional manager connections as well as catalog inventory"
        );

        catalog
            .activate("repo-work", &["healthy".into()], &mcp_capability())
            .await
            .expect("retry after atomic rollback should start a fresh healthy handshake");
        assert_eq!(
            healthy.initialize_requests.load(Ordering::SeqCst),
            2,
            "the retry must re-initialize instead of reusing a connection retained from the failed transaction"
        );
        assert_eq!(
            healthy.tools_list_requests.load(Ordering::SeqCst),
            2,
            "the retry must re-inventory instead of reusing provisional tools"
        );
    }

    #[tokio::test]
    async fn activation_and_search_are_journaled_with_agent_attribution_before_catalog_visibility()
    {
        let _guard = catalog_test_guard().await;
        let catalog = McpCatalog::new(vec![McpServerDescriptor::wasm(
            "github".into(),
            WasmMcpServerDescriptor {
                module_path: std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/echo-mcp.wasm"
                )),
                network_allowlist: Vec::new(),
                hooks: None,
                journal: None,
                agent_id: AgentId("catalog-agent".into()),
            },
        )])
        .expect("catalog should construct");
        let journal = Arc::new(RecordingJournal::default());
        let agent_id = AgentId("catalog-agent".into());
        {
            let mut manager = catalog.manager.lock().await;
            manager.journal = Some(journal.clone());
            manager.agent_id = agent_id.clone();
        }

        catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");
        let search = McpSearchTool::new(Arc::clone(&catalog));
        let found = search
            .call(json!({"query":"echo"}), &mcp_capability())
            .await
            .expect("search should return activated tool");
        assert_eq!(found.as_array().expect("search array").len(), 1);

        let entries = journal.read_all(&agent_id).expect("journal should read");
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.entry,
                JournalEntryKind::ToolCall { tool_name, arguments, .. }
                    if tool_name == "mcp_activation"
                        && arguments == &json!({"skill":"repo-work","servers":["github"]})
            )),
            "activation must be recorded with skill and dependency set before its inventory becomes visible; entries: {entries:?}"
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.entry,
                JournalEntryKind::ToolCall { tool_name, arguments, .. }
                    if tool_name == "mcp_search" && arguments == &json!({"query":"echo"})
            )),
            "search publication must be journaled before returning searchable tools; entries: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.schema_version == JOURNAL_SCHEMA_VERSION)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_and_remote_call_errors_are_actionable_without_leaking_backend_secrets() {
        let _guard = catalog_test_guard().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let journal_catalog = McpCatalog::with_journal(
            Vec::new(),
            Arc::new(SecretFailingJournal),
            AgentId("redaction-agent".into()),
        )
        .expect("catalog should construct");
        let search_error = McpSearchTool::new(journal_catalog)
            .call(json!({"query":"issues"}), &mcp_capability())
            .await
            .expect_err("journal failure should fail search");

        let remote = JsonRpcServer::new("secret_error_tool").await;
        let remote_catalog = McpCatalog::new(vec![McpServerDescriptor::network(
            "github".into(),
            remote.url.clone(),
            Some("http".into()),
        )])
        .expect("catalog should construct");
        remote_catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");
        McpSearchTool::new(Arc::clone(&remote_catalog))
            .call(json!({"query":"secret_error_tool"}), &mcp_capability())
            .await
            .expect("search should publish remote tool");
        let call_error = McpCallTool::new(remote_catalog)
            .call(
                json!({"server":"github","tool":"secret_error_tool","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect_err("remote JSON-RPC failure should be returned");

        let search_message = search_error.to_string();
        let call_message = call_error.to_string();
        let observable = format!(
            "{search_message}\n{call_message}\n{:?}",
            events.lock().expect("event capture mutex")
        );
        for secret in [
            "JOURNALUSER",
            "JOURNALPASS",
            "JOURNALQUERY",
            "JOURNALAUTH",
            "JOURNALMODULE",
            "REMOTEUSER",
            "REMOTEPASS",
            "REMOTEQUERY",
            "REMOTEAUTH",
            "REMOTEMODULE",
        ] {
            assert!(
                !observable.contains(secret),
                "returned errors and captured logs must redact {secret}; got: {observable}"
            );
        }
        assert!(search_message.to_ascii_lowercase().contains("search"));
        assert!(call_message.contains("github") && call_message.contains("secret_error_tool"));
    }
}
