use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use async_graphql::{EmptySubscription, Schema};
use serde_json::{Value, json};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, NewAgent, NewSkill, Skill, SkillId, Tenant};
use simulacra_config::{
    CatalogConfig, McpConfig, McpServerConfig, ProjectConfig, SimulacraConfig, VfsConfig,
};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot};
use simulacra_server::{
    BudgetPoolConfig, EngineError, ProviderFactory, ProviderKind, SimulacraEngine, TaskHandle,
    TaskManager, TaskState, TenantConfig,
};
use simulacra_types::{
    FinishReason, Message, Provider, ProviderError, ProviderResponse, ResourceBudget, Role,
    TokenUsage, ToolCallMessage, ToolDefinition,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Test serialization for env-var-touching tests. Use tokio::sync::Mutex
// so the guard can be held across `.await` points without tripping the
// `await_holding_lock` clippy lint.
static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

fn env_lock() -> &'static AsyncMutex<()> {
    ENV_LOCK.get_or_init(|| AsyncMutex::new(()))
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct ScriptedProvider {
    script: Mutex<VecDeque<ProviderResponse>>,
    recorded_messages: Mutex<Vec<Vec<Message>>>,
    recorded_tools: Mutex<Vec<Vec<ToolDefinition>>>,
}

impl ScriptedProvider {
    fn new(script: impl IntoIterator<Item = ProviderResponse>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            recorded_messages: Mutex::new(Vec::new()),
            recorded_tools: Mutex::new(Vec::new()),
        }
    }

    fn recorded_messages(&self) -> Vec<Vec<Message>> {
        self.recorded_messages
            .lock()
            .expect("recorded_messages lock should not be poisoned")
            .clone()
    }

    fn recorded_tools(&self) -> Vec<Vec<ToolDefinition>> {
        self.recorded_tools
            .lock()
            .expect("recorded_tools lock should not be poisoned")
            .clone()
    }
}

impl Provider for ScriptedProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let recorded = messages.to_vec();
        let recorded_tools = tools.to_vec();
        Box::pin(async move {
            let call_number = {
                let mut calls = self
                    .recorded_messages
                    .lock()
                    .expect("recorded_messages lock should not be poisoned");
                calls.push(recorded);
                calls.len()
            };
            self.recorded_tools
                .lock()
                .expect("recorded_tools lock should not be poisoned")
                .push(recorded_tools);

            let response = self
                .script
                .lock()
                .expect("script lock should not be poisoned")
                .pop_front()
                .unwrap_or_else(|| {
                    panic!(
                        "ScriptedProvider script exhausted on chat call #{call_number}; add another ProviderResponse to the test script"
                    )
                });

            Ok(response)
        })
    }
}

struct SharedProvider(Arc<ScriptedProvider>);

impl Provider for SharedProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.0.chat(messages, tools, budget)
    }
}

struct GenericHandoffProvider {
    recorded_messages: Mutex<Vec<Vec<Message>>>,
    recorded_tools: Mutex<Vec<Vec<ToolDefinition>>>,
}

impl GenericHandoffProvider {
    fn new() -> Self {
        Self {
            recorded_messages: Mutex::new(Vec::new()),
            recorded_tools: Mutex::new(Vec::new()),
        }
    }

    fn recorded_messages(&self) -> Vec<Vec<Message>> {
        self.recorded_messages
            .lock()
            .expect("recorded_messages lock should not be poisoned")
            .clone()
    }

    fn next_response(messages: &[Message]) -> ProviderResponse {
        let spawn_result = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call-spawn-generic"));

        if spawn_result.is_some() {
            return assistant_response("master synthesis: child handle accepted", "gpt-4o-mini");
        }

        tool_call_response(
            "call-spawn-generic",
            "spawn_agent",
            json!({
                "system_prompt": "You are a focused specialist sub-agent.",
                "task": "Analyze the delegated part and return a concise finding.",
                "budget": {
                    "max_tokens": 64,
                    "max_turns": 1,
                    "max_cost": "0",
                    "max_sub_agents": 0
                }
            }),
            "gpt-4o-mini",
        )
    }
}

impl Provider for GenericHandoffProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        let recorded = messages.to_vec();
        let recorded_tools = tools.to_vec();
        Box::pin(async move {
            self.recorded_messages
                .lock()
                .expect("recorded_messages lock should not be poisoned")
                .push(recorded);
            self.recorded_tools
                .lock()
                .expect("recorded_tools lock should not be poisoned")
                .push(recorded_tools);
            Ok(Self::next_response(messages))
        })
    }
}

struct SharedGenericHandoffProvider(Arc<GenericHandoffProvider>);

impl Provider for SharedGenericHandoffProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        self.0.chat(messages, tools, budget)
    }
}

fn empty_config() -> SimulacraConfig {
    SimulacraConfig {
        project: ProjectConfig {
            name: "provider-injection-tests".to_string(),
            description: None,
        },
        agent_types: HashMap::new(),
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

fn tenant_config(namespace: &str, agent_type: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: agent_type.to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

fn build_engine(catalog: &Catalog) -> SimulacraEngine {
    build_engine_with_config(catalog, empty_config())
}

fn build_engine_with_config(catalog: &Catalog, config: SimulacraConfig) -> SimulacraEngine {
    SimulacraEngine::new(
        config,
        None,
        Arc::new(catalog.agents()) as Arc<dyn AgentRepository>,
        Arc::new(catalog.skills()) as Arc<dyn SkillRepository>,
        Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>,
        Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>,
    )
    .expect("engine should construct over shared catalog handles")
}

async fn create_catalog_agent_with_capabilities(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    system_prompt: &str,
    model: &str,
    capabilities: &[String],
) {
    let skill_ids = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("test agent"),
                system_prompt,
                model,
                max_turns: Some(4),
                max_tokens: Some(256),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities,
                channel_ids: &[],
            },
        )
        .await
        .expect("agent should be created");
}

async fn create_catalog_skill(catalog: &Catalog, tenant: &Tenant, name: &str, body: &str) -> Skill {
    catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name,
                description: Some("Use runbook."),
                body,
                metadata: None,
            },
        )
        .await
        .expect("skill should be created")
}

async fn create_catalog_agent_with_skills(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    system_prompt: &str,
    model: &str,
    skill_ids: &[SkillId],
) {
    let capabilities = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("test agent"),
                system_prompt,
                model,
                max_turns: Some(4),
                max_tokens: Some(256),
                memory_pool_id: None,
                skill_ids,
                capabilities: &capabilities,
                channel_ids: &[],
            },
        )
        .await
        .expect("agent should be created");
}

async fn ensure_tenant(catalog: &Catalog, namespace: &str) -> Tenant {
    catalog
        .tenants()
        .get_or_create(namespace, Some(namespace))
        .await
        .expect("tenant should exist")
}

async fn spawn_minimal_mcp_server() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("MCP fixture should bind");
    let addr = listener.local_addr().expect("MCP fixture addr");
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_for_task = Arc::clone(&call_count);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let call_count = Arc::clone(&call_count_for_task);
            tokio::spawn(async move {
                let request = read_http_request(&mut socket).await;
                let body = if request.contains("\"method\":\"initialize\"") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {},
                            "serverInfo": { "name": "fixture-mcp", "version": "1.0.0" }
                        }
                    })
                } else if request.contains("\"method\":\"tools/list\"") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "Echo a payload.",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "text": { "type": "string" }
                                        }
                                    }
                                },
                                {
                                    "name": "delete",
                                    "description": "Delete a payload.",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {}
                                    }
                                }
                            ]
                        }
                    })
                } else if request.contains("\"method\":\"tools/call\"") {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": "echo: hello"
                            }],
                            "isError": false
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32601, "message": "unexpected request" }
                    })
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    (format!("http://{addr}/mcp"), call_count)
}

#[allow(dead_code)]
struct FakeOpenAiServer {
    base_url: String,
    requests: Arc<AsyncMutex<Vec<String>>>,
}

impl FakeOpenAiServer {
    #[allow(dead_code)]
    async fn first_request_json(&self) -> Value {
        let start = std::time::Instant::now();
        loop {
            if let Some(request) = self.requests.lock().await.first().cloned() {
                let body = request
                    .split("\r\n\r\n")
                    .nth(1)
                    .expect("captured request should include a body");
                return serde_json::from_str(body)
                    .expect("captured OpenAI request body should be valid JSON");
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "fake OpenAI server did not receive a request"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[allow(dead_code)]
async fn spawn_fake_openai_server(response: Value) -> FakeOpenAiServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("OpenAI fixture should bind");
    let addr = listener.local_addr().expect("OpenAI fixture addr");
    let requests = Arc::new(AsyncMutex::new(Vec::new()));
    let requests_for_task = Arc::clone(&requests);

    tokio::spawn(async move {
        let body = response.to_string();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let request = read_http_request(&mut socket).await;
            requests_for_task.lock().await.push(request);
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(http_response.as_bytes()).await;
        }
    });

    FakeOpenAiServer {
        base_url: format!("http://{addr}"),
        requests,
    }
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 1024];
    while let Ok(n) = socket.read(&mut tmp).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(header_end) = find_header_end(&buf) {
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            if buf.len() >= body_start + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn create_catalog_agent(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    system_prompt: &str,
    model: &str,
) {
    let skill_ids = Vec::new();
    let capabilities = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("test agent"),
                system_prompt,
                model,
                max_turns: Some(4),
                max_tokens: Some(256),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &capabilities,
                channel_ids: &[],
            },
        )
        .await
        .expect("agent should be created");
}

fn build_schema_for_tenant(
    catalog: &Catalog,
    tenant: &Tenant,
) -> Schema<QueryRoot, MutationRoot, EmptySubscription> {
    Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::new(catalog.agents()) as Arc<dyn AgentRepository>)
    .data(Arc::new(catalog.skills()) as Arc<dyn SkillRepository>)
    .data(Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "provider-injection-test".to_owned(),
        },
    })
    .finish()
}

async fn execute_or_panic(
    schema: &Schema<QueryRoot, MutationRoot, EmptySubscription>,
    op: &str,
) -> Value {
    let response = schema.execute(op).await;
    assert!(
        response.errors.is_empty(),
        "GraphQL op should succeed; errors: {:?}\nop: {op}",
        response.errors
    );
    response.data.into_json().expect("data should be JSON")
}

async fn wait_for_terminal(manager: &TaskManager, task_id: &str, timeout: Duration) -> TaskHandle {
    let start = tokio::time::Instant::now();
    loop {
        let handle = manager
            .get_task(task_id)
            .expect("task should remain visible while polling");
        if handle.state.is_terminal() {
            return handle;
        }
        assert!(
            start.elapsed() < timeout,
            "task {task_id} did not reach a terminal state within {timeout:?}; last state: {:?}",
            handle.state
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_state(
    manager: &TaskManager,
    task_id: &str,
    expected: TaskState,
    timeout: Duration,
) -> TaskHandle {
    let start = tokio::time::Instant::now();
    loop {
        let handle = manager
            .get_task(task_id)
            .expect("task should remain visible while polling");
        if handle.state == expected {
            return handle;
        }
        assert!(
            start.elapsed() < timeout,
            "task {task_id} did not reach state {expected:?} within {timeout:?}; last state: {:?}",
            handle.state
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn scripted_factory(provider: Arc<ScriptedProvider>) -> ProviderFactory {
    Arc::new(move |_kind, _model| {
        Ok(Box::new(SharedProvider(Arc::clone(&provider))) as Box<dyn Provider>)
    })
}

fn assistant_response(content: &str, model: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::EndTurn,
        provider_response_id: None,
        model: model.to_string(),
    }
}

fn tool_call_response(id: &str, name: &str, arguments: Value, model: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::ToolUse,
        provider_response_id: None,
        model: model.to_string(),
    }
}

fn message(role: Role, content: &str) -> Message {
    Message {
        role,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
    }
}

fn panic_message(payload: Box<dyn Any + Send + 'static>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "<non-string panic payload>".to_string(),
        },
    }
}

#[tokio::test]
async fn scripted_provider_returns_scripted_responses_in_order() {
    let provider = ScriptedProvider::new([
        assistant_response("first", "scripted-model"),
        assistant_response("second", "scripted-model"),
    ]);
    let mut budget = ResourceBudget::new(0, 0, Default::default(), 0);

    let first = provider
        .chat(&[message(Role::User, "turn one")], &[], &mut budget)
        .await
        .expect("first scripted response should succeed");
    let second = provider
        .chat(&[message(Role::User, "turn two")], &[], &mut budget)
        .await
        .expect("second scripted response should succeed");

    assert_eq!(first.message.content, "first");
    assert_eq!(second.message.content, "second");
}

#[tokio::test]
async fn scripted_provider_records_each_chat_calls_messages() {
    let provider = ScriptedProvider::new([
        assistant_response("one", "scripted-model"),
        assistant_response("two", "scripted-model"),
    ]);
    let mut budget = ResourceBudget::new(0, 0, Default::default(), 0);

    let first_messages = vec![
        message(Role::System, "system one"),
        message(Role::User, "user one"),
    ];
    let second_messages = vec![
        message(Role::System, "system two"),
        message(Role::User, "user two"),
    ];

    provider
        .chat(&first_messages, &[], &mut budget)
        .await
        .expect("first scripted response should succeed");
    provider
        .chat(&second_messages, &[], &mut budget)
        .await
        .expect("second scripted response should succeed");

    let recorded = provider.recorded_messages();
    assert_eq!(
        recorded.len(),
        2,
        "two chat calls should record two batches"
    );
    let flatten = |batch: &[Message]| -> Vec<(Role, String)> {
        batch
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect()
    };
    assert_eq!(
        flatten(&recorded[0]),
        vec![
            (Role::System, "system one".into()),
            (Role::User, "user one".into()),
        ]
    );
    assert_eq!(
        flatten(&recorded[1]),
        vec![
            (Role::System, "system two".into()),
            (Role::User, "user two".into()),
        ]
    );
}

#[tokio::test]
async fn scripted_provider_panics_when_script_exhausted() {
    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "only",
        "scripted-model",
    )]));
    let provider_for_panic = Arc::clone(&provider);

    let mut budget = ResourceBudget::new(0, 0, Default::default(), 0);
    provider
        .chat(&[message(Role::User, "first")], &[], &mut budget)
        .await
        .expect("first scripted response should succeed");

    let join_error = tokio::spawn(async move {
        let mut budget = ResourceBudget::new(0, 0, Default::default(), 0);
        let _ = provider_for_panic
            .chat(&[message(Role::User, "second")], &[], &mut budget)
            .await;
    })
    .await
    .expect_err("exhausted script should panic");

    assert!(join_error.is_panic(), "script exhaustion should panic");
    let panic = panic_message(join_error.into_panic());
    assert!(
        panic.contains("ScriptedProvider script exhausted"),
        "panic should explain the test bug; got: {panic}"
    );
}

#[tokio::test]
async fn with_provider_factory_sets_the_override_and_is_invoked_on_spawn_task() {
    let _env_guard = env_lock().lock().await;
    let _anthropic = EnvGuard::set("ANTHROPIC_API_KEY", None);

    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent(
        &catalog,
        &tenant,
        "override-agent",
        "Reply done.",
        "claude-3-5-sonnet",
    )
    .await;

    let provider = Arc::new(ScriptedProvider::new([
        tool_call_response(
            "call-echo",
            "echo",
            json!({ "text": "hello" }),
            "claude-3-5-sonnet",
        ),
        assistant_response("done.", "claude-3-5-sonnet"),
    ]));
    let engine =
        build_engine(&catalog).with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "finish once",
            &tenant_config("default", "override-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("provider override should bypass production env var requirements");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded = provider.recorded_messages();
    assert!(
        !recorded.is_empty(),
        "spawn_task should invoke the scripted provider via the override"
    );
}

#[tokio::test]
async fn server_task_consumes_input_response_and_resumes_agent_loop() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent(
        &catalog,
        &tenant,
        "input-agent",
        "Ask for missing details.",
        "claude-3-5-sonnet",
    )
    .await;

    let provider = Arc::new(ScriptedProvider::new([
        tool_call_response(
            "call-input",
            "request_input",
            json!({"prompt": "What account should I use?"}),
            "claude-3-5-sonnet",
        ),
        assistant_response("input handled", "claude-3-5-sonnet"),
    ]));
    let engine =
        build_engine(&catalog).with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "prepare account report",
            &tenant_config("default", "input-agent"),
            None,
            json!({"enable_human_input": true}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    wait_for_state(
        &manager,
        &handle.task_id,
        TaskState::WaitingInput,
        Duration::from_secs(5),
    )
    .await;
    manager
        .provide_input(&handle.task_id, "Use Acme account")
        .expect("input.response should resume the task");

    let terminal = {
        let start = tokio::time::Instant::now();
        loop {
            let current = manager
                .get_task(&handle.task_id)
                .expect("task should remain visible while polling");
            if current.state.is_terminal() {
                break current;
            }
            if start.elapsed() >= Duration::from_secs(5) {
                let (events, _) = manager
                    .subscribe_task(&handle.task_id)
                    .expect("task event history should be readable");
                panic!(
                    "task did not reach terminal state; last={:?}; parent_calls={:?}; events={events:?}",
                    current.state,
                    provider.recorded_messages()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded = provider.recorded_messages();
    let resumed = recorded
        .get(1)
        .expect("provider should be called again after input");
    assert!(resumed.iter().any(|message| {
        message.role == Role::Tool
            && message.tool_call_id.as_deref() == Some("call-input")
            && message.content == "Use Acme account"
    }));
}

#[tokio::test]
async fn server_task_consumes_tool_approval_and_resumes_agent_loop() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent(
        &catalog,
        &tenant,
        "approval-agent",
        "Read the task when approved.",
        "claude-3-5-sonnet",
    )
    .await;

    let provider = Arc::new(ScriptedProvider::new([
        tool_call_response(
            "call-read",
            "file_read",
            json!({"path": "/workspace/task.md"}),
            "claude-3-5-sonnet",
        ),
        assistant_response("approval handled", "claude-3-5-sonnet"),
    ]));
    let engine =
        build_engine(&catalog).with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "approved file read task",
            &tenant_config("default", "approval-agent"),
            None,
            json!({"require_tool_approval": true}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    wait_for_state(
        &manager,
        &handle.task_id,
        TaskState::WaitingApproval,
        Duration::from_secs(5),
    )
    .await;
    manager
        .respond_approval(&handle.task_id, "call-read", true, None)
        .expect("approval.respond should resume the task");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded = provider.recorded_messages();
    let resumed = recorded
        .get(1)
        .expect("provider should be called again after approved tool result");
    assert!(resumed.iter().any(|message| {
        message.role == Role::Tool
            && message.tool_call_id.as_deref() == Some("call-read")
            && message.content.contains("approved file read task")
    }));
}

#[tokio::test]
async fn catalog_spawn_capability_registers_spawn_agent_for_server_runs() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent_with_capabilities(
        &catalog,
        &tenant,
        "dispatcher",
        "Delegate when useful.",
        "claude-3-5-sonnet",
        &["spawn:worker".to_string()],
    )
    .await;

    let mut config = empty_config();
    config.agent_types.insert(
        "worker".to_string(),
        simulacra_config::AgentTypeConfig {
            model: "claude-3-5-sonnet".to_string(),
            system_prompt: Some("Handle delegated work.".to_string()),
            skills: vec![],
            max_turns: Some(2),
            max_tokens: Some(128),
            max_sub_agents: Some(0),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: None,
        },
    );

    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "done.",
        "claude-3-5-sonnet",
    )]));
    let engine = build_engine_with_config(&catalog, config)
        .with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "delegate this",
            &tenant_config("default", "dispatcher"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded_tools = provider.recorded_tools();
    let first_call_tools = recorded_tools
        .first()
        .expect("provider should receive tools");
    let first_call_tool_names = first_call_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "spawn_agent",
        "join_child_agent",
        "cancel_child_agent",
        "steer_child_agent",
        "child_status",
        "wait_child_agent",
        "close_child_agent",
    ] {
        assert!(
            first_call_tool_names.contains(&expected),
            "server-launched agents with spawn capability must receive {expected}; tools were: {first_call_tool_names:?}"
        );
    }
}

#[tokio::test]
async fn server_task_returns_live_generic_subagent_handle_and_resumes_parent() {
    let _env_guard = env_lock().lock().await;
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent_with_capabilities(
        &catalog,
        &tenant,
        "master",
        "Delegate to a specialist when useful, then synthesize the result.",
        "gpt-4o-mini",
        &["spawn:generic".to_string()],
    )
    .await;

    let _openai_key = EnvGuard::set("OPENAI_API_KEY", Some("test-key"));

    let provider = Arc::new(GenericHandoffProvider::new());
    let provider_for_factory = Arc::clone(&provider);
    let engine = build_engine(&catalog).with_provider_factory(Arc::new(move |_kind, _model| {
        Ok(Box::new(SharedGenericHandoffProvider(Arc::clone(
            &provider_for_factory,
        ))) as Box<dyn Provider>)
    }));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "Use a sub-agent for the specialist part and then summarize.",
            &tenant_config("default", "master"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should start");

    let terminal = {
        let start = tokio::time::Instant::now();
        loop {
            let current = manager
                .get_task(&handle.task_id)
                .expect("task should remain visible while polling");
            if current.state.is_terminal() {
                break current;
            }
            if start.elapsed() >= Duration::from_secs(5) {
                let (events, _) = manager
                    .subscribe_task(&handle.task_id)
                    .expect("task event history should be readable");
                panic!(
                    "task did not reach terminal state; last={:?}; parent_calls={:?}; events={events:?}",
                    current.state,
                    provider.recorded_messages()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    assert_eq!(
        terminal.state,
        TaskState::Completed,
        "master task should complete after receiving the live child handle"
    );
    let (events, _) = manager
        .subscribe_task(&handle.task_id)
        .expect("task event history should be readable");
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "agent.child_spawned"),
        "server stream should record the child spawn before child output; events: {events:?}"
    );

    let parent_calls = provider.recorded_messages();
    assert_eq!(
        parent_calls.len(),
        2,
        "master provider should spawn, then synthesize after receiving the live child handle"
    );
    let resumed_messages = parent_calls
        .get(1)
        .expect("second parent call should exist after spawn hand-off");
    assert!(
        resumed_messages.iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some("call-spawn-generic")
                && message.content.contains("\"status\":\"running\"")
        }),
        "master's second turn should include the spawn_agent live handle; messages were: {resumed_messages:?}"
    );
    assert!(
        resumed_messages
            .iter()
            .any(|message| message.content.contains("child-generic-")),
        "live handle should include the generated child id; messages were: {resumed_messages:?}"
    );
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Anthropic network access"]
async fn live_anthropic_server_generic_subagent_handoff_smoke() {
    let _env_guard = env_lock().lock().await;
    if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
        panic!("ANTHROPIC_API_KEY must be set for the live Anthropic handoff smoke");
    }

    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let capabilities = vec!["spawn:generic".to_string()];
    let skill_ids = Vec::new();
    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "master",
                description: Some("live Anthropic handoff smoke"),
                system_prompt: "You are a master orchestration agent. For this task, call spawn_agent exactly once before answering. Use a generic sub-agent with system_prompt \"You are a child validation agent. Return exactly CHILD_OK_7F3 and no other text.\" and task \"Return exactly CHILD_OK_7F3 and no other text.\" After the tool result returns, answer exactly LIVE_HANDOFF_OK: followed by the child message.",
                model: "claude-sonnet-4-20250514",
                max_turns: Some(6),
                max_tokens: Some(4000),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &capabilities,
                channel_ids: &[],
            },
        )
        .await
        .expect("agent should be created");

    let engine = build_engine(&catalog);
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "Use the required generic sub-agent handoff and report the child result.",
            &tenant_config("default", "master"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should start");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(120)).await;
    let (events, _) = manager
        .subscribe_task(&handle.task_id)
        .expect("task event history should be readable");
    assert_eq!(
        terminal.state,
        TaskState::Completed,
        "live Anthropic handoff task should complete; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "tool.called" && event["tool_name"] == "spawn_agent"),
        "parent should call spawn_agent; events: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["event"] == "agent.message"
                && event["child_agent_type"] == "generic"
                && event["content"].as_str() == Some("CHILD_OK_7F3")
        }),
        "server stream should include child-attributed output; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "agent.child_spawned"),
        "server stream should record the child spawn; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "agent.child_finished"),
        "server stream should record the child finish; events: {events:?}"
    );
    let assistant_text = events
        .iter()
        .filter(|event| event["event"] == "agent.message")
        .filter_map(|event| event["content"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        assistant_text.contains("LIVE_HANDOFF_OK") && assistant_text.contains("CHILD_OK_7F3"),
        "parent should resume from the child result; assistant text: {assistant_text:?}; events: {events:?}"
    );
}

#[tokio::test]
async fn server_task_registers_catalog_skill_tool_and_loads_catalog_skill_body() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let skill = create_catalog_skill(
        &catalog,
        &tenant,
        "runbook",
        "Follow the runbook.\nCheck the catalog path.",
    )
    .await;
    create_catalog_agent_with_skills(
        &catalog,
        &tenant,
        "skill-agent",
        "Load skills when useful.",
        "claude-3-5-sonnet",
        std::slice::from_ref(&skill.id),
    )
    .await;

    let provider = Arc::new(ScriptedProvider::new([
        tool_call_response(
            "call-tamper",
            "file_write",
            json!({
                "path": "/skills/runbook/SKILL.md",
                "content": "---\nname: runbook\ndescription: tampered\n---\n\nTampered body."
            }),
            "claude-3-5-sonnet",
        ),
        tool_call_response(
            "call-skill",
            "Skill",
            json!({ "command": "runbook" }),
            "claude-3-5-sonnet",
        ),
        assistant_response("done.", "claude-3-5-sonnet"),
    ]));
    let engine =
        build_engine(&catalog).with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "use the catalog runbook",
            &tenant_config("default", "skill-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded_tools = provider.recorded_tools();
    let first_call_tools = recorded_tools
        .first()
        .expect("provider should receive tools on the first call");
    let skill_tools: Vec<_> = first_call_tools
        .iter()
        .filter(|tool| tool.name == "Skill")
        .collect();
    assert_eq!(
        skill_tools.len(),
        1,
        "catalog-backed agents should receive exactly one Skill tool; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !first_call_tools.iter().any(|tool| tool.name == "runbook"),
        "catalog skills must not be registered as one tool per skill; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );

    let recorded_messages = provider.recorded_messages();
    let second_call_messages = recorded_messages
        .get(1)
        .expect("file_write call should produce a follow-up provider call");
    let tamper_result = second_call_messages
        .iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("call-tamper")
        })
        .expect("follow-up provider call should include the file_write tool result");
    assert!(
        tamper_result.content.contains("ERROR:")
            && tamper_result.content.contains("read-only")
            && tamper_result.content.contains("/skills"),
        "file_write tamper attempt should fail read-only, got: {:?}",
        tamper_result.content
    );

    let third_call_messages = recorded_messages
        .get(2)
        .expect("Skill call should produce a second follow-up provider call");
    let skill_result = third_call_messages
        .iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("call-skill")
        })
        .expect("follow-up provider call should include the Skill tool result");
    assert!(
        skill_result.content.contains("Follow the runbook.")
            && skill_result.content.contains("Check the catalog path."),
        "Skill tool result should contain the catalog skill body, got: {:?}",
        skill_result.content
    );
    assert!(
        !skill_result.content.contains("Tampered body.")
            && !skill_result.content.contains("name: runbook")
            && !skill_result.content.contains("---"),
        "Skill tool result should strip YAML frontmatter, got: {:?}",
        skill_result.content
    );
}

#[tokio::test]
async fn configured_mcp_server_tools_are_registered_for_server_runs() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent_with_capabilities(
        &catalog,
        &tenant,
        "mcp-agent",
        "Use MCP when useful.",
        "claude-3-5-sonnet",
        &["mcp:fetcher".to_string()],
    )
    .await;

    let (mcp_url, mcp_call_count) = spawn_minimal_mcp_server().await;
    let mut config = empty_config();
    config.mcp = Some(McpConfig {
        servers: vec![McpServerConfig {
            name: "fetcher".to_string(),
            transport: Some("http".to_string()),
            url: Some(mcp_url),
            module: None,
            env: None,
            network: vec![],
            wasi: None,
        }],
    });

    let provider = Arc::new(ScriptedProvider::new([
        tool_call_response(
            "call-echo",
            "echo",
            json!({ "text": "hello" }),
            "claude-3-5-sonnet",
        ),
        assistant_response("done.", "claude-3-5-sonnet"),
    ]));
    let engine = build_engine_with_config(&catalog, config)
        .with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "use the mcp fixture",
            &tenant_config("default", "mcp-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded_tools = provider.recorded_tools();
    let first_call_tools = recorded_tools
        .first()
        .expect("provider should receive tools");
    assert!(
        first_call_tools.iter().any(|tool| tool.name == "echo"),
        "server-launched agents with configured MCP must receive discovered MCP tools; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        mcp_call_count.load(Ordering::SeqCst) >= 1,
        "the selected MCP server tool should be invoked through tools/call"
    );
}

#[tokio::test]
async fn configured_mcp_server_tools_are_not_registered_without_agent_mcp_capability() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent_with_capabilities(
        &catalog,
        &tenant,
        "no-mcp-agent",
        "Do not use MCP.",
        "claude-3-5-sonnet",
        &[],
    )
    .await;

    let (mcp_url, mcp_call_count) = spawn_minimal_mcp_server().await;
    let mut config = empty_config();
    config.mcp = Some(McpConfig {
        servers: vec![McpServerConfig {
            name: "fetcher".to_string(),
            transport: Some("http".to_string()),
            url: Some(mcp_url),
            module: None,
            env: None,
            network: vec![],
            wasi: None,
        }],
    });

    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "done.",
        "claude-3-5-sonnet",
    )]));
    let engine = build_engine_with_config(&catalog, config)
        .with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "do not use the mcp fixture",
            &tenant_config("default", "no-mcp-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded_tools = provider.recorded_tools();
    let first_call_tools = recorded_tools
        .first()
        .expect("provider should receive tools");
    assert!(
        !first_call_tools.iter().any(|tool| tool.name == "echo"),
        "agents without MCP capability must not receive discovered MCP tools; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mcp_call_count.load(Ordering::SeqCst),
        0,
        "without an MCP grant, the fixture should never receive tools/call"
    );
}

#[tokio::test]
async fn explicit_mcp_tool_capability_filters_provider_visible_tools() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent_with_capabilities(
        &catalog,
        &tenant,
        "narrow-mcp-agent",
        "Use only the allowed MCP tool.",
        "claude-3-5-sonnet",
        &["mcp:fetcher:echo".to_string()],
    )
    .await;

    let (mcp_url, _mcp_call_count) = spawn_minimal_mcp_server().await;
    let mut config = empty_config();
    config.mcp = Some(McpConfig {
        servers: vec![McpServerConfig {
            name: "fetcher".to_string(),
            transport: Some("http".to_string()),
            url: Some(mcp_url),
            module: None,
            env: None,
            network: vec![],
            wasi: None,
        }],
    });

    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "done.",
        "claude-3-5-sonnet",
    )]));
    let engine = build_engine_with_config(&catalog, config)
        .with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "use only the narrow MCP grant",
            &tenant_config("default", "narrow-mcp-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("server task should spawn");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded_tools = provider.recorded_tools();
    let first_call_tools = recorded_tools
        .first()
        .expect("provider should receive tools");
    assert!(
        first_call_tools.iter().any(|tool| tool.name == "echo"),
        "explicit MCP tool grant should expose the matching tool; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !first_call_tools.iter().any(|tool| tool.name == "delete"),
        "explicit MCP tool grant must hide other server tools; tools were: {:?}",
        first_call_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn without_with_provider_factory_the_engine_retains_existing_env_var_required_behavior() {
    let _env_guard = env_lock().lock().await;
    let _anthropic = EnvGuard::set("ANTHROPIC_API_KEY", None);

    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent(
        &catalog,
        &tenant,
        "no-override-agent",
        "Reply done.",
        "claude-3-5-sonnet",
    )
    .await;

    let engine = build_engine(&catalog);
    let manager = TaskManager::new();

    let error = engine
        .spawn_task(
            &manager,
            "should fail before running",
            &tenant_config("default", "no-override-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect_err("without an override the production env-var validation should remain intact");

    match error {
        EngineError::MissingEnvVar(name) => {
            assert_eq!(name, "ANTHROPIC_API_KEY");
        }
        other => panic!("expected missing env-var error, got: {other:?}"),
    }
}

#[tokio::test]
async fn provider_factory_is_invoked_with_the_resolved_provider_kind_and_model_string() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    create_catalog_agent(
        &catalog,
        &tenant,
        "kind-check-agent",
        "Reply done.",
        "ollama:llama3",
    )
    .await;

    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "done.",
        "ollama:llama3",
    )]));
    let invocations = Arc::new(Mutex::new(Vec::<(ProviderKind, String)>::new()));
    let engine = build_engine(&catalog).with_provider_factory(Arc::new({
        let provider = Arc::clone(&provider);
        let invocations = Arc::clone(&invocations);
        move |kind, model| {
            invocations
                .lock()
                .expect("invocations lock should not be poisoned")
                .push((kind, model.to_string()));
            Ok(Box::new(SharedProvider(Arc::clone(&provider))))
        }
    }));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "finish once",
            &tenant_config("default", "kind-check-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed with a scripted override");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let invocations = invocations
        .lock()
        .expect("invocations lock should not be poisoned")
        .clone();
    assert_eq!(
        invocations,
        vec![(ProviderKind::Ollama, "ollama:llama3".to_string())]
    );
}

#[test]
fn debug_provider_factory_is_set_accessor_tracks_override_installation() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let engine = build_engine(&catalog);
    assert!(
        !engine.debug_provider_factory_is_set(),
        "fresh engines should report no provider override"
    );

    let overridden =
        engine.with_provider_factory(scripted_factory(Arc::new(ScriptedProvider::new(Vec::<
            ProviderResponse,
        >::new(
        )))));
    assert!(
        overridden.debug_provider_factory_is_set(),
        "with_provider_factory should flip the debug accessor"
    );
}

#[tokio::test]
async fn graphql_created_agent_runs_to_completion_under_a_scripted_provider() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let schema = build_schema_for_tenant(&catalog, &tenant);

    let create_agent = r#"
        mutation {
            createAgent(input: {
                name: "graphql-scripted-runner"
                systemPrompt: "Reply done."
                model: "ollama:llama3"
                skillIds: []
                capabilities: []
            }) {
                id
                name
                systemPrompt
                model
            }
        }
    "#;
    let data = execute_or_panic(&schema, create_agent).await;
    assert_eq!(data["createAgent"]["name"], "graphql-scripted-runner");
    assert_eq!(data["createAgent"]["systemPrompt"], "Reply done.");
    assert_eq!(data["createAgent"]["model"], "ollama:llama3");

    let provider = Arc::new(ScriptedProvider::new([assistant_response(
        "done.",
        "ollama:llama3",
    )]));
    let engine =
        build_engine(&catalog).with_provider_factory(scripted_factory(Arc::clone(&provider)));
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "run the GraphQL-authored agent",
            &tenant_config("default", "graphql-scripted-runner"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed for the GraphQL-authored agent");

    let terminal = wait_for_terminal(&manager, &handle.task_id, Duration::from_secs(5)).await;
    assert_eq!(terminal.state, TaskState::Completed);

    let recorded = provider.recorded_messages();
    assert!(
        !recorded.is_empty(),
        "the scripted provider should be consulted at least once"
    );
    assert!(
        recorded
            .iter()
            .flatten()
            .any(|message| message.role == Role::System && message.content.contains("Reply done.")),
        "the GraphQL system prompt should flow into the provider messages; got: {recorded:?}"
    );
}
