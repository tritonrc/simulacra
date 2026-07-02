use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use chrono::DateTime;
use serde_json::Value;
use simulacra_catalog::repo::{AgentFileRepository, AgentRepository, TenantRepository};
use simulacra_catalog::{
    Agent, AgentFile, AgentFileId, AgentFileStore, Catalog, CatalogError, NewAgent, NewAgentFile,
    Tenant,
};
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthProvider, BudgetPoolConfig, SimulacraEngine,
    TaskManager, TenantConfig, TenantResolver, build_router,
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
                skill_patterns: vec![],
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
            name: "simulacra-agent-files-rest-tests".to_string(),
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
        vfs_root: PathBuf::from(format!("/srv/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

#[derive(Debug, Default)]
struct RecordingAgentFileStore {
    bytes: Mutex<HashMap<String, Vec<u8>>>,
}

#[async_trait]
impl AgentFileStore for RecordingAgentFileStore {
    async fn put(&self, file_id: &AgentFileId, bytes: &[u8]) -> Result<(), CatalogError> {
        self.bytes
            .lock()
            .unwrap()
            .insert(file_id.as_str().to_owned(), bytes.to_vec());
        Ok(())
    }

    async fn get(&self, file_id: &AgentFileId) -> Result<Vec<u8>, CatalogError> {
        self.bytes
            .lock()
            .unwrap()
            .get(file_id.as_str())
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(format!("agent_file bytes id={file_id}")))
    }

    async fn delete(&self, file_id: &AgentFileId) -> Result<(), CatalogError> {
        self.bytes.lock().unwrap().remove(file_id.as_str());
        Ok(())
    }
}

struct TestContext {
    server: TestServer,
    catalog: Catalog,
    tenant_a: Tenant,
    tenant_b: Tenant,
    agent_a: Agent,
    agent_b: Agent,
}

async fn make_agent_in_tenant(catalog: &Catalog, tenant: &Tenant, name: &str) -> Agent {
    let skill_ids = [];
    let capabilities: Vec<String> = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("agent under test"),
                system_prompt: "You are a helpful assistant.",
                model: "openai/gpt-oss-120b",
                max_turns: Some(32),
                max_tokens: Some(2048),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &capabilities,
                channel_ids: &[],
            },
        )
        .await
        .unwrap()
}

async fn create_file(
    catalog: &Catalog,
    tenant: &Tenant,
    agent: &Agent,
    name: &str,
    mime_type: &str,
    bytes: &[u8],
) -> AgentFile {
    catalog
        .agent_files()
        .create(
            &tenant.id,
            NewAgentFile {
                agent_id: &agent.id,
                name,
                mime_type,
                bytes,
            },
        )
        .await
        .unwrap()
}

async fn setup() -> TestContext {
    let mut tenants = HashMap::new();
    tenants.insert("tenant-a".to_string(), tenant("tenant-a"));
    tenants.insert("tenant-b".to_string(), tenant("tenant-b"));

    let store: Arc<dyn AgentFileStore> = Arc::new(RecordingAgentFileStore::default());
    let catalog = Catalog::open_in_memory_with_agent_file_store(store).unwrap();
    let tenant_a = catalog
        .tenants()
        .create("tenant-a", Some("tenant-a"))
        .await
        .unwrap();
    let tenant_b = catalog
        .tenants()
        .create("tenant-b", Some("tenant-b"))
        .await
        .unwrap();
    let agent_a = make_agent_in_tenant(&catalog, &tenant_a, "agent-a").await;
    let agent_b = make_agent_in_tenant(&catalog, &tenant_b, "agent-b").await;

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants, None));
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
        SimulacraEngine::new(
            engine_config(),
            None,
            Arc::new(catalog.agents()),
            Arc::new(catalog.skills()),
            Arc::new(catalog.memory_pools()),
            Arc::new(catalog.tenants()),
        )
        .unwrap(),
    );

    let state = AppState::with_engine(manager, resolver, auth, engine)
        .with_agent_files(Arc::new(catalog.agent_files()), catalog.agent_file_store());
    let server = TestServer::new(build_router(state, vec![], None)).unwrap();

    TestContext {
        server,
        catalog,
        tenant_a,
        tenant_b,
        agent_a,
        agent_b,
    }
}

fn auth_header() -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("ApiKey key-a"),
    )
}

fn multipart_body(
    part_name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "simulacra-agent-files-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    match filename {
        Some(filename) => body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{part_name}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        ),
        None => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{part_name}\"\r\n").as_bytes(),
        ),
    }
    if let Some(content_type) = content_type {
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary.to_string(), body)
}

async fn upload(
    server: &TestServer,
    agent_id: &str,
    part_name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) -> axum_test::TestResponse {
    let (boundary, body) = multipart_body(part_name, filename, content_type, bytes);
    let (auth_name, auth_value) = auth_header();
    server
        .post(&format!("/api/v1/agents/{agent_id}/files"))
        .add_header(auth_name, auth_value)
        .add_header(
            HeaderName::from_static("content-type"),
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        )
        .bytes(body.into())
        .await
}

fn assert_rfc3339(value: &Value) {
    DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap();
}

fn assert_looks_like_ulid(value: &str) {
    assert_eq!(value.len(), 26);
    assert!(value.chars().all(|c| c.is_ascii_alphanumeric()));
}

#[tokio::test]
async fn valid_multipart_upload_returns_201_and_agent_file_json_shape() {
    let ctx = setup().await;
    let bytes = b"hello from multipart";

    let response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("handbook.pdf"),
        Some("application/pdf"),
        bytes,
    )
    .await;

    response.assert_status(StatusCode::CREATED);
    let body: Value = response.json();
    assert!(body.get("id").unwrap().is_string());
    assert_eq!(body["agentId"], Value::String(ctx.agent_a.id.to_string()));
    assert_eq!(body["name"], Value::String("handbook.pdf".to_string()));
    assert_eq!(
        body["mimeType"],
        Value::String("application/pdf".to_string())
    );
    assert_eq!(body["sizeBytes"], Value::from(bytes.len() as u64));
    assert_eq!(
        body["downloadUrl"],
        Value::String(format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_a.id,
            body["id"].as_str().unwrap()
        ))
    );
    assert_rfc3339(&body["createdAt"]);
    assert_rfc3339(&body["updatedAt"]);
}

#[tokio::test]
async fn upload_response_id_parses_and_catalog_row_exists_with_expected_size() {
    let ctx = setup().await;
    let bytes = b"catalog-backed upload bytes";

    let response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("notes.txt"),
        Some("text/plain"),
        bytes,
    )
    .await;

    response.assert_status(StatusCode::CREATED);
    let body: Value = response.json();
    let file_id = AgentFileId(body["id"].as_str().unwrap().to_string());
    assert_looks_like_ulid(file_id.as_str());

    let stored = ctx
        .catalog
        .agent_files()
        .get(&ctx.tenant_a.id, &file_id)
        .await
        .unwrap();
    assert_eq!(stored.agent_id, ctx.agent_a.id);
    assert_eq!(stored.name, "notes.txt");
    assert_eq!(stored.size_bytes, bytes.len() as u64);
}

#[tokio::test]
async fn upload_missing_file_part_returns_400() {
    let ctx = setup().await;

    let response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "note",
        None,
        None,
        b"not the file part",
    )
    .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_with_bad_filename_returns_400() {
    let ctx = setup().await;

    let response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("../escape.pdf"),
        Some("application/pdf"),
        b"bad filename",
    )
    .await;

    response.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_to_unknown_agent_in_current_tenant_returns_404() {
    let ctx = setup().await;
    let missing_agent = simulacra_catalog::AgentId::new();
    let seed = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("seed.txt"),
        Some("text/plain"),
        b"seed upload proves the route exists",
    )
    .await;
    seed.assert_status(StatusCode::CREATED);

    let response = upload(
        &ctx.server,
        missing_agent.as_str(),
        "file",
        Some("missing-agent.txt"),
        Some("text/plain"),
        b"unknown agent",
    )
    .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_to_cross_tenant_agent_returns_404() {
    let ctx = setup().await;
    let seed = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("seed.txt"),
        Some("text/plain"),
        b"seed upload proves the route exists",
    )
    .await;
    seed.assert_status(StatusCode::CREATED);

    let response = upload(
        &ctx.server,
        ctx.agent_b.id.as_str(),
        "file",
        Some("foreign.txt"),
        Some("text/plain"),
        b"tenant-a cannot upload here",
    )
    .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_upload_name_returns_409_on_second_upload() {
    let ctx = setup().await;

    let first = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("dupe.csv"),
        Some("text/csv"),
        b"v1",
    )
    .await;
    first.assert_status(StatusCode::CREATED);

    let second = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("dupe.csv"),
        Some("text/csv"),
        b"v2",
    )
    .await;

    second.assert_status(StatusCode::CONFLICT);
}

#[tokio::test]
async fn upload_body_over_50_mib_returns_413_and_stores_nothing() {
    let ctx = setup().await;
    let oversized = vec![0xAB; 50 * 1024 * 1024 + 1];

    let response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("oversized.bin"),
        Some("application/octet-stream"),
        &oversized,
    )
    .await;

    // The handler should enforce MAX_AGENT_FILE_BYTES = 50MB, not rely on axum's default body limit.
    response.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
    let files = ctx
        .catalog
        .agent_files()
        .list_for_agent(&ctx.tenant_a.id, &ctx.agent_a.id)
        .await
        .unwrap();
    assert!(files.is_empty());
}

#[tokio::test]
async fn upload_then_get_download_url_returns_200_content_headers_and_verbatim_binary_bytes() {
    let ctx = setup().await;
    let bytes = vec![0x00, 0x11, 0xFF, 0x7F, b'A', b'Z'];

    let upload_response = upload(
        &ctx.server,
        ctx.agent_a.id.as_str(),
        "file",
        Some("blob.bin"),
        Some("application/octet-stream"),
        &bytes,
    )
    .await;

    upload_response.assert_status(StatusCode::CREATED);
    let uploaded: Value = upload_response.json();
    let (auth_name, auth_value) = auth_header();
    let download_response = ctx
        .server
        .get(uploaded["downloadUrl"].as_str().unwrap())
        .add_header(auth_name, auth_value)
        .await;

    download_response.assert_status(StatusCode::OK);
    download_response.assert_header("content-type", "application/octet-stream");
    download_response.assert_header("content-length", &bytes.len().to_string());
    assert_eq!(download_response.as_bytes().as_ref(), bytes.as_slice());
}

#[tokio::test]
async fn download_unknown_file_id_returns_404() {
    let ctx = setup().await;
    let file = create_file(
        &ctx.catalog,
        &ctx.tenant_a,
        &ctx.agent_a,
        "known.txt",
        "text/plain",
        b"known bytes",
    )
    .await;
    let (auth_name, auth_value) = auth_header();
    let known = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_a.id, file.id
        ))
        .add_header(auth_name.clone(), auth_value.clone())
        .await;
    known.assert_status(StatusCode::OK);

    let response = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_a.id,
            AgentFileId::new()
        ))
        .add_header(auth_name, auth_value)
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn download_cross_tenant_file_id_returns_404() {
    let ctx = setup().await;
    let local_file = create_file(
        &ctx.catalog,
        &ctx.tenant_a,
        &ctx.agent_a,
        "tenant-a-only.pdf",
        "application/pdf",
        b"local bytes",
    )
    .await;
    let foreign_file = create_file(
        &ctx.catalog,
        &ctx.tenant_b,
        &ctx.agent_b,
        "tenant-b-only.pdf",
        "application/pdf",
        b"foreign bytes",
    )
    .await;
    let (auth_name, auth_value) = auth_header();
    let known = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_a.id, local_file.id
        ))
        .add_header(auth_name.clone(), auth_value.clone())
        .await;
    known.assert_status(StatusCode::OK);

    let response = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_b.id, foreign_file.id
        ))
        .add_header(auth_name, auth_value)
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn download_with_mismatched_agent_id_and_valid_file_id_returns_404() {
    let ctx = setup().await;
    let sibling_agent = make_agent_in_tenant(&ctx.catalog, &ctx.tenant_a, "agent-a-sibling").await;
    let file = create_file(
        &ctx.catalog,
        &ctx.tenant_a,
        &ctx.agent_a,
        "shared-name.txt",
        "text/plain",
        b"owned by agent-a only",
    )
    .await;
    let (auth_name, auth_value) = auth_header();
    let known = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            ctx.agent_a.id, file.id
        ))
        .add_header(auth_name.clone(), auth_value.clone())
        .await;
    known.assert_status(StatusCode::OK);

    let response = ctx
        .server
        .get(&format!(
            "/api/v1/agents/{}/files/{}/bytes",
            sibling_agent.id, file.id
        ))
        .add_header(auth_name, auth_value)
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
}
