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
            return Err(ToolError::ExecutionFailed(error.to_string()));
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
                McpServerKind::Network { url, transport } => manager.connect_named(&descriptor.name, url, transport.as_deref()).await,
                McpServerKind::Wasm(descriptor) => {
                    let mut module = crate::load_wasm_mcp_module(&descriptor.module_path).map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
                    module = module.with_network_allowlist(descriptor.network_allowlist.clone()).with_agent_id(descriptor.agent_id.clone());
                    if let Some(hooks) = &descriptor.hooks { module = module.with_hooks(Arc::clone(hooks)); }
                    if let Some(journal) = &descriptor.journal { module = module.with_journal(Arc::clone(journal)); }
                    manager.connect_wasm_module(server, module).await
                }
            }
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
        Ok(())
        }.await;
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use simulacra_types::{
        AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
        JournalError, JournalStorage, TokenUsage,
    };

    #[derive(Default)]
    struct RecordingJournal(Mutex<Vec<JournalEntry>>);

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

    struct JsonRpcServer {
        url: String,
        initialize_requests: Arc<AtomicUsize>,
        tools_list_requests: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl JsonRpcServer {
        fn new(tool_name: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("test MCP server should bind");
            listener
                .set_nonblocking(true)
                .expect("test MCP listener should be nonblocking");
            let url = format!(
                "http://{}",
                listener.local_addr().expect("test server address")
            );
            let initialize_requests = Arc::new(AtomicUsize::new(0));
            let tools_list_requests = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let initialize_for_thread = Arc::clone(&initialize_requests);
            let list_for_thread = Arc::clone(&tools_list_requests);
            let stop_for_thread = Arc::clone(&stop);
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
            let thread = thread::spawn(move || {
                ready_tx
                    .send(())
                    .expect("test MCP server readiness receiver should remain available");
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .expect("request timeout");
                            let Some(request) = read_json_rpc_request(&mut stream) else {
                                continue;
                            };
                            let body = if request.contains("\"method\":\"initialize\"") {
                                initialize_for_thread.fetch_add(1, Ordering::SeqCst);
                                json!({"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"test","version":"1"},"capabilities":{}}}).to_string()
                            } else if request.contains("\"method\":\"tools/list\"") {
                                list_for_thread.fetch_add(1, Ordering::SeqCst);
                                json!({"jsonrpc":"2.0","result":{"tools":[{"name":tool_name,"description":"A catalog test tool","inputSchema":{"type":"object"}}]}}).to_string()
                            } else {
                                json!({"jsonrpc":"2.0","result":{}}).to_string()
                            };
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => break,
                    }
                }
            });
            ready_rx
                .recv()
                .expect("test MCP server should begin accepting before activation");
            Self {
                url,
                initialize_requests,
                tools_list_requests,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for JsonRpcServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("test MCP server should stop");
            }
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

    fn read_json_rpc_request(stream: &mut std::net::TcpStream) -> Option<String> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut expected_len = None;
        loop {
            match stream.read(&mut buffer) {
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
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return (!request.is_empty())
                        .then(|| String::from_utf8_lossy(&request).into_owned());
                }
                Err(_) => return None,
            }
        }
        (!request.is_empty()).then(|| String::from_utf8_lossy(&request).into_owned())
    }

    #[tokio::test]
    async fn failed_multi_server_activation_discards_manager_state_and_retry_restarts_cleanly() {
        let _guard = catalog_test_guard().await;
        let healthy = JsonRpcServer::new("healthy_tool");
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
}
