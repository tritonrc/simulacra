//! S037 Wave C — admin ingestion endpoint (`POST /api/v1/ingestion`) tests.
//!
//! These cover the shape and behavior of the HTTP endpoint: auth/tenant
//! resolution, source name validation, mode semantics (merge vs replace),
//! and the 404 response when memory is not configured on the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use base64::Engine as _;
use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_memory::{
    DefaultEmbedder, Embedder, MemoryStore, SqliteMemoryStore, SqliteVectorIndex, VectorIndex,
};
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthProvider, BudgetPoolConfig,
    LocalDiskArtifactStore, SimulacraEngine, TaskManager, TenantConfig, TenantResolver,
    build_router,
};
use simulacra_types::{ArtifactStore, MemoryPath, TenantId};

fn engine_config() -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "ollama:llama3".to_string(),
            acp_profile: None,
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
                paths_write: vec!["/workspace/**".to_string()],

                skill_patterns: vec![],

                memory: None,
            }),
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        "acme".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-ingestion-tests".to_string(),
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

fn server_tenant(namespace: &str) -> TenantConfig {
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

struct TestRig {
    _tmp: tempfile::TempDir,
    memory_store: Arc<dyn MemoryStore>,
    _vector_index: Arc<dyn VectorIndex>,
    state: AppState,
    tenant: TenantId,
}

async fn build_rig(with_memory: bool) -> TestRig {
    let tmp = tempfile::tempdir().unwrap();

    let mut tenants = HashMap::new();
    tenants.insert("acme".to_string(), server_tenant("acme"));
    let resolver = Arc::new(TenantResolver::new(tenants, None));

    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "key-acme".to_string(),
            subject: "user-acme".to_string(),
            tenant_namespace: Some("acme".to_string()),
            scopes: vec!["tasks:manage".to_string()],
        }]));

    let store: Arc<dyn ArtifactStore> = Arc::new(LocalDiskArtifactStore::new(tmp.path()).unwrap());
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
    let manager = Arc::new(TaskManager::new());

    let embedder: Arc<dyn Embedder> = Arc::new(DefaultEmbedder::load_default().unwrap());
    let memory_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let vector_index: Arc<dyn VectorIndex> =
        Arc::new(SqliteVectorIndex::new(tmp.path(), embedder.id().clone()).unwrap());

    let state = if with_memory {
        AppState::with_memory(
            manager,
            resolver,
            auth,
            engine,
            Arc::clone(&memory_store),
            Arc::clone(&vector_index),
            Arc::clone(&embedder),
        )
    } else {
        AppState::with_engine(manager, resolver, auth, engine)
    };

    TestRig {
        _tmp: tmp,
        memory_store,
        _vector_index: vector_index,
        state,
        tenant: TenantId::parse("acme").unwrap(),
    }
}

fn auth_header() -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("ApiKey key-acme"),
    )
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

#[tokio::test]
async fn ingestion_writes_files_under_mnt_source() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "pto.md", "content": b64("PTO: unlimited")},
                {"path": "remote.md", "content": b64("Remote work: allowed")}
            ]
        }))
        .await;

    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(true));

    let pto = MemoryPath::parse("/mnt/hr-policies/pto.md").unwrap();
    let (content, _) = rig.memory_store.get(&rig.tenant, &pto).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "PTO: unlimited");

    let remote = MemoryPath::parse("/mnt/hr-policies/remote.md").unwrap();
    let (content, _) = rig.memory_store.get(&rig.tenant, &remote).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "Remote work: allowed");
}

#[tokio::test]
async fn ingestion_rejects_invalid_source_name() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "bad source!",
            "files": []
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["code"], json!("invalid_source"));
}

#[tokio::test]
async fn ingestion_rejects_unknown_mode() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "mode": "weird",
            "files": []
        }))
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["code"], json!("invalid_mode"));
}

#[tokio::test]
async fn ingestion_replace_mode_deletes_prefix_first() {
    let rig = build_rig(true).await;

    // Seed an existing file inside /mnt/hr/ so we can verify it is gone
    // after a replace.
    let existing = MemoryPath::parse("/mnt/hr/old.md").unwrap();
    rig.memory_store
        .put(&rig.tenant, &existing, b"stale policy")
        .unwrap();

    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "hr",
            "mode": "replace",
            "files": [
                {"path": "new.md", "content": b64("new policy")}
            ]
        }))
        .await;

    response.assert_status_ok();

    // old.md must be gone.
    let old_exists = rig.memory_store.exists(&rig.tenant, &existing).unwrap();
    assert!(!old_exists, "replace mode should have deleted old.md");

    // new.md must be present.
    let new_path = MemoryPath::parse("/mnt/hr/new.md").unwrap();
    let (content, _) = rig.memory_store.get(&rig.tenant, &new_path).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "new policy");
}

#[tokio::test]
async fn ingestion_merge_mode_preserves_existing() {
    let rig = build_rig(true).await;

    let existing = MemoryPath::parse("/mnt/hr/old.md").unwrap();
    rig.memory_store
        .put(&rig.tenant, &existing, b"stable policy")
        .unwrap();

    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "hr",
            "mode": "merge",
            "files": [
                {"path": "addition.md", "content": b64("additional policy")}
            ]
        }))
        .await;

    response.assert_status_ok();

    // old.md must still be present.
    let (content, _) = rig.memory_store.get(&rig.tenant, &existing).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "stable policy");

    // addition.md must be present.
    let add_path = MemoryPath::parse("/mnt/hr/addition.md").unwrap();
    let (content, _) = rig.memory_store.get(&rig.tenant, &add_path).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "additional policy");
}

#[tokio::test]
async fn ingestion_requires_authenticated_tenant() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();

    let response = server
        .post("/api/v1/ingestion")
        .json(&json!({
            "source": "hr-policies",
            "files": []
        }))
        .await;

    response.assert_status(StatusCode::UNAUTHORIZED);
}

/// S037 assertion 1167: `/api/v1/ingestion/stream` emits a task-like
/// SSE event sequence (`ingestion.started` → per-file `ingestion.written`
/// → `ingestion.completed`) so operators can audit ingestion in real time,
/// mirroring the envelope of `/api/v1/tasks/:task_id/events`.
#[tokio::test]
async fn ingestion_stream_emits_per_file_progress_events() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "pto.md", "content": b64("PTO: unlimited")},
                {"path": "remote.md", "content": b64("Remote work: allowed")}
            ]
        }))
        .await;

    response.assert_status_ok();
    let content_type = response
        .header("content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "streaming ingestion must return Content-Type: text/event-stream, got {content_type:?}",
    );

    let body = response.text();
    let json_events = parse_sse_events(&body);

    assert!(
        json_events.len() >= 4,
        "expected at least started + 2 written + completed, got {json_events:?}"
    );

    // Every event must share the same ingestion_id (a non-empty UUID).
    let ingestion_id = json_events[0]["ingestion_id"]
        .as_str()
        .expect("started event must carry an ingestion_id")
        .to_string();
    assert!(
        !ingestion_id.is_empty(),
        "ingestion_id must be a non-empty string"
    );
    for e in &json_events {
        assert_eq!(
            e["ingestion_id"],
            json!(ingestion_id),
            "every event must carry the same ingestion_id: {e:?}"
        );
    }

    // seq must be a monotonically increasing counter starting at 1.
    for (i, e) in json_events.iter().enumerate() {
        let expected_seq = (i as u64) + 1;
        assert_eq!(
            e["seq"],
            json!(expected_seq),
            "event at index {i} must have seq={expected_seq}: {e:?}"
        );
    }

    assert_eq!(json_events[0]["event"], json!("ingestion.started"));
    assert_eq!(json_events[0]["source"], json!("hr-policies"));
    assert_eq!(json_events[0]["mode"], json!("merge"));
    assert_eq!(json_events[0]["file_count"], json!(2));

    let written: Vec<&serde_json::Value> = json_events
        .iter()
        .filter(|e| e["event"] == json!("ingestion.written"))
        .collect();
    assert_eq!(written.len(), 2);
    assert!(
        written
            .iter()
            .any(|e| e["path"] == json!("/mnt/hr-policies/pto.md")),
        "missing written event for pto.md: {written:?}"
    );
    assert!(
        written
            .iter()
            .any(|e| e["path"] == json!("/mnt/hr-policies/remote.md")),
        "missing written event for remote.md: {written:?}"
    );

    let last = json_events.last().unwrap();
    assert_eq!(last["event"], json!("ingestion.completed"));
    assert_eq!(last["count"], json!(2));

    // Real memory writes must have landed.
    let pto = MemoryPath::parse("/mnt/hr-policies/pto.md").unwrap();
    let (content, _) = rig.memory_store.get(&rig.tenant, &pto).unwrap();
    assert_eq!(String::from_utf8(content).unwrap(), "PTO: unlimited");
}

/// Replace-mode ingestion emits an `ingestion.cleared` event before the
/// `ingestion.written` events so an observer can see both phases.
#[tokio::test]
async fn ingestion_stream_replace_mode_emits_cleared_event_before_writes() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    // Seed an old file so replace mode has something to clear.
    let old_path = MemoryPath::parse("/mnt/hr-policies/old.md").unwrap();
    rig.memory_store
        .put(&rig.tenant, &old_path, b"obsolete policy")
        .unwrap();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "mode": "replace",
            "files": [
                {"path": "new.md", "content": b64("New policy")}
            ]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();
    let json_events = parse_sse_events(&body);

    let cleared_idx = json_events
        .iter()
        .position(|e| e["event"] == json!("ingestion.cleared"))
        .expect("replace mode must emit an ingestion.cleared event");
    let first_written_idx = json_events
        .iter()
        .position(|e| e["event"] == json!("ingestion.written"))
        .expect("at least one ingestion.written event expected");
    assert!(
        cleared_idx < first_written_idx,
        "cleared event must precede written events"
    );
    assert_eq!(
        json_events[cleared_idx]["prefix"],
        json!("/mnt/hr-policies")
    );

    // The cleared event must carry the same ingestion_id as the surrounding
    // events and a strictly-ordered seq.
    let ingestion_id = json_events[0]["ingestion_id"].as_str().unwrap().to_string();
    assert_eq!(
        json_events[cleared_idx]["ingestion_id"],
        json!(ingestion_id)
    );
    assert_eq!(json_events[cleared_idx]["seq"], json!(2));
}

/// Invalid base64 in one file must abort before any destructive write:
/// `delete_prefix` must NOT run, no files may be written, and the terminal
/// event must be a synchronous HTTP error (no SSE stream opened).
#[tokio::test]
async fn ingestion_stream_rejects_invalid_base64_before_opening_stream() {
    let rig = build_rig(true).await;

    // Seed a file under the target prefix. In replace mode the naive
    // implementation would have called delete_prefix before discovering the
    // bad base64 — if this file is still present after the request, the
    // pre-validation pass did its job.
    let seed_path = MemoryPath::parse("/mnt/hr-policies/seed.md").unwrap();
    rig.memory_store
        .put(&rig.tenant, &seed_path, b"seed content")
        .unwrap();

    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "mode": "replace",
            "files": [
                {"path": "ok.md", "content": b64("good payload")},
                {"path": "bad.md", "content": "!!!not-valid-base64!!!"}
            ]
        }))
        .await;

    // Pre-validation happens before the stream is opened, so we expect a
    // synchronous 400 — not a 200 with an SSE error event.
    response.assert_status(StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"]["code"], json!("invalid_base64"));

    // The seeded file must still be present: delete_prefix must not have run.
    let (content, _) = rig.memory_store.get(&rig.tenant, &seed_path).unwrap();
    assert_eq!(
        String::from_utf8(content).unwrap(),
        "seed content",
        "replace-mode delete_prefix must not run when pre-validation fails",
    );

    // No files from the request may have been written.
    let ok_path = MemoryPath::parse("/mnt/hr-policies/ok.md").unwrap();
    assert!(
        !rig.memory_store.exists(&rig.tenant, &ok_path).unwrap(),
        "no files may be written when pre-validation fails"
    );
}

/// Malformed file paths (parent escape, absolute) must fail pre-validation:
/// synchronous HTTP 400 with `invalid_file_path`, no SSE stream, no writes.
#[tokio::test]
async fn ingestion_stream_rejects_invalid_file_path_before_opening_stream() {
    let rig = build_rig(true).await;

    // Seed a file under the target prefix. In replace mode the naive
    // implementation would have called delete_prefix before discovering the
    // bad path — if this file is still present after the request, the
    // pre-validation pass did its job.
    let seed_path = MemoryPath::parse("/mnt/hr-policies/seed.md").unwrap();
    rig.memory_store
        .put(&rig.tenant, &seed_path, b"seed content")
        .unwrap();

    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "mode": "replace",
            "files": [
                {"path": "ok.md", "content": b64("good payload")},
                {"path": "../secret.md", "content": b64("escape attempt")}
            ]
        }))
        .await;

    // Pre-validation happens before the stream is opened, so we expect a
    // synchronous 400 — not a 200 with an SSE error event.
    response.assert_status(StatusCode::BAD_REQUEST);
    let content_type = response
        .header("content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        !content_type.starts_with("text/event-stream"),
        "pre-validation failure must NOT open an SSE stream, got content-type: {content_type:?}"
    );

    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"]["code"], json!("invalid_file_path"));

    // The seeded file must still be present: delete_prefix must not have run.
    let (content, _) = rig.memory_store.get(&rig.tenant, &seed_path).unwrap();
    assert_eq!(
        String::from_utf8(content).unwrap(),
        "seed content",
        "replace-mode delete_prefix must not run when pre-validation fails",
    );

    // No files from the request may have been written.
    let ok_path = MemoryPath::parse("/mnt/hr-policies/ok.md").unwrap();
    assert!(
        !rig.memory_store.exists(&rig.tenant, &ok_path).unwrap(),
        "no files may be written when pre-validation fails"
    );
}

/// Worker completeness smoke-test for the SSE ingest path. The worker must
/// drive every `put` to completion regardless of what the observer does with
/// the event stream; the channel is explicitly best-effort. A true
/// mid-stream TCP disconnect would be the most direct proof, but
/// `axum-test::TestServer` materializes the entire response body before
/// returning from `.await` — there is no opportunity to drop the underlying
/// HTTP connection mid-body. A proper disconnect test would need a lower-
/// level HTTP client (`hyper::client::conn`) driven against a bound
/// `TcpListener`; that is out of scope here.
///
/// What this test DOES verify: the worker does not gate file writes on the
/// observer consuming events. All five files land in the store and the
/// terminal `ingestion.completed` event reports the full count, which
/// exercises the same code path a disconnect would take (the worker keeps
/// looping over `validated` regardless of whether `emit_ingest_event`
/// observed a closed channel).
#[tokio::test]
async fn ingestion_stream_completes_ingest_even_when_client_reads_nothing() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state.clone(), vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "a.md", "content": b64("content a")},
                {"path": "b.md", "content": b64("content b")},
                {"path": "c.md", "content": b64("content c")},
                {"path": "d.md", "content": b64("content d")},
                {"path": "e.md", "content": b64("content e")},
            ]
        }))
        .await;

    response.assert_status_ok();

    // Drop the response without inspecting the body. Even if axum-test has
    // already buffered every event, the worker must have completed all five
    // puts — the writes cannot be gated on an observer.
    drop(response);

    // Poll the store. axum-test awaits the worker's full lifecycle (the SSE
    // stream only closes after `ingestion.completed`), so in practice the
    // writes are visible immediately; the poll is a small tolerance for the
    // spawn_blocking handoff and any SQLite commit latency.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let all_present = ["a.md", "b.md", "c.md", "d.md", "e.md"].iter().all(|name| {
            let p = MemoryPath::parse(&format!("/mnt/hr-policies/{name}")).unwrap();
            rig.memory_store.exists(&rig.tenant, &p).unwrap_or(false)
        });
        if all_present {
            break;
        }
        if attempts > 50 {
            panic!("worker did not complete ingest");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Verify contents match too, not just existence.
    for (name, body) in [
        ("a.md", "content a"),
        ("b.md", "content b"),
        ("c.md", "content c"),
        ("d.md", "content d"),
        ("e.md", "content e"),
    ] {
        let p = MemoryPath::parse(&format!("/mnt/hr-policies/{name}")).unwrap();
        let (content, _) = rig.memory_store.get(&rig.tenant, &p).unwrap();
        assert_eq!(String::from_utf8(content).unwrap(), body);
    }
}

/// Unauthenticated requests must fail before the stream is opened.
#[tokio::test]
async fn ingestion_stream_rejects_unauthenticated_before_opening_stream() {
    let rig = build_rig(true).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();

    let response = server
        .post("/api/v1/ingestion/stream")
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "pto.md", "content": b64("PTO: unlimited")}
            ]
        }))
        .await;

    response.assert_status(StatusCode::UNAUTHORIZED);
    let content_type = response
        .header("content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        !content_type.starts_with("text/event-stream"),
        "401 must NOT open an SSE stream, got content-type: {content_type:?}"
    );
}

/// When memory is not configured, the stream endpoint must 404 synchronously
/// (not open an empty SSE stream).
#[tokio::test]
async fn ingestion_stream_returns_404_when_memory_disabled() {
    let rig = build_rig(false).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion/stream")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "pto.md", "content": b64("PTO: unlimited")}
            ]
        }))
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["code"], json!("memory_not_configured"));
}

/// Parses SSE wire format into a flat list of JSON event payloads.
///
/// Strict: any `data:` frame that fails to parse as JSON panics. A framing
/// regression or a malformed event payload must fail loudly rather than be
/// silently discarded.
fn parse_sse_events(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter_map(|frame| {
            let mut data: Vec<&str> = Vec::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.trim_start());
                }
            }
            if data.is_empty() {
                None
            } else {
                Some(data.join("\n"))
            }
        })
        .map(|data| {
            serde_json::from_str::<serde_json::Value>(&data)
                .unwrap_or_else(|e| panic!("invalid SSE JSON payload {data:?}: {e}"))
        })
        .collect()
}

#[tokio::test]
async fn ingestion_returns_404_when_memory_disabled() {
    let rig = build_rig(false).await;
    let server = TestServer::new(build_router(rig.state, vec![], None)).unwrap();
    let (h, v) = auth_header();

    let response = server
        .post("/api/v1/ingestion")
        .add_header(h, v)
        .json(&json!({
            "source": "hr-policies",
            "files": [
                {"path": "pto.md", "content": b64("PTO: unlimited")}
            ]
        }))
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["code"], json!("memory_not_configured"));
}
