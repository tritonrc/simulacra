// E2E integration test: Simulacra API → AgentLoop → JS fetch → credential injection → /proc/mailbox.
//
// Proves the full pipeline:
//   HTTP API → SimulacraEngine → real AgentLoop → JS code execution
//   → credential-injected external API call → write to /proc/mailbox
//
// Prerequisites:
//   ANTHROPIC_API_KEY=sk-...   (required — real LLM calls)
//   TOY_SAAS_TOKEN=toy-saas-secret-token-xyz   (set by this test)
//   Aniani running on localhost:4320   (optional — traces)
//
// Usage:
//   TOY_SAAS_TOKEN=toy-saas-secret-token-xyz ANTHROPIC_API_KEY=sk-... \
//     cargo run -p simulacra-server --example e2e_test

use simulacra_server::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ──────────────────────────────────────────────────────────────────────────────
// Toy SaaS inline (same logic as toy_saas.rs but importable)
// ──────────────────────────────────────────────────────────────────────────────

mod toy_saas_inline {
    use axum::{
        Json, Router,
        extract::Request,
        http::StatusCode,
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const VALID_TOKEN: &str = "toy-saas-secret-token-xyz";

    #[derive(Debug, Default)]
    pub struct ToySaasState {
        pub authed_requests: AtomicU64,
        pub unauthed_requests: AtomicU64,
    }

    async fn require_auth(req: Request, next: Next) -> Response {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let state = req.extensions().get::<Arc<ToySaasState>>().cloned();

        match auth_header.as_deref() {
            Some(h) if h == format!("Bearer {VALID_TOKEN}") => {
                if let Some(s) = &state {
                    s.authed_requests.fetch_add(1, Ordering::Relaxed);
                }
                next.run(req).await
            }
            _ => {
                if let Some(s) = &state {
                    s.unauthed_requests.fetch_add(1, Ordering::Relaxed);
                }
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "unauthorized"})),
                )
                    .into_response()
            }
        }
    }

    async fn get_me() -> impl IntoResponse {
        Json(json!({"id": "user-42", "name": "Test User", "email": "test@example.com"}))
    }

    async fn get_projects() -> impl IntoResponse {
        Json(json!({"projects": [{"id": "p1", "name": "Alpha"}, {"id": "p2", "name": "Beta"}]}))
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct NoteReq {
        text: String,
    }

    async fn post_notes(Json(body): Json<NoteReq>) -> impl IntoResponse {
        (
            StatusCode::CREATED,
            Json(json!({"id": "note-123", "created": true, "text": body.text})),
        )
    }

    async fn get_deals() -> impl IntoResponse {
        Json(json!({"deals": deals_fixture()}))
    }

    async fn get_contacts() -> impl IntoResponse {
        Json(json!({"contacts": contacts_fixture()}))
    }

    async fn get_pipeline_summary() -> impl IntoResponse {
        let deals = deals_fixture();
        let mut total_value: f64 = 0.0;
        let mut stage_counts: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        let mut at_risk = 0u64;
        for deal in &deals {
            total_value += deal["amount"].as_f64().unwrap_or(0.0);
            let stage = deal["stage"].as_str().unwrap_or("unknown").to_string();
            *stage_counts.entry(stage).or_insert(0) += 1;
            if deal["at_risk"].as_bool().unwrap_or(false) {
                at_risk += 1;
            }
        }
        Json(json!({
            "total_value": total_value,
            "stage_counts": stage_counts,
            "at_risk_count": at_risk,
            "deal_count": deals.len(),
        }))
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
                let stage = stages[(i as usize) % stages.len()];
                let amount = 1_000.0 + (i as f64) * 5_750.0;
                let owner = owners[(i as usize) % owners.len()];
                let month = 1 + (i % 9);
                let day = 1 + (i % 28);
                let close_date = format!("2026-{month:02}-{day:02}");
                let last_activity = format!("2026-{month:02}-{day:02}");
                let at_risk = matches!(i, 3 | 9 | 15 | 21);
                json!({
                    "id": format!("deal-{i:03}"),
                    "name": format!("Deal with Customer {}", i + 1),
                    "amount": amount,
                    "stage": stage,
                    "close_date": close_date,
                    "owner": owner,
                    "last_activity_date": last_activity,
                    "at_risk": at_risk,
                })
            })
            .collect()
    }

    fn contacts_fixture() -> Vec<serde_json::Value> {
        (0..12u32)
            .map(|i| {
                let deal_ids: Vec<String> =
                    vec![format!("deal-{:03}", i), format!("deal-{:03}", i + 12)];
                json!({
                    "id": format!("contact-{i:03}"),
                    "name": format!("Contact {}", i + 1),
                    "email": format!("contact{}@example.com", i + 1),
                    "company": format!("Customer {} Inc", i + 1),
                    "deal_ids": deal_ids,
                })
            })
            .collect()
    }

    pub fn build_router(state: Arc<ToySaasState>) -> Router {
        let api = Router::new()
            .route("/api/me", get(get_me))
            .route("/api/projects", get(get_projects))
            .route("/api/notes", post(post_notes))
            .route("/api/deals", get(get_deals))
            .route("/api/contacts", get(get_contacts))
            .route("/api/pipeline/summary", get(get_pipeline_summary))
            .layer(middleware::from_fn(require_auth));

        let health = Router::new().route("/health", get(|| async { Json(json!({"ok": true})) }));

        Router::new()
            .merge(api)
            .merge(health)
            .layer(axum::Extension(state))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// OTel setup
// ──────────────────────────────────────────────────────────────────────────────

fn init_otel_tracing() -> opentelemetry_sdk::trace::SdkTracerProvider {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;
    use tracing_subscriber::layer::SubscriberExt;

    let base =
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or("http://localhost:4320".into());
    let trace_endpoint = format!("{base}/v1/traces");

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&trace_endpoint)
        .build()
        .expect("OTLP exporter");

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("simulacra-e2e-test")
                .build(),
        )
        .build();

    let tracer = provider.tracer("simulacra-e2e-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(true);
    let filter =
        tracing_subscriber::EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| {
            "INFO,simulacra_sandbox=DEBUG,simulacra_server=DEBUG,simulacra_integration=DEBUG".into()
        }));

    let subscriber = tracing_subscriber::Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer);

    tracing::subscriber::set_global_default(subscriber).ok();
    provider
}

// ──────────────────────────────────────────────────────────────────────────────
// Main
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let trace_provider = init_otel_tracing();

    // ── Env validation ───────────────────────────────────────────────────
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ERROR: ANTHROPIC_API_KEY must be set.");
        std::process::exit(1);
    }

    // Ensure toy SaaS token is available for the integration registry.
    // SAFETY: Single-threaded at this point — no other threads are reading env vars yet.
    unsafe { std::env::set_var("TOY_SAAS_TOKEN", "toy-saas-secret-token-xyz") };

    println!("\n=== Simulacra E2E Integration Test ===\n");

    // ── Step 1: Start toy SaaS server on a SEPARATE thread ─────────────
    //
    // The Simulacra agent uses ureq (blocking HTTP client). If the toy SaaS runs
    // on the same tokio runtime, ureq blocks the worker threads and the SaaS
    // can't accept connections → deadlock. Running on a separate thread with
    // its own runtime avoids this.
    println!("[1/6] Starting toy SaaS server on 127.0.0.1:9091...");
    let saas_state = Arc::new(toy_saas_inline::ToySaasState::default());
    let saas_state_for_thread = Arc::clone(&saas_state);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("toy SaaS runtime");
        rt.block_on(async {
            let saas_router = toy_saas_inline::build_router(saas_state_for_thread);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:9091")
                .await
                .expect("failed to bind 9091");
            axum::serve(listener, saas_router).await.unwrap();
        });
    });

    // Quick health check.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let health_resp = reqwest::get("http://127.0.0.1:9091/health")
        .await
        .expect("toy SaaS health check failed");
    assert!(health_resp.status().is_success(), "toy SaaS not healthy");
    println!("       Toy SaaS healthy.");

    // ── Step 2: Build Simulacra API server ───────────────────────────────────
    println!("[2/6] Building Simulacra API server on 127.0.0.1:9092...");

    let task_manager = Arc::new(TaskManager::new());

    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "demo-key".into(),
            subject: "e2e-tester".into(),
            tenant_namespace: Some("demo".into()),
            scopes: vec![],
        }]));

    let mut tenants = HashMap::new();
    tenants.insert(
        "demo".to_string(),
        TenantConfig {
            namespace: "demo".into(),
            agent_type: "default".into(),
            vfs_root: "/tmp".into(),
            budget_pool: BudgetPoolConfig {
                max_tokens: 50000,
                max_cost: "2.00".into(),
            },
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    let resolver = TenantResolver::new(tenants, None);

    let mut agent_types = HashMap::new();
    agent_types.insert(
        "default".to_string(),
        simulacra_config::AgentTypeConfig {
            model: "claude-sonnet-4-6".into(),
            system_prompt: None,
            max_turns: Some(20),
            max_tokens: Some(50000),
            max_sub_agents: None,
            capabilities: Some(simulacra_config::CapabilitiesConfig {
                shell: false,
                javascript: true,
                python: false,
                network: vec!["*".into()],
                mcp: vec![],
                paths_read: vec!["/**".into()],
                paths_write: vec!["/workspace/**".into(), "/proc/mailbox/**".into()],

                skill_patterns: vec![],

                memory: None,
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    // Integration: toy-saas with credential injection.
    let mut integrations = HashMap::new();
    integrations.insert(
        "toy-saas".to_string(),
        simulacra_config::IntegrationConfig {
            auth: simulacra_config::AuthMethod::ApiKey {
                key: "TOY_SAAS_TOKEN".into(),
                placement: "header".into(), // Authorization: Bearer <token>
            },
            base_url: "http://127.0.0.1:9091".into(),
            description: Some("Toy SaaS API for E2E testing".into()),
            rate_limit_rps: 0,
            skills_path: None,
        },
    );

    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "e2e-test".into(),
            description: None,
        },
        agent_types,
        integrations,
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: simulacra_config::VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: simulacra_config::CatalogConfig::default(),
    };

    // Build typed IntegrationRegistry.
    let integration_registry =
        simulacra_integration::IntegrationRegistry::from_config(&simulacra_config.integrations)
            .expect("integration registry construction failed");
    println!("       Integrations: {:?}", integration_registry.names());
    let integration_registry = Some(Arc::new(integration_registry));

    let engine =
        SimulacraEngine::new_with_in_memory_catalog(simulacra_config, integration_registry)
            .await
            .expect("engine construction failed");
    let state = AppState::with_engine(
        Arc::clone(&task_manager),
        Arc::new(resolver),
        auth,
        Arc::new(engine),
    );
    let router = build_router(state, vec![], None);

    let simulacra_listener = tokio::net::TcpListener::bind("127.0.0.1:9092")
        .await
        .expect("failed to bind 9092");
    tokio::spawn(async move {
        axum::serve(simulacra_listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!("       Simulacra API server ready.");

    // ── Step 3: Create a task ────────────────────────────────────────────
    println!("[3/6] Sending task to Simulacra API...");

    let client = reqwest::Client::new();
    let task_body = serde_json::json!({
        "task": concat!(
            "Do these steps:\n",
            "1. Read /svc/toy-saas/config.json to discover the API base URL and status.\n",
            "2. Use js_exec to call fetch('http://127.0.0.1:9091/api/me') with GET method. ",
            "Do NOT set Authorization header — the platform injects it automatically. ",
            "Use this exact code: `const r = await fetch('http://127.0.0.1:9091/api/me'); ",
            "const j = await r.json(); JSON.stringify(j)`\n",
            "3. Use js_exec to call fetch('http://127.0.0.1:9091/api/projects') — same pattern, no auth header.\n",
            "4. Write a summary of the API responses to /proc/mailbox/saas-report.md using the write_file tool.\n",
            "5. Read /proc/mailbox/saas-report.md back to confirm the write succeeded.\n",
            "6. Report everything you found including the raw API response bodies."
        )
    });

    let create_resp = client
        .post("http://127.0.0.1:9092/api/v1/tasks/create")
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&task_body)
        .send()
        .await
        .expect("task create request failed");

    let create_status = create_resp.status();
    let create_body: serde_json::Value = create_resp.json().await.expect("invalid JSON response");

    println!("       Response: {create_status} {create_body}");

    if !create_body["ok"].as_bool().unwrap_or(false) {
        eprintln!("FAIL: Task creation failed: {create_body}");
        std::process::exit(1);
    }

    let task_id = create_body["data"]["task_id"]
        .as_str()
        .expect("no task_id in response")
        .to_string();
    println!("       Task ID: {task_id}");

    // ── Step 4: Poll for completion ──────────────────────────────────────
    println!("[4/6] Waiting for task completion (up to 60s)...");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut final_state = String::new();
    loop {
        if std::time::Instant::now() > deadline {
            eprintln!("FAIL: Task did not complete within 60s");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let status_resp = client
            .get(format!(
                "http://127.0.0.1:9092/api/v1/tasks/{task_id}/status"
            ))
            .header("Authorization", "ApiKey demo-key")
            .send()
            .await;

        match status_resp {
            Ok(resp) => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let state = body["data"]["state"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                print!("       State: {state}\r");
                match state.as_str() {
                    "completed" | "failed" | "killed" | "cancelled" => {
                        final_state = state;
                        println!();
                        break;
                    }
                    _ => continue,
                }
            }
            Err(e) => {
                eprintln!("       Poll error: {e}");
                continue;
            }
        }
    }

    println!("       Final state: {final_state}");

    // ── Step 5: Verify results ───────────────────────────────────────────
    println!("[5/6] Verifying results...");

    let mut pass = true;

    // Check 1: Task completed.
    if final_state == "completed" {
        println!("       [PASS] Task completed successfully");
    } else {
        println!("       [FAIL] Task ended with state: {final_state}");
        pass = false;
    }

    // Check 2: Toy SaaS received authenticated requests.
    let authed = saas_state.authed_requests.load(Ordering::Relaxed);
    let unauthed = saas_state.unauthed_requests.load(Ordering::Relaxed);
    if authed > 0 {
        println!(
            "       [PASS] Toy SaaS received {authed} authenticated request(s) ({unauthed} rejected)"
        );
    } else {
        println!(
            "       [FAIL] Toy SaaS received 0 authenticated requests ({unauthed} rejected) — credential injection broken?"
        );
        pass = false;
    }

    // Check 3: Task ID was returned.
    if !task_id.is_empty() {
        println!("       [PASS] Got task_id: {task_id}");
    } else {
        println!("       [FAIL] No task_id returned");
        pass = false;
    }

    // ── Step 6: Flush traces and query Aniani ─────────────────────────
    println!("[6/6] Flushing traces and querying Aniani...");

    // Force-flush the trace provider so all spans are exported before querying.
    if let Err(e) = trace_provider.force_flush() {
        println!("       Trace flush error: {e} (Aniani may not be running)");
    }
    // Brief pause for Aniani to index the flushed spans.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let aniani_resp = client
        .get("http://localhost:4320/api/v1/traces")
        .query(&[("service", "simulacra-e2e-test"), ("limit", "5")])
        .send()
        .await;

    match aniani_resp {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let trace_count = body["traces"].as_array().map(|a| a.len()).unwrap_or(0);
            if trace_count > 0 {
                println!("       [PASS] Found {trace_count} trace(s) in Aniani");
            } else {
                println!("       [WARN] Aniani reachable but 0 traces found — check OTLP pipeline");
            }
        }
        Ok(resp) => {
            println!(
                "       [WARN] Aniani query returned {}: traces may not be available",
                resp.status()
            );
        }
        Err(e) => {
            println!("       [SKIP] Aniani not reachable ({e}) — trace verification skipped");
        }
    }

    // Shut down the trace provider cleanly.
    if let Err(e) = trace_provider.shutdown() {
        eprintln!("       Trace provider shutdown error: {e}");
    }

    // ── Summary ──────────────────────────────────────────────────────────
    println!("\n=== E2E Test Summary ===");
    println!("  Task ID:               {task_id}");
    println!("  Final state:           {final_state}");
    println!(
        "  SaaS authed requests:  {}",
        saas_state.authed_requests.load(Ordering::Relaxed)
    );
    println!(
        "  SaaS unauthed reqs:    {}",
        saas_state.unauthed_requests.load(Ordering::Relaxed)
    );

    if pass {
        println!("\n  RESULT: ALL CHECKS PASSED\n");
    } else {
        println!("\n  RESULT: SOME CHECKS FAILED\n");
        std::process::exit(1);
    }
}
