use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthProvider, BudgetPoolConfig,
    LocalDiskArtifactStore, SimulacraEngine, TaskManager, TaskState, TenantConfig, TenantResolver,
    build_router,
};
use simulacra_types::{ArtifactStore, VirtualFs};
use simulacra_vfs::{MailboxFs, MemoryFs};

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
    tenants.insert(
        "tenant-b".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-artifact-api-tests".to_string(),
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

async fn state_with_store(
    store: Arc<dyn ArtifactStore>,
) -> (AppState, Arc<TaskManager>, HashMap<String, TenantConfig>) {
    let mut tenants = HashMap::new();
    tenants.insert("tenant-a".to_string(), tenant("tenant-a"));
    tenants.insert("tenant-b".to_string(), tenant("tenant-b"));

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants.clone(), None));
    let auth: Arc<dyn AuthProvider> = Arc::new(ApiKeyAuthProvider::from_entries(vec![
        ApiKeyEntry {
            key: "key-a".to_string(),
            subject: "user-a".to_string(),
            tenant_namespace: Some("tenant-a".to_string()),
            scopes: vec!["tasks:manage".to_string()],
        },
        ApiKeyEntry {
            key: "key-b".to_string(),
            subject: "user-b".to_string(),
            tenant_namespace: Some("tenant-b".to_string()),
            scopes: vec!["tasks:manage".to_string()],
        },
    ]));
    let engine = Arc::new(
        SimulacraEngine::with_components_in_memory_catalog(
            engine_config(),
            None,
            simulacra_server::WorkerPoolConfig::default(),
            store,
        )
        .await
        .unwrap(),
    );
    let state = AppState::with_engine(manager.clone(), resolver, auth, engine);
    (state, manager, tenants)
}

#[tokio::test]
async fn get_artifacts_returns_a_json_envelope_with_artifact_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "summary.md", b"# summary")
        .unwrap();
    store
        .put(
            &handle.task_id,
            "tenant-a",
            "reports/flagged.csv",
            b"id,amount\n1,12000",
        )
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();
    let response = server
        .get(&format!("/api/v1/tasks/{}/artifacts", handle.task_id))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(true));

    let mut paths = body["data"]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap().to_string(),
                entry["size"].as_u64().unwrap(),
                entry["content_type"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    paths.sort();

    assert_eq!(
        paths,
        vec![
            (
                "reports/flagged.csv".to_string(),
                17,
                "text/csv".to_string()
            ),
            ("summary.md".to_string(), 9, "text/markdown".to_string()),
        ]
    );
}

#[tokio::test]
async fn get_single_artifact_returns_raw_bytes_with_content_headers() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "summary.md", b"# summary")
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();
    let response = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/summary.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    response.assert_header("content-type", "text/markdown");
    response.assert_header("content-disposition", "inline; filename=\"summary.md\"");
    assert_eq!(response.as_bytes().as_ref(), b"# summary");
}

#[tokio::test]
async fn artifact_route_errors_use_the_standard_json_envelope() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();

    let unauthenticated = server
        .get(&format!("/api/v1/tasks/{}/artifacts", handle.task_id))
        .await;
    unauthenticated.assert_status(StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = unauthenticated.json();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"]["code"], json!("unauthorized"));

    let missing = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/missing.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    missing.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = missing.json();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"]["code"], json!("not_found"));
}

#[tokio::test]
async fn artifact_routes_enforce_tenant_ownership_with_401_403_and_404() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "summary.md", b"# summary")
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();

    let unauthenticated = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/summary.md",
            handle.task_id
        ))
        .await;
    unauthenticated.assert_status(StatusCode::UNAUTHORIZED);

    let wrong_tenant = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/summary.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-b"),
        )
        .await;
    wrong_tenant.assert_status(StatusCode::FORBIDDEN);
    let body: serde_json::Value = wrong_tenant.json();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"]["code"], json!("forbidden"));

    let missing_task = server
        .get("/api/v1/tasks/does-not-exist/artifacts/summary.md")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    missing_task.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nested_artifact_paths_work_for_downloads() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();
    store
        .put(
            &handle.task_id,
            "tenant-a",
            "reports/q1-summary.md",
            b"q1 summary",
        )
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();
    let response = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/reports/q1-summary.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), b"q1 summary");
}

#[tokio::test]
async fn content_type_is_inferred_from_file_extension() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();

    // Seed artifacts with various extensions
    store
        .put(&handle.task_id, "tenant-a", "data.json", b"{}")
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "report.csv", b"a,b")
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "notes.txt", b"hi")
        .unwrap();
    store
        .put(&handle.task_id, "tenant-a", "unknown.xyz", b"bin")
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();

    let cases = vec![
        ("data.json", "application/json"),
        ("report.csv", "text/csv"),
        ("notes.txt", "text/plain"),
        ("unknown.xyz", "application/octet-stream"),
    ];

    for (path, expected_ct) in cases {
        let response = server
            .get(&format!(
                "/api/v1/tasks/{}/artifacts/{}",
                handle.task_id, path
            ))
            .add_header(
                HeaderName::from_static("authorization"),
                HeaderValue::from_static("ApiKey key-a"),
            )
            .await;
        response.assert_status_ok();
        response.assert_header("content-type", expected_ct);
    }
}

#[tokio::test]
async fn artifacts_are_available_while_the_task_is_still_running() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();
    assert_eq!(handle.state, TaskState::Running);

    store
        .put(&handle.task_id, "tenant-a", "partial.md", b"partial result")
        .unwrap();

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();
    let response = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/partial.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), b"partial result");
}

#[tokio::test]
async fn artifacts_remain_retrievable_after_the_agents_vfs_has_been_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(temp.path()).unwrap());
    let (state, manager, tenants) = state_with_store(store.clone()).await;
    let handle = manager
        .create_task(
            tenants.get("tenant-a").unwrap(),
            "build a report",
            None,
            json!({}),
            None,
        )
        .unwrap();

    {
        let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let mailbox = MailboxFs::new(
            inner,
            handle.task_id.clone(),
            "tenant-a".to_string(),
            store.clone(),
        );
        mailbox
            .write("/proc/mailbox/summary.md", b"persisted after drop")
            .unwrap();
    }

    let server = TestServer::new(build_router(state, vec![], None)).unwrap();
    let response = server
        .get(&format!(
            "/api/v1/tasks/{}/artifacts/summary.md",
            handle.task_id
        ))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;

    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), b"persisted after drop");
}
