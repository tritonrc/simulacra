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

struct CatalogState {
    snapshot: Arc<CatalogSnapshot>,
    // Retained as the mutable commit source for activation. Search reads only
    // the immutable snapshot.
    activated: BTreeMap<String, Vec<ToolDefinition>>,
    published: HashSet<(String, String)>,
}

impl Default for CatalogState {
    fn default() -> Self {
        Self {
            snapshot: Arc::new(CatalogSnapshot::default()),
            activated: BTreeMap::new(),
            published: HashSet::new(),
        }
    }
}

#[derive(Default)]
struct CatalogSnapshot {
    entries: Vec<SearchEntry>,
    postings: BTreeMap<String, Vec<usize>>,
}

struct SearchEntry {
    server: String,
    tool_index: usize,
    tool_name: String,
    normalized_tool_name: String,
    server_tokens: Vec<String>,
    tool_tokens: Vec<String>,
    description_tokens: Vec<String>,
}

impl CatalogSnapshot {
    /// Build the immutable, schema-free search projection. The authoritative
    /// inventory remains in `CatalogState`; only result winners consult it to
    /// materialize their schemas.
    fn build(inventories: &BTreeMap<String, Vec<ToolDefinition>>) -> Self {
        let mut entries = Vec::new();
        let mut postings: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (server, tools) in inventories {
            let server_tokens = normalize_tokens(server);
            for (tool_index, tool) in tools.iter().enumerate() {
                let tool_tokens = normalize_tokens(&tool.name);
                let description_tokens = normalize_tokens(&tool.description);
                let entry_index = entries.len();
                let mut indexed_tokens = BTreeSet::new();
                indexed_tokens.extend(server_tokens.iter().cloned());
                indexed_tokens.extend(tool_tokens.iter().cloned());
                indexed_tokens.extend(description_tokens.iter().cloned());
                for token in indexed_tokens {
                    postings.entry(token).or_default().push(entry_index);
                }
                entries.push(SearchEntry {
                    server: server.clone(),
                    tool_index,
                    tool_name: tool.name.clone(),
                    normalized_tool_name: tool_tokens.join(" "),
                    server_tokens: server_tokens.clone(),
                    tool_tokens,
                    description_tokens,
                });
            }
        }
        Self { entries, postings }
    }

    fn candidates_for_term(&self, term: &str) -> BTreeSet<usize> {
        let mut candidates = BTreeSet::new();
        if let Some(exact) = self.postings.get(term) {
            candidates.extend(exact);
        }
        if term.chars().count() >= 3 {
            for (token, postings) in self.postings.range(term.to_owned()..) {
                if !token.starts_with(term) {
                    break;
                }
                candidates.extend(postings);
            }
        }
        candidates
    }
}

#[cfg(test)]
fn refresh_test_snapshot(state: &mut CatalogState) {
    // Some focused tests seed the authoritative inventory directly. Rebuild
    // only the schema-free projection for that test seam.
    state.snapshot = Arc::new(CatalogSnapshot::build(&state.activated));
}

#[cfg(not(test))]
fn refresh_test_snapshot(_: &mut CatalogState) {}

fn normalize_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn field_score(tokens: &[String], term: &str, exact: u32, prefix: u32) -> u32 {
    if tokens.iter().any(|token| token == term) {
        exact
    } else if term.chars().count() >= 3 && tokens.iter().any(|token| token.starts_with(term)) {
        prefix
    } else {
        0
    }
}

struct RankedMatch {
    entry_index: usize,
    score: u32,
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
        self.activate_with_context(
            skill,
            servers,
            capability,
            &simulacra_types::SkillActivationContext::model(),
        )
        .await
    }

    pub async fn activate_with_context(
        &self,
        skill: &str,
        servers: &[String],
        capability: &CapabilityToken,
        context: &simulacra_types::SkillActivationContext,
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
            emit_activation_telemetry(skill, &declared_servers, 0, "failure", context);
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
            emit_activation_telemetry(skill, &declared_servers, 0, "failure", context);
            return Err(error);
        }
        let already: BTreeSet<String> = self.state.lock().await.activated.keys().cloned().collect();
        let pending: Vec<_> = unique
            .into_iter()
            .filter(|server| !already.contains(server))
            .collect();
        if pending.is_empty() {
            emit_activation_telemetry(skill, &declared_servers, 0, "success", context);
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
                    #[cfg(feature = "wasm")]
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
                    #[cfg(not(feature = "wasm"))]
                    McpServerKind::Wasm(_) => Err(crate::McpError::ConnectionFailed(
                        "WASM MCP support is disabled at compile time".to_string(),
                    )),
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
            emit_activation_telemetry(skill, &declared_servers, 0, "failure", context);
            return Err(error);
        }
        drop(manager);
        let count = temporary.values().map(Vec::len).sum();
        let mut state = self.state.lock().await;
        state.activated.extend(temporary);
        // Publish the index while the authoritative inventory is still held,
        // so readers can never observe a snapshot that disagrees with it.
        state.snapshot = Arc::new(CatalogSnapshot::build(&state.activated));
        emit_activation_telemetry(skill, &declared_servers, count, "success", context);
        Ok(count)
    }

    async fn search(&self, query: &str) -> Result<Vec<Value>, ToolError> {
        let snapshot = {
            let mut state = self.state.lock().await;
            refresh_test_snapshot(&mut state);
            Arc::clone(&state.snapshot)
        };
        let mut seen_terms = HashSet::new();
        let terms: Vec<_> = normalize_tokens(query)
            .into_iter()
            .filter(|term| seen_terms.insert(term.clone()))
            .collect();
        let catalog_tool_count = snapshot.entries.len();
        let candidates = if terms.is_empty() {
            (0..catalog_tool_count).collect::<BTreeSet<_>>()
        } else {
            let mut term_sets = terms.iter().map(|term| snapshot.candidates_for_term(term));
            let Some(mut intersection) = term_sets.next() else {
                return Ok(Vec::new());
            };
            for term_set in term_sets {
                intersection.retain(|candidate| term_set.contains(candidate));
            }
            intersection
        };
        let indexed_candidate_count = candidates.len();
        // Matching and per-term relevance intentionally use distinct terms,
        // but complete-name relevance retains the query's normalized order.
        // Keeping the first occurrence also means duplicate query terms cannot
        // alter ranking.
        let complete_query = terms.join(" ");
        let mut winners: Vec<RankedMatch> = Vec::with_capacity(5);
        for entry_index in candidates {
            let entry = &snapshot.entries[entry_index];
            let score = terms.iter().fold(0_u32, |total, term| {
                total
                    + field_score(&entry.tool_tokens, term, 6, 5)
                        .max(field_score(&entry.server_tokens, term, 4, 3))
                        .max(field_score(&entry.description_tokens, term, 2, 1))
            }) + u32::from(
                !complete_query.is_empty() && complete_query == entry.normalized_tool_name,
            ) * 1_000;
            let insertion = winners
                .iter()
                .position(|existing| {
                    let other = &snapshot.entries[existing.entry_index];
                    score > existing.score
                        || (score == existing.score
                            && (entry.server.as_str(), entry.tool_name.as_str())
                                < (other.server.as_str(), other.tool_name.as_str()))
                })
                .unwrap_or(winners.len());
            if insertion < 5 {
                winners.insert(insertion, RankedMatch { entry_index, score });
                if winners.len() > 5 {
                    winners.pop();
                }
            }
        }
        self.manager
            .lock()
            .await
            .append_journal_tool_call("mcp_search", &json!({"query_length": query.len()}))
            .map_err(|_| {
                tracing::warn!(stage = "audit", "MCP catalog search failed");
                ToolError::ExecutionFailed("MCP catalog search could not be audited".into())
            })?;
        let mut state = self.state.lock().await;
        let results = winners
            .iter()
            .map(|winner| {
                let entry = &snapshot.entries[winner.entry_index];
                state
                    .published
                    .insert((entry.server.clone(), entry.tool_name.clone()));
                let tool = state
                    .activated
                    .get(&entry.server)
                    .and_then(|tools| tools.get(entry.tool_index))
                    .expect("published catalog snapshot entries retain authoritative tools");
                json!({"server": entry.server, "tool": tool.name, "description": tool.description, "input_schema": tool.input_schema})
            })
            .collect();
        tracing::info!(
            simulacra.mcp.search.query_length = query.len(),
            simulacra.mcp.search.result_count = winners.len(),
            simulacra.mcp.search.catalog_tool_count = catalog_tool_count,
            simulacra.mcp.search.indexed_candidate_count = indexed_candidate_count,
            "MCP catalog search"
        );
        for winner in &winners {
            let entry = &snapshot.entries[winner.entry_index];
            tracing::info!(
                server = %entry.server,
                tool = %entry.tool_name,
                "MCP catalog search result"
            );
        }
        Ok(results)
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
        let result = self
            .manager
            .lock()
            .await
            .call_tool(&server, &tool, arguments, &capability)
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(crate::McpError::CapabilityDenied(detail)) => Err(ToolError::ExecutionFailed(
                format!("capability denied for MCP tool {server}:{tool}: {detail}"),
            )),
            Err(_) => {
                tracing::warn!(server = %server, tool = %tool, stage = "dispatch", "MCP catalog call failed");
                Err(ToolError::ExecutionFailed(format!(
                    "MCP call to server {server:?} tool {tool:?} failed during dispatch"
                )))
            }
        }
    }
}

impl SkillDependencyActivator for McpCatalog {
    fn activate(
        &self,
        skill: String,
        mcp_servers: Vec<String>,
        capability: CapabilityToken,
        context: simulacra_types::SkillActivationContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send + '_>> {
        Box::pin(async move {
            self.activate_with_context(&skill, &mcp_servers, &capability, &context)
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

fn emit_activation_telemetry(
    skill: &str,
    servers: &[String],
    tool_count: usize,
    outcome: &str,
    context: &simulacra_types::SkillActivationContext,
) {
    let server_set = serde_json::to_string(servers).unwrap_or_else(|_| "[]".to_string());
    tracing::info!(
        simulacra.skill.name = %skill,
        simulacra.mcp.servers = %server_set,
        simulacra.mcp.activated_tool_count = tool_count,
        simulacra.mcp.activation.outcome = %outcome,
        simulacra.skill.source = %context.source,
        simulacra.mcp.activation.link = %context.link,
        simulacra.mcp.activation.correlation = %context.correlation,
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

    fn output_from_value(&self, value: Value) -> simulacra_types::ToolOutput {
        let mut output = simulacra_types::ToolOutput::from_value(value);
        output.log_preview = "[REDACTED]".into();
        output
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
        AgentId, CheckpointData, JournalEntry, JournalError, JournalStorage, TokenUsage,
    };
    #[cfg(feature = "wasm")]
    use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntryKind};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Default)]
    struct RecordingJournal(Mutex<Vec<JournalEntry>>);

    struct SecretFailingJournal;

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        fields: HashMap<String, String>,
        #[cfg(feature = "wasm")]
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
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.events
                .lock()
                .expect("event capture mutex")
                .push(CapturedEvent {
                    fields,
                    #[cfg(feature = "wasm")]
                    current_span: _ctx.lookup_current().map(|span| span.name().to_string()),
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
            Self::with_tools(vec![ToolDefinition {
                name: tool_name.into(),
                description: "A catalog test tool".into(),
                input_schema: json!({"type":"object"}),
            }])
            .await
        }

        async fn with_tools(tools: Vec<ToolDefinition>) -> Self {
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
                    let tools = tools.clone();
                    tokio::spawn(async move {
                        let Some(request) = read_json_rpc_request(&mut stream).await else {
                            return;
                        };
                        let body = if request.contains("\"method\":\"initialize\"") {
                            initialize_requests.fetch_add(1, Ordering::SeqCst);
                            json!({"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"test","version":"1"},"capabilities":{}}}).to_string()
                        } else if request.contains("\"method\":\"tools/list\"") {
                            tools_list_requests.fetch_add(1, Ordering::SeqCst);
                            json!({"jsonrpc":"2.0","result":{"tools":tools}}).to_string()
                        } else if request.contains("\"method\":\"tools/call\"") {
                            tool_call_requests.fetch_add(1, Ordering::SeqCst);
                            if tools.iter().any(|tool| tool.name == "secret_error_tool") {
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

    #[cfg(feature = "wasm")]
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

    #[cfg(feature = "wasm")]
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

    #[cfg(feature = "wasm")]
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

    #[cfg(feature = "wasm")]
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
    async fn legacy_sse_activation_failure_redacts_endpoint_secrets_but_retains_safe_context() {
        let _guard = catalog_test_guard().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let endpoint = "http://sse-user:SSEPASS@127.0.0.1:not-a-port/sse?authorization=SSEAUTH&module_path=/tmp/SECRET-MODULE.wasm";
        let catalog = McpCatalog::new(vec![McpServerDescriptor::network(
            "legacy".into(),
            endpoint.into(),
            Some("sse".into()),
        )])
        .expect("catalog should construct without connecting");

        let error = catalog
            .activate("legacy-skill", &["legacy".into()], &mcp_capability())
            .await
            .expect_err("malformed legacy SSE endpoint should fail activation")
            .to_string();
        let rendered = format!("{error} {:?}", events.lock().expect("event capture mutex"));
        for secret in [
            endpoint,
            "sse-user",
            "SSEPASS",
            "SSEAUTH",
            "SECRET-MODULE.wasm",
            "authorization",
        ] {
            assert!(
                !rendered.contains(secret),
                "legacy SSE failure leaked {secret:?}: {rendered}"
            );
        }
        for safe in ["legacy", "sse", "connect"] {
            assert!(
                rendered.to_ascii_lowercase().contains(safe),
                "legacy SSE failure must retain safe context {safe:?}: {rendered}"
            );
        }
    }

    #[cfg(not(feature = "wasm"))]
    #[tokio::test(flavor = "current_thread")]
    async fn wasm_activation_without_feature_returns_sanitized_typed_failure() {
        let module_path = std::path::PathBuf::from("/private/SECRET-MCP-MODULE.wasm");
        let catalog = McpCatalog::new(vec![McpServerDescriptor::wasm(
            "github".into(),
            WasmMcpServerDescriptor {
                module_path: module_path.clone(),
                network_allowlist: Vec::new(),
                hooks: None,
                journal: None,
                agent_id: AgentId("no-wasm-agent".into()),
            },
        )])
        .expect("WASM descriptor should remain valid when support is disabled");

        let error = catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect_err("WASM activation should fail when support is disabled");

        match error {
            ToolError::ExecutionFailed(message) => {
                assert_eq!(
                    message,
                    "skill \"repo-work\" could not activate MCP server \"github\" during connect"
                );
                assert!(
                    !message.contains(module_path.to_string_lossy().as_ref())
                        && !message.contains("SECRET-MCP-MODULE.wasm"),
                    "public activation failure leaked the configured module path: {message}"
                );
            }
            other => panic!("expected typed execution failure, got {other:?}"),
        }
    }

    #[cfg(feature = "wasm")]
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

    #[cfg(feature = "wasm")]
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

    #[cfg(feature = "wasm")]
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
                    if tool_name == "mcp_search" && arguments == &json!({"query_length":4})
            )),
            "search publication must journal safe query metadata before returning searchable tools; entries: {entries:?}"
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

    #[tokio::test(flavor = "current_thread")]
    async fn successful_search_and_call_redact_secret_inputs_from_journal_and_tracing() {
        let _guard = catalog_test_guard().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let journal = Arc::new(RecordingJournal::default());
        let agent_id = AgentId("secret-input-agent".into());
        let remote = JsonRpcServer::new("issues").await;
        let catalog = McpCatalog::with_journal(
            vec![McpServerDescriptor::network(
                "github".into(),
                remote.url.clone(),
                Some("http".into()),
            )],
            journal.clone(),
            agent_id.clone(),
        )
        .expect("catalog should construct");
        catalog
            .activate("repo-work", &["github".into()], &mcp_capability())
            .await
            .expect("activation should succeed");

        let secret_query = "issues https://QUERYUSER:QUERYPASS@example.invalid/mcp?token=QUERYTOKEN Authorization: Bearer QUERYAUTH";
        let results = McpSearchTool::new(Arc::clone(&catalog))
            .call(json!({"query":secret_query}), &mcp_capability())
            .await
            .expect("search should succeed");
        assert_eq!(results.as_array().expect("search result array").len(), 0);
        McpSearchTool::new(Arc::clone(&catalog))
            .call(json!({"query":"issues"}), &mcp_capability())
            .await
            .expect("safe search should publish the remote tool");
        let secret_arguments = json!({
            "endpoint":"https://CALLUSER:CALLPASS@example.invalid/mcp?token=CALLTOKEN",
            "authorization":"Bearer CALLAUTH",
            "module_path":"/private/CALLMODULE.wasm"
        });
        McpCallTool::new(catalog)
            .call(
                json!({"server":"github","tool":"issues","arguments":secret_arguments}),
                &mcp_capability(),
            )
            .await
            .expect("remote call should succeed");

        let entries = journal.read_all(&agent_id).expect("journal should read");
        let observable = format!(
            "{:?}\n{:?}",
            entries,
            events.lock().expect("event capture mutex")
        );
        for secret in [
            "QUERYUSER",
            "QUERYPASS",
            "QUERYTOKEN",
            "QUERYAUTH",
            "CALLUSER",
            "CALLPASS",
            "CALLTOKEN",
            "CALLAUTH",
            "CALLMODULE",
        ] {
            assert!(
                !observable.contains(secret),
                "journals and tracing must omit raw search/call inputs containing {secret}; got: {observable}"
            );
        }
        let captured = events.lock().expect("event capture mutex");
        let search_event = captured
            .iter()
            .find(|event| {
                event.fields.get("simulacra.mcp.search.query_length")
                    == Some(&secret_query.len().to_string())
            })
            .expect("safe search telemetry");
        assert_eq!(
            search_event.fields.get("simulacra.mcp.search.query_length"),
            Some(&secret_query.len().to_string())
        );
        assert_eq!(
            search_event.fields.get("simulacra.mcp.search.result_count"),
            Some(&"0".into())
        );
        assert!(captured.iter().any(|event| {
            event
                .fields
                .get("server")
                .is_some_and(|value| value == "github")
                && event
                    .fields
                    .get("tool")
                    .is_some_and(|value| value == "issues")
        }));
    }

    fn indexed_tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": {"payload": {"type": "string"}}
            }),
        }
    }

    fn indexed_descriptor(name: &str, server: &JsonRpcServer) -> McpServerDescriptor {
        McpServerDescriptor::network(name.into(), server.url.clone(), Some("http".into()))
    }

    async fn indexed_search_pairs(catalog: &Arc<McpCatalog>, query: &str) -> Vec<(String, String)> {
        McpSearchTool::new(Arc::clone(catalog))
            .call(json!({"query": query}), &mcp_capability())
            .await
            .expect("search should succeed")
            .as_array()
            .expect("search result array")
            .iter()
            .map(|result| {
                (
                    result["server"].as_str().expect("server name").to_string(),
                    result["tool"].as_str().expect("tool name").to_string(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn indexed_search_normalizes_boundaries_and_requires_every_distinct_term() {
        let enterprise = JsonRpcServer::with_tools(vec![indexed_tool(
            "Repo_Search",
            "Find pull requests by CODE owner and language",
        )])
        .await;
        let github = JsonRpcServer::with_tools(vec![indexed_tool(
            "repo_owner",
            "Show repository owner metadata",
        )])
        .await;
        let linear = JsonRpcServer::with_tools(vec![indexed_tool(
            "language_search",
            "Search language references",
        )])
        .await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("GitHub-Enterprise", &enterprise),
            indexed_descriptor("github", &github),
            indexed_descriptor("linear", &linear),
        ])
        .expect("catalog should construct");
        catalog
            .activate(
                "repo-work",
                &["GitHub-Enterprise".into(), "github".into(), "linear".into()],
                &mcp_capability(),
            )
            .await
            .expect("fixture inventories should activate");

        let canonical = indexed_search_pairs(&catalog, "GITHUB repo CODE language").await;
        assert_eq!(
            canonical,
            vec![("GitHub-Enterprise".into(), "Repo_Search".into())],
            "server, tool, description, and query must be lowercased and tokenized at non-alphanumeric boundaries"
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "language code github repo").await,
            canonical,
            "query-term order must not affect matches or ranking"
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "repo language repo code github code").await,
            canonical,
            "duplicate normalized terms must not affect matches or ranking"
        );
        assert!(
            indexed_search_pairs(&catalog, "github repo code language missing")
                .await
                .is_empty(),
            "a tool missing any distinct query term must be excluded"
        );
    }

    #[tokio::test]
    async fn indexed_search_honors_prefix_boundary_and_relevance_order() {
        let zeta = JsonRpcServer::with_tools(vec![
            indexed_tool("a", "one-character exact token"),
            indexed_tool("al", "two-character exact token"),
        ])
        .await;
        let alpha = JsonRpcServer::with_tools(vec![
            indexed_tool("alpha", "Greek target"),
            indexed_tool("alpine", "Mountain target"),
        ])
        .await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("zeta", &zeta),
            indexed_descriptor("alpha", &alpha),
        ])
        .expect("catalog should construct");
        catalog
            .activate(
                "prefixes",
                &["zeta".into(), "alpha".into()],
                &mcp_capability(),
            )
            .await
            .expect("fixture inventories should activate");
        assert_eq!(
            indexed_search_pairs(&catalog, "a").await,
            vec![("zeta".into(), "a".into())],
            "one-character terms must match exact tokens but not prefixes"
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "al").await,
            vec![("zeta".into(), "al".into())],
            "one- and two-character terms must not prefix-match longer tokens"
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "alp").await,
            vec![
                ("alpha".into(), "alpha".into()),
                ("alpha".into(), "alpine".into()),
            ],
            "three-character terms may prefix-match indexed tokens"
        );
    }

    #[tokio::test]
    async fn indexed_search_applies_all_six_field_weights_in_order() {
        let exact_tool = JsonRpcServer::with_tools(vec![indexed_tool("needle", "other")]).await;
        let tool_prefix =
            JsonRpcServer::with_tools(vec![indexed_tool("needlework", "other")]).await;
        let exact_server = JsonRpcServer::with_tools(vec![indexed_tool("third", "other")]).await;
        let server_prefix = JsonRpcServer::with_tools(vec![indexed_tool("fourth", "other")]).await;
        let exact_description =
            JsonRpcServer::with_tools(vec![indexed_tool("fifth", "needle token")]).await;
        let description_prefix =
            JsonRpcServer::with_tools(vec![indexed_tool("sixth", "needlework token")]).await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("z-tool-exact", &exact_tool),
            indexed_descriptor("z-tool-prefix", &tool_prefix),
            indexed_descriptor("needle", &exact_server),
            indexed_descriptor("needle-server", &server_prefix),
            indexed_descriptor("z-description-exact", &exact_description),
            indexed_descriptor("z-description-prefix", &description_prefix),
        ])
        .expect("catalog should construct");
        let dependencies = [
            "z-tool-exact".into(),
            "z-tool-prefix".into(),
            "needle".into(),
            "needle-server".into(),
            "z-description-exact".into(),
            "z-description-prefix".into(),
        ];
        catalog
            .activate("weights", &dependencies, &mcp_capability())
            .await
            .expect("fixture inventories should activate");
        assert_eq!(
            indexed_search_pairs(&catalog, "needle").await,
            vec![
                ("z-tool-exact".into(), "needle".into()),
                ("z-tool-prefix".into(), "needlework".into()),
                ("needle".into(), "third".into()),
                ("needle-server".into(), "fourth".into()),
                ("z-description-exact".into(), "fifth".into()),
            ],
            "the top-five bound must retain the first five weights and omit description prefix, the sixth"
        );
    }

    #[tokio::test]
    async fn indexed_search_complete_tool_name_boost_is_separate_from_token_weights() {
        let server = JsonRpcServer::with_tools(vec![
            indexed_tool("search-repo", "same"),
            indexed_tool("repo-search", "same"),
        ])
        .await;
        let catalog = McpCatalog::new(vec![indexed_descriptor("tools", &server)])
            .expect("catalog should construct");
        catalog
            .activate("boost", &["tools".into()], &mcp_capability())
            .await
            .expect("fixture inventory should activate");
        assert_eq!(
            indexed_search_pairs(&catalog, "search repo").await,
            vec![
                ("tools".into(), "search-repo".into()),
                ("tools".into(), "repo-search".into()),
            ],
            "a complete normalized query in a tool's native token order must boost that tool over a competing permutation"
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "repo search").await,
            vec![
                ("tools".into(), "repo-search".into()),
                ("tools".into(), "search-repo".into()),
            ],
            "reordering terms must retain both matches while applying the complete-query boost to the corresponding native-order tool name"
        );
    }

    #[tokio::test]
    async fn indexed_search_ties_empty_query_and_publication_are_bounded_and_deterministic() {
        let alpha = JsonRpcServer::with_tools(vec![
            indexed_tool("gamma", "same tie"),
            indexed_tool("alpha", "same tie"),
            indexed_tool("beta", "same tie"),
        ])
        .await;
        let bravo = JsonRpcServer::with_tools(vec![
            indexed_tool("beta", "same tie"),
            indexed_tool("alpha", "same tie"),
        ])
        .await;
        let charlie = JsonRpcServer::with_tools(vec![indexed_tool("delta", "same tie")]).await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("bravo", &bravo),
            indexed_descriptor("alpha", &alpha),
            indexed_descriptor("charlie", &charlie),
        ])
        .expect("catalog should construct");
        let dependencies = ["bravo".into(), "alpha".into(), "charlie".into()];
        assert_eq!(
            catalog
                .activate("ties", &dependencies, &mcp_capability())
                .await
                .expect("fixture inventories should activate"),
            6
        );
        assert_eq!(
            catalog
                .activate("ties", &dependencies, &mcp_capability())
                .await
                .expect("repeat activation should reuse committed inventories"),
            0
        );
        let expected = vec![
            ("alpha".into(), "alpha".into()),
            ("alpha".into(), "beta".into()),
            ("alpha".into(), "gamma".into()),
            ("bravo".into(), "alpha".into()),
            ("bravo".into(), "beta".into()),
        ];
        assert_eq!(indexed_search_pairs(&catalog, "same").await, expected);
        assert_eq!(indexed_search_pairs(&catalog, "").await, expected);

        McpCallTool::new(Arc::clone(&catalog))
            .call(
                json!({"server":"alpha","tool":"alpha","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect("a returned result must be published and callable");
        let omitted_error = McpCallTool::new(catalog)
            .call(
                json!({"server":"charlie","tool":"delta","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect_err("sixth match must not be published");
        assert!(
            omitted_error
                .to_string()
                .contains("not activated and search-published"),
            "matches beyond the five-result bound must remain unpublished"
        );
    }

    #[tokio::test]
    async fn indexed_snapshot_replaces_atomically_after_later_success_without_losing_publication() {
        let first = JsonRpcServer::with_tools(vec![indexed_tool("first_tool", "first")]).await;
        let later = JsonRpcServer::with_tools(vec![indexed_tool("later_tool", "later")]).await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("first", &first),
            indexed_descriptor("later", &later),
        ])
        .expect("catalog should construct");
        catalog
            .activate("first-skill", &["first".into()], &mcp_capability())
            .await
            .expect("first inventory should activate");
        assert_eq!(
            indexed_search_pairs(&catalog, "first").await,
            vec![("first".into(), "first_tool".into())]
        );

        catalog
            .activate("later-skill", &["later".into()], &mcp_capability())
            .await
            .expect("later inventory should activate");
        assert_eq!(
            indexed_search_pairs(&catalog, "later").await,
            vec![("later".into(), "later_tool".into())]
        );
        assert_eq!(
            indexed_search_pairs(&catalog, "").await,
            vec![
                ("first".into(), "first_tool".into()),
                ("later".into(), "later_tool".into()),
            ]
        );
        McpCallTool::new(Arc::clone(&catalog))
            .call(
                json!({"server":"first","tool":"first_tool","arguments":{}}),
                &mcp_capability(),
            )
            .await
            .expect("publication from the earlier snapshot must survive replacement");
        assert_eq!(
            catalog
                .activate("later-skill", &["later".into()], &mcp_capability())
                .await
                .expect("repeat activation should succeed"),
            0
        );
        assert_eq!(indexed_search_pairs(&catalog, "").await.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn indexed_search_telemetry_reports_candidate_pruning_without_query_terms() {
        let _guard = catalog_test_guard().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry::Registry::default().with(EventCapture {
            events: Arc::clone(&events),
        });
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let github = JsonRpcServer::with_tools(vec![
            indexed_tool("repo_search", "selective alpha"),
            indexed_tool("issue_lookup", "ticket workflow"),
        ])
        .await;
        let linear =
            JsonRpcServer::with_tools(vec![indexed_tool("triage_queue", "ticket workflow")]).await;
        let notion =
            JsonRpcServer::with_tools(vec![indexed_tool("doc_search", "workspace notes")]).await;
        let slack =
            JsonRpcServer::with_tools(vec![indexed_tool("thread_lookup", "workspace chat")]).await;
        let drive =
            JsonRpcServer::with_tools(vec![indexed_tool("file_search", "workspace files")]).await;
        let catalog = McpCatalog::new(vec![
            indexed_descriptor("github", &github),
            indexed_descriptor("linear", &linear),
            indexed_descriptor("notion", &notion),
            indexed_descriptor("slack", &slack),
            indexed_descriptor("drive", &drive),
        ])
        .expect("catalog should construct");
        catalog
            .activate(
                "telemetry",
                &[
                    "github".into(),
                    "linear".into(),
                    "notion".into(),
                    "slack".into(),
                    "drive".into(),
                ],
                &mcp_capability(),
            )
            .await
            .expect("fixture inventories should activate");

        let query = "selective alpha";
        assert_eq!(
            indexed_search_pairs(&catalog, query).await,
            vec![("github".into(), "repo_search".into())]
        );
        let captured = events.lock().expect("event capture mutex");
        let search_event = captured
            .iter()
            .find(|event| {
                event.fields.get("simulacra.mcp.search.query_length")
                    == Some(&query.len().to_string())
            })
            .expect("search telemetry event");
        assert_eq!(
            search_event
                .fields
                .get("simulacra.mcp.search.catalog_tool_count"),
            Some(&"6".into())
        );
        let candidate_count = search_event
            .fields
            .get("simulacra.mcp.search.indexed_candidate_count")
            .expect("candidate count telemetry")
            .parse::<usize>()
            .expect("candidate count should be numeric");
        assert!(candidate_count > 0 && candidate_count < 6);
        let rendered = format!("{captured:?}");
        for forbidden in ["selective", "alpha", query] {
            assert!(
                !rendered.contains(forbidden),
                "telemetry leaked {forbidden:?}"
            );
        }
    }
}
