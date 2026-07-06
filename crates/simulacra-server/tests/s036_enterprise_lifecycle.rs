use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, extract::Request};
use axum_test::TestServer;
use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, AuthMethod, CapabilitiesConfig, CatalogConfig, IntegrationConfig,
    ProjectConfig, SimulacraConfig, TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_integration::IntegrationRegistry;
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthProvider, BudgetPoolConfig,
    LocalDiskArtifactStore, ProviderFactory, SimulacraEngine, TaskManager, TaskState, TenantConfig,
    TenantResolver, WorkerPoolConfig, build_router,
};
use simulacra_types::{
    ArtifactStore, FinishReason, Message, Provider, ProviderError, ProviderResponse,
    ResourceBudget, Role, TokenUsage, ToolCallMessage, ToolDefinition,
};
use tokio::sync::Mutex as AsyncMutex;

const TOY_TOKEN_ENV: &str = "SIMULACRA_S036_TOY_SAAS_TOKEN";
const TOY_TOKEN: &str = "toy-saas-secret-token-xyz";

static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

fn env_lock() -> &'static AsyncMutex<()> {
    ENV_LOCK.get_or_init(|| AsyncMutex::new(()))
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
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

#[derive(Default)]
struct ToySaasState {
    authed_requests: AtomicU64,
    unauthed_requests: AtomicU64,
}

async fn require_auth(req: Request, next: Next) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let state = req.extensions().get::<Arc<ToySaasState>>().cloned();

    if auth.as_deref() == Some(&format!("Bearer {TOY_TOKEN}")) {
        if let Some(state) = state {
            state.authed_requests.fetch_add(1, Ordering::Relaxed);
        }
        next.run(req).await
    } else {
        if let Some(state) = state {
            state.unauthed_requests.fetch_add(1, Ordering::Relaxed);
        }
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

async fn get_deals() -> impl IntoResponse {
    Json(json!({ "deals": deals_fixture() }))
}

fn deals_fixture() -> Vec<serde_json::Value> {
    let owners = ["alice", "bob", "carol", "dan"];
    let stages = [
        "discovery",
        "proposal",
        "negotiation",
        "closed_won",
        "closed_lost",
    ];

    (0..24u32)
        .map(|i| {
            let month = 1 + (i % 9);
            let day = 1 + (i % 28);
            json!({
                "id": format!("deal-{i:03}"),
                "name": format!("Deal with Customer {}", i + 1),
                "amount": 1_000.0 + (i as f64) * 5_750.0,
                "stage": stages[(i as usize) % stages.len()],
                "close_date": format!("2026-{month:02}-{day:02}"),
                "owner": owners[(i as usize) % owners.len()],
                "last_activity_date": format!("2026-{month:02}-{day:02}"),
                "at_risk": matches!(i, 3 | 9 | 15 | 21),
            })
        })
        .collect()
}

fn toy_saas_router(state: Arc<ToySaasState>) -> Router {
    Router::new()
        .route("/api/deals", get(get_deals))
        .layer(middleware::from_fn(require_auth))
        .layer(axum::Extension(state))
}

async fn start_toy_saas(state: Arc<ToySaasState>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind toy saas listener");
    let addr = listener.local_addr().expect("toy saas local addr");
    let router = toy_saas_router(state);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("toy saas runtime");
        rt.block_on(async move {
            axum::serve(listener, router).await.expect("serve toy saas");
        });
    });
    format!("http://{addr}")
}

fn simulacra_config(base_url: &str) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "ollama:llama3".to_string(),
            acp_profile: None,
            system_prompt: Some("Use the available tools to produce the requested report.".into()),
            skills: vec![],
            max_turns: Some(8),
            max_tokens: Some(8_192),
            max_sub_agents: Some(0),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec!["127.0.0.1".to_string()],
                mcp: vec![],
                shell: false,
                javascript: true,
                python: false,
                paths_read: vec!["/**".to_string()],
                paths_write: vec!["/workspace/**".to_string(), "/proc/mailbox/**".to_string()],
                skill_patterns: vec![],
                memory: None,
            }),
        },
    );

    let mut integrations = HashMap::new();
    integrations.insert(
        "toy-saas".to_string(),
        IntegrationConfig {
            auth: AuthMethod::ApiKey {
                key: TOY_TOKEN_ENV.to_string(),
                placement: "header".to_string(),
            },
            base_url: base_url.to_string(),
            description: Some("Toy SaaS pipeline API".to_string()),
            rate_limit_rps: 0,
            skills_path: None,
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        "tenant-a".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: Some(vec!["toy-saas".to_string()]),
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "s036-enterprise-lifecycle".to_string(),
            description: None,
        },
        agent_types,
        integrations,
        tenants,
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

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "worker".to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec!["toy-saas".to_string()],
        mcp_servers: Default::default(),
    }
}

struct EnterpriseLifecycleProvider {
    calls: Mutex<usize>,
}

impl Provider for EnterpriseLifecycleProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut calls = self
                .calls
                .lock()
                .expect("calls lock should not be poisoned");
            *calls += 1;

            let response = match *calls {
                1 => tool_call(
                    "tc-svc",
                    "file_read",
                    json!({"path": "/svc/toy-saas/config.json"}),
                ),
                2 => {
                    let config = latest_tool_payload(messages);
                    let base_url = config
                        .split("\"base_url\":\"")
                        .nth(1)
                        .and_then(|tail| tail.split('"').next())
                        .expect("provider should discover base_url from /svc config");
                    tool_call(
                        "tc-fetch",
                        "js_exec",
                        json!({
                            "code": format!(
                                "(async () => {{ const r = await fetch('{base_url}/api/deals'); const j = await r.json(); return JSON.stringify(j); }})()"
                            )
                        }),
                    )
                }
                3 => {
                    let deals = latest_tool_payload(messages);
                    assert!(
                        deals.contains("deal-000") && deals.contains("deal-023"),
                        "provider must see the credentialed deal payload before writing report, got: {deals}"
                    );
                    tool_call(
                        "tc-write",
                        "file_write",
                        json!({
                            "path": "/proc/mailbox/pipeline-report.md",
                            "content": "# Pipeline report\n\nFetched 24 deals from toy-saas.\n\nIncludes deal-000 and deal-023.",
                        }),
                    )
                }
                4 => assistant_response("pipeline report complete"),
                other => panic!("unexpected provider chat call #{other}"),
            };

            Ok(response)
        })
    }
}

fn latest_tool_payload(messages: &[Message]) -> String {
    let raw = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .map(|message| message.content.as_str())
        .unwrap_or("");
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
}

struct SharedProvider(Arc<EnterpriseLifecycleProvider>);

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

fn provider_factory(provider: Arc<EnterpriseLifecycleProvider>) -> ProviderFactory {
    Arc::new(move |_kind, _model| {
        Ok(Box::new(SharedProvider(Arc::clone(&provider))) as Box<dyn Provider>)
    })
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ProviderResponse {
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
            provider_content: vec![],
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::ToolUse,
        provider_response_id: None,
        model: "ollama:llama3".to_string(),
    }
}

fn assistant_response(content: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage::default(),
        finish_reason: FinishReason::EndTurn,
        provider_response_id: None,
        model: "ollama:llama3".to_string(),
    }
}

async fn wait_for_terminal(manager: &TaskManager, task_id: &str) -> simulacra_server::TaskHandle {
    let start = tokio::time::Instant::now();
    loop {
        let handle = manager
            .get_task(task_id)
            .expect("task should remain visible while polling");
        if handle.state.is_terminal() {
            return handle;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "task {task_id} did not reach a terminal state; last state: {:?}",
            handle.state
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn toy_saas_task_lifecycle_discovers_fetches_writes_and_retrieves_artifact() {
    let _env_guard_lock = env_lock().lock().await;
    let _token_guard = EnvGuard::set(TOY_TOKEN_ENV, TOY_TOKEN);

    let toy_state = Arc::new(ToySaasState::default());
    let base_url = start_toy_saas(Arc::clone(&toy_state)).await;
    let config = simulacra_config(&base_url);
    let registry = Arc::new(
        IntegrationRegistry::from_config(&config.integrations)
            .expect("integration registry should resolve toy token"),
    );
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let provider = Arc::new(EnterpriseLifecycleProvider {
        calls: Mutex::new(0),
    });
    let engine = Arc::new(
        SimulacraEngine::with_components_in_memory_catalog(
            config,
            Some(registry),
            WorkerPoolConfig::default(),
            store,
        )
        .await
        .unwrap()
        .with_provider_factory(provider_factory(Arc::clone(&provider))),
    );
    let manager = Arc::new(TaskManager::new());

    let mut tenants = HashMap::new();
    tenants.insert("tenant-a".to_string(), tenant("tenant-a"));
    let resolver = Arc::new(TenantResolver::new(tenants, None));
    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "key-a".to_string(),
            subject: "user-a".to_string(),
            tenant_namespace: Some("tenant-a".to_string()),
            scopes: vec!["tasks:manage".to_string()],
        }]));
    let state = AppState::with_engine(Arc::clone(&manager), resolver, auth, engine);
    let server = TestServer::new(build_router(state, vec![], None)).unwrap();

    let create = server
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Discover toy-saas, fetch deals, write a pipeline report artifact."
        }))
        .await;
    create.assert_status_ok();
    let body: serde_json::Value = create.json();
    let task_id = body["data"]["task_id"]
        .as_str()
        .expect("task id should be returned")
        .to_string();

    let terminal = wait_for_terminal(&manager, &task_id).await;
    assert_eq!(terminal.state, TaskState::Completed);
    assert_eq!(
        toy_state.authed_requests.load(Ordering::Relaxed),
        1,
        "fetch should receive platform-injected credentials"
    );
    assert_eq!(
        toy_state.unauthed_requests.load(Ordering::Relaxed),
        0,
        "agent should not need to hardcode an Authorization header"
    );

    let artifact = server
        .get(&format!(
            "/api/v1/tasks/{task_id}/artifacts/pipeline-report.md"
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    artifact.assert_status_ok();
    artifact.assert_header("content-type", "text/markdown");
    let body = artifact.as_bytes();
    let report = std::str::from_utf8(body.as_ref()).unwrap();
    assert!(report.contains("Fetched 24 deals from toy-saas"));
    assert!(report.contains("deal-000"));
    assert!(report.contains("deal-023"));

    assert_eq!(
        *provider
            .calls
            .lock()
            .expect("calls lock should not be poisoned"),
        4,
        "provider should discover, fetch, write, then complete"
    );
}
