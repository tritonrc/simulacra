use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::server::CreateTaskRequest;
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthProvider, BudgetPoolConfig, FileAttachment,
    LocalDiskArtifactStore, ProviderFactory, SimulacraEngine, TaskManager, TaskState, TenantConfig,
    TenantResolver, build_router,
};
use simulacra_types::{
    ArtifactStore, FinishReason, Message, Provider, ProviderError, ProviderResponse,
    ResourceBudget, Role, TokenUsage, ToolCallMessage, ToolDefinition, VirtualFs,
};

fn engine_config() -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            model: "ollama:llama3".to_string(),
            system_prompt: Some("You are the worker.".to_string()),
            skills: vec![],
            max_turns: Some(12),
            max_tokens: Some(8_192),
            max_sub_agents: Some(0),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec!["/**".to_string()],
                paths_write: vec!["/workspace/**".to_string(), "/proc/mailbox/**".to_string()],

                memory: None,
            }),
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        "tenant-a".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-file-attachment-tests".to_string(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
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
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

async fn server() -> TestServer {
    let mut tenants = HashMap::new();
    tenants.insert("tenant-a".to_string(), tenant("tenant-a"));

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants, None));
    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "key-a".to_string(),
            subject: "user-a".to_string(),
            tenant_namespace: Some("tenant-a".to_string()),
            scopes: vec!["tasks:manage".to_string()],
        }]));
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(engine_config(), None)
            .await
            .unwrap(),
    );
    let state = AppState::with_engine(manager, resolver, auth, engine);
    TestServer::new(build_router(state, vec![], None)).unwrap()
}

struct AttachmentRoundTripProvider {
    calls: Mutex<usize>,
}

impl Provider for AttachmentRoundTripProvider {
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
                1 => tool_call_response(
                    "tc-read",
                    "file_read",
                    json!({"path": "/workspace/expenses.csv"}),
                ),
                2 => {
                    let csv = messages
                        .iter()
                        .rev()
                        .find(|message| message.role == Role::Tool)
                        .map(|message| message.content.as_str())
                        .unwrap_or("");
                    assert!(
                        csv.contains("Acme,12500"),
                        "second provider turn must see the attached CSV read result, got: {csv}"
                    );
                    let report = format!(
                        "# Expense report\n\nProcessed attached CSV.\n\nInput contained Acme,12500: {}",
                        csv.contains("Acme,12500")
                    );
                    tool_call_response(
                        "tc-write",
                        "file_write",
                        json!({
                            "path": "/proc/mailbox/expense-report.md",
                            "content": report,
                        }),
                    )
                }
                3 => assistant_response("artifact ready"),
                other => panic!("unexpected provider chat call #{other}"),
            };

            Ok(response)
        })
    }
}

struct SharedAttachmentRoundTripProvider(Arc<AttachmentRoundTripProvider>);

impl Provider for SharedAttachmentRoundTripProvider {
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

fn attachment_round_trip_factory(provider: Arc<AttachmentRoundTripProvider>) -> ProviderFactory {
    Arc::new(move |_kind, _model| {
        Ok(Box::new(SharedAttachmentRoundTripProvider(Arc::clone(&provider))) as Box<dyn Provider>)
    })
}

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> ProviderResponse {
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

#[test]
fn create_task_request_accepts_an_optional_files_map_of_file_attachments() {
    let body = json!({
        "task": "Categorize expenses",
        "files": {
            "expenses.csv": {
                "data": "vendor,amount\nAcme,12500"
            }
        }
    });

    let request: CreateTaskRequest = serde_json::from_value(body).unwrap();

    assert!(
        request.files.is_some(),
        "CreateTaskRequest must expose files: Option<HashMap<...>>"
    );
    assert!(request.files.unwrap().contains_key("expenses.csv"));
}

#[tokio::test]
async fn utf8_attachments_are_seeded_into_workspace_before_the_agent_loop_starts() {
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(engine_config(), None)
            .await
            .unwrap(),
    );
    let manager = TaskManager::new();
    let attachments = HashMap::from([(
        "expenses.csv".to_string(),
        FileAttachment {
            data: "vendor,amount\nAcme,12500".to_string(),
            encoding: None,
        },
    )]);

    let handle = engine
        .spawn_task(
            &manager,
            "Categorize these expenses",
            &tenant("tenant-a"),
            None,
            json!({}),
            Some(attachments),
            None,
        )
        .await
        .unwrap();

    let seeded = engine.debug_workspace_snapshot(&handle.task_id).unwrap();
    assert_eq!(
        seeded.read("/workspace/expenses.csv").unwrap(),
        b"vendor,amount\nAcme,12500"
    );
}

#[tokio::test]
async fn base64_attachments_are_decoded_and_seeded_into_workspace_before_the_agent_loop_starts() {
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(engine_config(), None)
            .await
            .unwrap(),
    );
    let manager = TaskManager::new();
    let attachments = HashMap::from([(
        "logo.txt".to_string(),
        FileAttachment {
            data: "aGVsbG8gd29ybGQ=".to_string(),
            encoding: Some("base64".to_string()),
        },
    )]);

    let handle = engine
        .spawn_task(
            &manager,
            "Read the attached file",
            &tenant("tenant-a"),
            None,
            json!({}),
            Some(attachments),
            None,
        )
        .await
        .unwrap();

    let seeded = engine.debug_workspace_snapshot(&handle.task_id).unwrap();
    assert_eq!(seeded.read("/workspace/logo.txt").unwrap(), b"hello world");
}

#[tokio::test]
async fn nested_filenames_seed_workspace_and_create_parent_directories() {
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(engine_config(), None)
            .await
            .unwrap(),
    );
    let manager = TaskManager::new();
    let attachments = HashMap::from([(
        "reports/q1.csv".to_string(),
        FileAttachment {
            data: "id,amount\n1,9000".to_string(),
            encoding: None,
        },
    )]);

    let handle = engine
        .spawn_task(
            &manager,
            "Summarize the report",
            &tenant("tenant-a"),
            None,
            json!({}),
            Some(attachments),
            None,
        )
        .await
        .unwrap();

    let seeded = engine.debug_workspace_snapshot(&handle.task_id).unwrap();
    let mut entries = seeded.list_dir("/workspace").unwrap();
    entries.sort();
    assert_eq!(entries, vec!["reports", "task.md"]);
    assert_eq!(
        seeded.read("/workspace/reports/q1.csv").unwrap(),
        b"id,amount\n1,9000"
    );
}

#[tokio::test]
async fn invalid_parent_escape_filename_is_rejected_synchronously_with_400() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {
                "../escape.csv": {
                    "data": "x"
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn absolute_filenames_are_rejected_synchronously_with_400() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {
                "/tmp/escape.csv": {
                    "data": "x"
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_filenames_are_rejected_synchronously_with_400() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {
                "": {
                    "data": "x"
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn single_attachment_over_10_mb_returns_413_based_on_decoded_size() {
    let oversized = "A".repeat(10 * 1024 * 1024 + 1);
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {
                "oversized.txt": {
                    "data": oversized
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn total_attachment_bytes_over_50_mb_return_413() {
    let large = "A".repeat(26 * 1024 * 1024);
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {
                "part-1.txt": {
                    "data": large
                },
                "part-2.txt": {
                    "data": "B".repeat(26 * 1024 * 1024)
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn invalid_base64_encoding_is_rejected_synchronously_with_400() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Read this file",
            "files": {
                "data.bin": {
                    "data": "not valid base64!!!@@@",
                    "encoding": "base64"
                }
            }
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn exactly_10_mb_attachment_is_accepted() {
    let exactly_10mb = "A".repeat(10 * 1024 * 1024);
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Process this file",
            "files": {
                "big.txt": {
                    "data": exactly_10mb
                }
            }
        }))
        .await;

    // Should succeed — 10 MB is the limit, not 10 MB - 1
    response.assert_status_ok();
}

#[tokio::test]
async fn omitted_files_field_behaves_like_the_existing_task_create_flow() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses"
        }))
        .await;

    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(true));
    assert!(body["data"]["task_id"].is_string());
}

#[tokio::test]
async fn empty_files_map_behaves_like_the_existing_task_create_flow() {
    let response = server()
        .await
        .post("/api/v1/tasks/create")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .json(&json!({
            "task": "Categorize expenses",
            "files": {}
        }))
        .await;

    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(true));
    assert!(body["data"]["task_id"].is_string());
}

#[tokio::test]
async fn task_create_with_attached_csv_round_trips_to_retrievable_mailbox_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let provider = Arc::new(AttachmentRoundTripProvider {
        calls: Mutex::new(0),
    });
    let engine = Arc::new(
        SimulacraEngine::with_components_in_memory_catalog(
            engine_config(),
            None,
            simulacra_server::WorkerPoolConfig::default(),
            Arc::clone(&store),
        )
        .await
        .unwrap()
        .with_provider_factory(attachment_round_trip_factory(Arc::clone(&provider))),
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
            "task": "Read expenses.csv and write a report artifact.",
            "files": {
                "expenses.csv": {
                    "data": "vendor,amount\nAcme,12500\nBeta,40"
                }
            }
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

    let list = server
        .get(&format!("/api/v1/tasks/{task_id}/artifacts"))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    list.assert_status_ok();
    let list_body: serde_json::Value = list.json();
    assert_eq!(list_body["ok"], json!(true));
    assert_eq!(
        list_body["data"]["artifacts"][0]["path"],
        json!("expense-report.md")
    );
    assert!(list_body["data"]["artifacts"][0]["size"].as_u64().unwrap() > 0);

    let artifact = server
        .get(&format!(
            "/api/v1/tasks/{task_id}/artifacts/expense-report.md"
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    artifact.assert_status_ok();
    artifact.assert_header("content-type", "text/markdown");
    let bytes = artifact.as_bytes();
    let report = std::str::from_utf8(bytes.as_ref()).unwrap();
    assert!(report.contains("Processed attached CSV"));
    assert!(report.contains("Input contained Acme,12500: true"));

    assert_eq!(
        *provider
            .calls
            .lock()
            .expect("calls lock should not be poisoned"),
        3,
        "provider should have read, written, then completed"
    );
}
