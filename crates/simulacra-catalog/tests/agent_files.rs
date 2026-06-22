use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use simulacra_catalog::repo::{AgentFileRepository, AgentRepository, TenantRepository};
use simulacra_catalog::{
    Agent, AgentFile, AgentFileId, AgentFileStore, Catalog, CatalogError, NewAgent, NewAgentFile,
    Tenant,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn fresh() -> Catalog {
    Catalog::open_in_memory().unwrap()
}

fn fresh_with_store(store: Arc<dyn AgentFileStore>) -> Catalog {
    Catalog::open_in_memory_with_agent_file_store(store).unwrap()
}

async fn make_agent(
    catalog: &Catalog,
    tenant_namespace: &str,
    agent_name: &str,
) -> (Tenant, Agent) {
    let tenant = catalog
        .tenants()
        .create(tenant_namespace, Some(tenant_namespace))
        .await
        .unwrap();

    let agent = make_agent_in_tenant(catalog, &tenant, agent_name).await;

    (tenant, agent)
}

async fn make_agent_in_tenant(catalog: &Catalog, tenant: &Tenant, agent_name: &str) -> Agent {
    let skill_ids = [];
    let capabilities: Vec<String> = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: agent_name,
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

fn deterministic_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(((i * 31 + 7) % 251) as u8);
    }
    out
}

fn schema_object_exists(catalog: &Catalog, object_type: &str, name: &str) -> bool {
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();
    guard
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = ?1 AND name = ?2",
            params![object_type, name],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
        .is_some()
}

fn blob_row_count(catalog: &Catalog, file_id: &AgentFileId) -> i64 {
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();
    guard
        .query_row(
            "SELECT COUNT(*) FROM agent_file_bytes WHERE file_id = ?1",
            params![file_id.as_str()],
            |row| row.get(0),
        )
        .unwrap()
}

fn set_file_created_at(catalog: &Catalog, file_id: &AgentFileId, timestamp: &str) {
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();
    guard
        .execute(
            "UPDATE agent_files SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![timestamp, file_id.as_str()],
        )
        .unwrap();
}

#[derive(Debug, Default)]
struct RecordingAgentFileStore {
    bytes: Mutex<HashMap<String, Vec<u8>>>,
    put_calls: Mutex<Vec<(String, Vec<u8>)>>,
    get_calls: Mutex<Vec<String>>,
    delete_calls: Mutex<Vec<String>>,
}

impl RecordingAgentFileStore {
    fn put_calls(&self) -> Vec<(String, Vec<u8>)> {
        self.put_calls.lock().unwrap().clone()
    }

    fn get_calls(&self) -> Vec<String> {
        self.get_calls.lock().unwrap().clone()
    }

    fn delete_calls(&self) -> Vec<String> {
        self.delete_calls.lock().unwrap().clone()
    }

    fn clear_read_delete_recordings(&self) {
        self.get_calls.lock().unwrap().clear();
        self.delete_calls.lock().unwrap().clear();
    }
}

#[async_trait]
impl AgentFileStore for RecordingAgentFileStore {
    async fn put(&self, file_id: &AgentFileId, bytes: &[u8]) -> Result<(), CatalogError> {
        self.bytes
            .lock()
            .unwrap()
            .insert(file_id.as_str().to_owned(), bytes.to_vec());
        self.put_calls
            .lock()
            .unwrap()
            .push((file_id.as_str().to_owned(), bytes.to_vec()));
        Ok(())
    }

    async fn get(&self, file_id: &AgentFileId) -> Result<Vec<u8>, CatalogError> {
        self.get_calls
            .lock()
            .unwrap()
            .push(file_id.as_str().to_owned());
        self.bytes
            .lock()
            .unwrap()
            .get(file_id.as_str())
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!("agent_file bytes id={}", file_id.as_str()))
            })
    }

    async fn delete(&self, file_id: &AgentFileId) -> Result<(), CatalogError> {
        self.delete_calls
            .lock()
            .unwrap()
            .push(file_id.as_str().to_owned());
        self.bytes.lock().unwrap().remove(file_id.as_str());
        Ok(())
    }
}

#[tokio::test]
async fn migration_runs_via_catalog_open_and_creates_agent_files_table() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    assert!(schema_object_exists(&catalog, "table", "agent_files"));
    assert!(schema_object_exists(&catalog, "table", "agent_file_bytes"));
    assert!(schema_object_exists(
        &catalog,
        "index",
        "idx_agent_files_agent"
    ));

    let created = create_file(
        &catalog,
        &tenant,
        &agent,
        "migration-check.pdf",
        "application/pdf",
        b"catalog migration smoke bytes",
    )
    .await;

    let listed = catalog
        .agent_files()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    assert_eq!(blob_row_count(&catalog, &created.id), 1);
}

#[tokio::test]
async fn create_populates_id_and_timestamps_and_stores_bytes_via_store() {
    let store = Arc::new(RecordingAgentFileStore::default());
    let catalog = fresh_with_store(store.clone());
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;
    let bytes = b"hello agent file bytes";

    let created = create_file(
        &catalog,
        &tenant,
        &agent,
        "handbook.txt",
        "text/plain",
        bytes,
    )
    .await;

    assert!(!created.id.as_str().is_empty());
    assert_eq!(created.agent_id, agent.id);
    assert_eq!(created.name, "handbook.txt");
    assert_eq!(created.mime_type, "text/plain");
    assert_eq!(created.size_bytes, bytes.len() as u64);
    assert!(created.created_at <= created.updated_at);

    let put_calls = store.put_calls();
    assert_eq!(put_calls.len(), 1);
    assert_eq!(put_calls[0].0, created.id.as_str());
    assert_eq!(put_calls[0].1, bytes.to_vec());
    assert!(store.get_calls().is_empty());
    assert!(store.delete_calls().is_empty());
}

#[tokio::test]
async fn create_with_duplicate_name_on_same_agent_returns_conflict() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    create_file(
        &catalog,
        &tenant,
        &agent,
        "handbook.pdf",
        "application/pdf",
        b"first version",
    )
    .await;

    let err = catalog
        .agent_files()
        .create(
            &tenant.id,
            NewAgentFile {
                agent_id: &agent.id,
                name: "handbook.pdf",
                mime_type: "application/pdf",
                bytes: b"second version",
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn create_with_invalid_filename_chars_returns_validation() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    let err = catalog
        .agent_files()
        .create(
            &tenant.id,
            NewAgentFile {
                agent_id: &agent.id,
                name: "../escape.txt",
                mime_type: "text/plain",
                bytes: b"nope",
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::Validation(_)));
}

#[tokio::test]
async fn list_for_agent_returns_files_in_created_at_id_order() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    let first = create_file(
        &catalog,
        &tenant,
        &agent,
        "first.pdf",
        "application/pdf",
        b"first",
    )
    .await;
    let second = create_file(
        &catalog,
        &tenant,
        &agent,
        "second.pdf",
        "application/pdf",
        b"second",
    )
    .await;
    let third = create_file(
        &catalog,
        &tenant,
        &agent,
        "third.pdf",
        "application/pdf",
        b"third",
    )
    .await;

    set_file_created_at(&catalog, &first.id, "2024-01-01T00:00:00Z");
    set_file_created_at(&catalog, &second.id, "2024-01-01T00:00:00Z");
    set_file_created_at(&catalog, &third.id, "2024-01-01T00:00:01Z");

    let listed = catalog
        .agent_files()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();

    let mut expected_same_timestamp = [first.id.clone(), second.id.clone()];
    expected_same_timestamp.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].id, expected_same_timestamp[0]);
    assert_eq!(listed[1].id, expected_same_timestamp[1]);
    assert_eq!(listed[2].id, third.id);
}

#[tokio::test]
async fn list_for_agent_filters_to_just_that_agent() {
    let catalog = fresh();
    let (tenant, first_agent) = make_agent(&catalog, "acme", "assistant-a").await;
    let second_agent = make_agent_in_tenant(&catalog, &tenant, "assistant-b").await;

    let first_file = create_file(
        &catalog,
        &tenant,
        &first_agent,
        "only-a.txt",
        "text/plain",
        b"a",
    )
    .await;
    create_file(
        &catalog,
        &tenant,
        &second_agent,
        "only-b.txt",
        "text/plain",
        b"b",
    )
    .await;

    let listed = catalog
        .agent_files()
        .list_for_agent(&tenant.id, &first_agent.id)
        .await
        .unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, first_file.id);
    assert_eq!(listed[0].name, "only-a.txt");
}

#[tokio::test]
async fn read_bytes_returns_original_bytes_verbatim_for_binary_content() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;
    let bytes = [0x00_u8, 0xFF, 0x80, 0x7F, b'A', 0x00, b'Z'];

    let file = create_file(
        &catalog,
        &tenant,
        &agent,
        "binary.dat",
        "application/octet-stream",
        &bytes,
    )
    .await;

    let round_trip = catalog
        .agent_files()
        .read_bytes(&tenant.id, &file.id)
        .await
        .unwrap();

    assert_eq!(round_trip, bytes.to_vec());
}

#[tokio::test]
async fn delete_cascades_to_agent_file_bytes() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    let file = create_file(
        &catalog,
        &tenant,
        &agent,
        "delete-me.bin",
        "application/octet-stream",
        b"delete me",
    )
    .await;

    assert_eq!(blob_row_count(&catalog, &file.id), 1);

    catalog
        .agent_files()
        .delete(&tenant.id, &file.id)
        .await
        .unwrap();

    assert_eq!(blob_row_count(&catalog, &file.id), 0);
    assert!(matches!(
        catalog
            .agent_files()
            .get(&tenant.id, &file.id)
            .await
            .unwrap_err(),
        CatalogError::NotFound(_)
    ));
    assert!(matches!(
        catalog
            .agent_files()
            .read_bytes(&tenant.id, &file.id)
            .await
            .unwrap_err(),
        CatalogError::NotFound(_)
    ));
}

#[tokio::test]
async fn get_returns_full_metadata_without_loading_bytes() {
    let store = Arc::new(RecordingAgentFileStore::default());
    let catalog = fresh_with_store(store.clone());
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    let created = create_file(
        &catalog,
        &tenant,
        &agent,
        "metadata-only.csv",
        "text/csv",
        b"col\nvalue\n",
    )
    .await;

    store.clear_read_delete_recordings();

    let fetched = catalog
        .agent_files()
        .get(&tenant.id, &created.id)
        .await
        .unwrap();

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.agent_id, agent.id);
    assert_eq!(fetched.name, "metadata-only.csv");
    assert_eq!(fetched.mime_type, "text/csv");
    assert_eq!(fetched.size_bytes, b"col\nvalue\n".len() as u64);
    assert_eq!(store.get_calls(), Vec::<String>::new());
    assert_eq!(store.delete_calls(), Vec::<String>::new());
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_id() {
    let catalog = fresh();
    let (tenant, _) = make_agent(&catalog, "acme", "assistant").await;

    let err = catalog
        .agent_files()
        .get(&tenant.id, &AgentFileId::from("missing-agent-file"))
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn tenant_a_cannot_get_tenant_b_agent_file_via_get_or_read_bytes() {
    let catalog = fresh();
    let (tenant_a, _) = make_agent(&catalog, "tenant-a", "assistant-a").await;
    let (tenant_b, agent_b) = make_agent(&catalog, "tenant-b", "assistant-b").await;

    let file_b = create_file(
        &catalog,
        &tenant_b,
        &agent_b,
        "secret.pdf",
        "application/pdf",
        b"tenant-b-secret",
    )
    .await;

    let get_err = catalog
        .agent_files()
        .get(&tenant_a.id, &file_b.id)
        .await
        .unwrap_err();
    assert!(matches!(get_err, CatalogError::NotFound(_)));

    let read_err = catalog
        .agent_files()
        .read_bytes(&tenant_a.id, &file_b.id)
        .await
        .unwrap_err();
    assert!(matches!(read_err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn tenant_a_cannot_list_tenant_b_files_via_list_for_agent() {
    let catalog = fresh();
    let (tenant_a, _) = make_agent(&catalog, "tenant-a", "assistant").await;
    let (tenant_b, agent_b) = make_agent(&catalog, "tenant-b", "assistant").await;

    create_file(
        &catalog,
        &tenant_b,
        &agent_b,
        "secret.pdf",
        "application/pdf",
        b"tenant-b-secret",
    )
    .await;

    let err = catalog
        .agent_files()
        .list_for_agent(&tenant_a.id, &agent_b.id)
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn tenant_a_cannot_delete_tenant_b_file_via_delete() {
    let catalog = fresh();
    let (tenant_a, _) = make_agent(&catalog, "tenant-a", "assistant-a").await;
    let (tenant_b, agent_b) = make_agent(&catalog, "tenant-b", "assistant-b").await;

    let file_b = create_file(
        &catalog,
        &tenant_b,
        &agent_b,
        "secret.pdf",
        "application/pdf",
        b"tenant-b-secret",
    )
    .await;

    let err = catalog
        .agent_files()
        .delete(&tenant_a.id, &file_b.id)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));

    let still_there = catalog
        .agent_files()
        .read_bytes(&tenant_b.id, &file_b.id)
        .await
        .unwrap();
    assert_eq!(still_there, b"tenant-b-secret".to_vec());
}

#[tokio::test]
async fn resolved_agent_carries_files_vec() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    let first = create_file(
        &catalog,
        &tenant,
        &agent,
        "alpha.txt",
        "text/plain",
        b"alpha",
    )
    .await;
    let second = create_file(&catalog, &tenant, &agent, "beta.txt", "text/plain", b"beta").await;

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();

    assert_eq!(resolved.files.len(), 2);
    assert_eq!(resolved.files[0].id, first.id);
    assert_eq!(resolved.files[0].name, "alpha.txt");
    assert_eq!(resolved.files[1].id, second.id);
    assert_eq!(resolved.files[1].name, "beta.txt");
}

#[tokio::test]
async fn resolved_agent_files_filtered_to_correct_tenant() {
    let catalog = fresh();
    let (tenant_a, agent_a) = make_agent(&catalog, "tenant-a", "assistant").await;
    let (tenant_b, agent_b) = make_agent(&catalog, "tenant-b", "assistant").await;

    let file_a = create_file(
        &catalog,
        &tenant_a,
        &agent_a,
        "tenant-a.txt",
        "text/plain",
        b"a",
    )
    .await;
    let file_b = create_file(
        &catalog,
        &tenant_b,
        &agent_b,
        "tenant-b.txt",
        "text/plain",
        b"b",
    )
    .await;

    let resolved_a = catalog
        .agents()
        .resolve(&tenant_a.id, "assistant")
        .await
        .unwrap();
    let resolved_b = catalog
        .agents()
        .resolve(&tenant_b.id, "assistant")
        .await
        .unwrap();

    assert_eq!(resolved_a.files.len(), 1);
    assert_eq!(resolved_a.files[0].id, file_a.id);
    assert_eq!(resolved_a.files[0].name, "tenant-a.txt");

    assert_eq!(resolved_b.files.len(), 1);
    assert_eq!(resolved_b.files[0].id, file_b.id);
    assert_eq!(resolved_b.files[0].name, "tenant-b.txt");
}

#[tokio::test]
async fn catalog_accepts_arbitrary_byte_lengths() {
    let catalog = fresh();
    let (tenant, agent) = make_agent(&catalog, "acme", "assistant").await;

    for len in [0_usize, 1, 7, 4 * 1024, 10 * 1024 * 1024] {
        let bytes = deterministic_bytes(len);
        let name = format!("len-{len}.bin");

        let created = create_file(
            &catalog,
            &tenant,
            &agent,
            &name,
            "application/octet-stream",
            &bytes,
        )
        .await;
        assert_eq!(created.size_bytes, len as u64);

        let round_trip = catalog
            .agent_files()
            .read_bytes(&tenant.id, &created.id)
            .await
            .unwrap();
        assert_eq!(round_trip.len(), len);
        assert_eq!(round_trip, bytes);
    }
}
