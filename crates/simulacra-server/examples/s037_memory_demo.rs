// S037 Wave D demo: real Claude agent using semantic_search + memory_read_chunk.
//
// Proves the full enterprise memory loop:
//   1. Simulacra server starts with memory enabled (SqliteMemoryStore + SqliteVectorIndex
//      + DefaultEmbedder + BackgroundEmbedder)
//   2. We pre-seed /mnt/hr/ with 4 HR policy documents via direct MemoryStore::put
//      (the BackgroundEmbedder picks them up automatically)
//   3. We POST a task: "What does the company PTO policy say about carry-over?"
//   4. A real Claude agent receives the task. Its config has memory enabled with
//      search_scopes = ["/mnt/hr"], so it gets the semantic_search and
//      memory_read_chunk tools registered into its ToolRegistry.
//   5. Claude calls semantic_search for "PTO carry-over policy"
//   6. Claude calls memory_read_chunk on the top hit's hit_id
//   7. Claude writes its answer (with citations) to /proc/mailbox/answer.md
//   8. We retrieve the artifact and verify it actually references the policy text
//
// Prereq:
//   ANTHROPIC_API_KEY=sk-...  (real LLM calls)
//   Aniani on localhost:4320 (optional — traces)
//
// Run:
//   ANTHROPIC_API_KEY=sk-... cargo run -p simulacra-server --example s037_memory_demo

use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, MarkdownSectionChunker, MemoryStore, SqliteMemoryStore, SqliteVectorIndex,
    VectorIndex,
};
use simulacra_server::*;
use simulacra_types::{MemoryPath, TenantId};
use std::collections::HashMap;
use std::sync::Arc;

const SERVICE_NAME: &str = "simulacra-s037-memory-demo";

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
                .with_service_name(SERVICE_NAME)
                .build(),
        )
        .build();

    let tracer = provider.tracer(SERVICE_NAME);
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let filter =
        tracing_subscriber::EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| {
            "WARN,simulacra_server=INFO,simulacra_runtime=INFO,simulacra_memory=INFO,simulacra_tool=INFO".into()
        }));

    let subscriber = tracing_subscriber::Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer);

    tracing::subscriber::set_global_default(subscriber).ok();
    provider
}

#[tokio::main]
async fn main() {
    let trace_provider = init_otel_tracing();

    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ERROR: ANTHROPIC_API_KEY must be set.");
        std::process::exit(1);
    }

    println!("\n╭───────────────────────────────────────────────────╮");
    println!("│  S037 Wave D — Real Claude Agent + Memory Tools  │");
    println!("╰───────────────────────────────────────────────────╯\n");

    // ── Step 1: Memory subsystem ─────────────────────────────────────────
    let memory_dir = std::env::temp_dir().join("simulacra-s037-memory-demo");
    let _ = std::fs::remove_dir_all(&memory_dir);
    println!("[1/7] Memory store at {}", memory_dir.display());

    let embedder_concrete = Arc::new(DefaultEmbedder::load_default().expect("default embedder"));
    let embedder: Arc<dyn Embedder> = embedder_concrete.clone();
    let memory_store: Arc<dyn MemoryStore> =
        Arc::new(SqliteMemoryStore::new(&memory_dir).expect("memory store"));
    let vector_index: Arc<dyn VectorIndex> = Arc::new(
        SqliteVectorIndex::new(&memory_dir, embedder_concrete.id().clone()).expect("vector index"),
    );

    println!(
        "       Embedder:    {} (dim {})",
        embedder.id(),
        embedder.dim()
    );
    println!("       Store:       SqliteMemoryStore");
    println!("       Index:       SqliteVectorIndex (sqlite-vec)");

    let chunker_selector: ChunkerSelector = {
        let md = Arc::new(MarkdownSectionChunker) as Arc<dyn Chunker>;
        Arc::new(move |path| {
            if path.as_str().ends_with(".md") {
                Some(md.clone())
            } else {
                None
            }
        })
    };

    let _bg_embedder = BackgroundEmbedder::spawn(
        Arc::clone(&memory_store),
        Arc::clone(&vector_index),
        Arc::clone(&embedder),
        chunker_selector,
        BackgroundEmbedderConfig::default(),
    )
    .expect("background embedder");
    println!("       BackgroundEmbedder: spawned");

    // ── Step 2: Pre-seed HR policy docs into /mnt/hr/ ───────────────────
    println!("\n[2/7] Pre-seeding 4 HR policy docs into /mnt/hr/ for tenant 'demo'");
    let tenant_id = TenantId::parse("demo").unwrap();

    let policy_docs: &[(&str, &str)] = &[
        (
            "/mnt/hr/pto.md",
            "# PTO Policy\n\n\
             Full-time employees accrue 2.5 days of paid time off per month, for a total of 30 days per year.\n\n\
             ## Carry-over\n\n\
             Unused PTO carries over to the next year, capped at 30 days. Excess balances above 30 days are forfeit on January 1.\n\n\
             ## Approval\n\n\
             PTO requests of 5 days or more require manager approval. Shorter requests are auto-approved.\n",
        ),
        (
            "/mnt/hr/remote-work.md",
            "# Remote Work Policy\n\n\
             Employees may work remotely up to 4 days per week with manager approval.\n\n\
             ## Equipment\n\n\
             Acme provides a laptop, monitor, and a $300 ergonomic chair stipend for full-time remote workers.\n",
        ),
        (
            "/mnt/hr/expenses.md",
            "# Expense Reimbursement\n\n\
             Submit receipts within 30 days of purchase via the expense portal.\n\n\
             ## Per diem\n\n\
             Meals are reimbursed up to $50 per day during business travel.\n\n\
             ## Pre-approval\n\n\
             Any single expense over $500 requires written manager pre-approval before purchase.\n",
        ),
        (
            "/mnt/hr/security.md",
            "# Security Policy\n\n\
             All employees must enable 2FA on company accounts within 7 days of joining.\n\n\
             ## Personal devices\n\n\
             Personal devices accessing company data must enroll in the managed MDM profile.\n\n\
             ## Incident reporting\n\n\
             Report suspected security incidents to security@acme.com within 1 hour of detection.\n",
        ),
    ];

    for (path, content) in policy_docs {
        let mp = MemoryPath::parse(path).unwrap();
        let v = memory_store
            .put(&tenant_id, &mp, content.as_bytes())
            .expect("seed policy doc");
        println!("       seeded {path:25} → version {v}");
    }

    // Wait for the BackgroundEmbedder to chunk + embed + upsert.
    println!("       Waiting for background indexing to complete...");
    let scope = MemoryPath::parse("/mnt/hr").unwrap();
    let probe = embedder
        .embed(&["wait probe"])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let hits = vector_index
            .search(&tenant_id, &scope, &probe, embedder.id(), 100, None)
            .unwrap_or_default();
        if hits.len() >= 4 {
            println!(
                "       Indexed: {} chunks searchable in /mnt/hr",
                hits.len()
            );
            break;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("FAIL: indexing timed out at {} chunks", hits.len());
            std::process::exit(1);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ── Step 3: Build the Simulacra API server ───────────────────────────────
    println!("\n[3/7] Building Simulacra API server with memory enabled on 127.0.0.1:9094...");

    let task_manager = Arc::new(TaskManager::new());

    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "demo-key".into(),
            subject: "memory-demo".into(),
            tenant_namespace: Some("demo".into()),
            scopes: vec![],
        }]));

    let mut tenants = HashMap::new();
    tenants.insert(
        "demo".to_string(),
        TenantConfig {
            namespace: "demo".into(),
            agent_type: "researcher".into(),
            vfs_root: "/tmp".into(),
            budget_pool: BudgetPoolConfig {
                max_tokens: 80000,
                max_cost: "2.00".into(),
            },
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    let resolver = TenantResolver::new(tenants, None);

    // Agent type with MEMORY ENABLED — search_scopes lets it search /mnt/hr/.
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "researcher".to_string(),
        simulacra_config::AgentTypeConfig {
            backend: Default::default(),
            model: "claude-sonnet-4-6".into(),
            acp_profile: None,
            system_prompt: None,
            max_turns: Some(15),
            max_tokens: Some(60000),
            max_sub_agents: None,
            capabilities: Some(simulacra_config::CapabilitiesConfig {
                shell: false,
                javascript: false,
                python: false,
                network: vec![],
                mcp: vec![],
                paths_read: vec!["/**".into()],
                paths_write: vec!["/workspace/**".into(), "/proc/mailbox/**".into()],
                skill_patterns: vec![],
                memory: Some(simulacra_config::MemoryCapabilityConfig {
                    enabled: true,
                    search_scopes: vec!["/mnt/hr".into(), "/var/memory/self".into()],
                    write_scopes: vec!["/var/memory/self".into()],
                }),
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "s037-memory-demo".into(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
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

    // Build the engine with memory wired up. AppState::with_engine inherits
    // the memory handles automatically.
    let artifact_store: Arc<dyn simulacra_types::ArtifactStore> = Arc::new(
        LocalDiskArtifactStore::new(&std::path::PathBuf::from("/tmp/simulacra-s037-artifacts"))
            .expect("artifact store"),
    );
    let engine = Arc::new(
        SimulacraEngine::with_memory_in_memory_catalog(
            simulacra_config,
            None,
            artifact_store,
            Arc::clone(&memory_store),
            Arc::clone(&vector_index),
            Arc::clone(&embedder),
        )
        .await
        .expect("engine construction failed"),
    );
    let state = AppState::with_engine(
        Arc::clone(&task_manager),
        Arc::new(resolver),
        auth,
        Arc::clone(&engine),
    );
    let router = build_router(state, vec![], None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9094")
        .await
        .expect("failed to bind 9094");
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!("       Simulacra API server ready.");

    // ── Step 4: POST a task that requires memory retrieval ──────────────
    println!("\n[4/7] POSTing a task that requires the agent to use semantic_search...");

    let client = reqwest::Client::new();
    let task_body = serde_json::json!({
        "task": concat!(
            "You are an HR research assistant. Answer this employee question, ",
            "grounded in the company HR policies stored in /mnt/hr/.\n\n",
            "Question: \"What does our PTO policy say about carry-over from year to year? ",
            "Specifically: how many days can I carry over, and what happens to anything above that limit?\"\n\n",
            "Steps:\n",
            "1. Use the semantic_search tool with scope \"/mnt/hr\" to find the relevant policy. ",
            "Search for something like \"PTO carry-over policy\".\n",
            "2. Use memory_read_chunk on the top hit's hit_id to get the full chunk text.\n",
            "3. Write a clear, complete answer to /proc/mailbox/pto-answer.md that:\n",
            "   - Directly answers both parts of the question\n",
            "   - Quotes the relevant policy text\n",
            "   - Cites the source path (/mnt/hr/pto.md)\n",
            "4. Stop. Do NOT search further once you have the answer."
        )
    });

    let create_resp = client
        .post("http://127.0.0.1:9094/api/v1/tasks/create")
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&task_body)
        .send()
        .await
        .expect("task create request failed");

    let create_body: serde_json::Value = create_resp.json().await.expect("invalid JSON response");
    if !create_body["ok"].as_bool().unwrap_or(false) {
        eprintln!("FAIL: Task creation failed: {create_body}");
        std::process::exit(1);
    }
    let task_id = create_body["data"]["task_id"]
        .as_str()
        .expect("no task_id")
        .to_string();
    println!("       Task ID: {task_id}");

    // ── Step 5: Wait for completion ──────────────────────────────────────
    println!("\n[5/7] Waiting for the agent to use memory tools (up to 120s)...");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut final_state = String::new();
    loop {
        if std::time::Instant::now() > deadline {
            eprintln!("FAIL: timed out");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let resp = client
            .get(format!(
                "http://127.0.0.1:9094/api/v1/tasks/{task_id}/status"
            ))
            .header("Authorization", "ApiKey demo-key")
            .send()
            .await;
        if let Ok(r) = resp {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let s = body["data"]["state"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            print!("       State: {s}    \r");
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            if matches!(s.as_str(), "completed" | "failed" | "killed" | "cancelled") {
                final_state = s;
                println!();
                break;
            }
        }
    }
    println!("       Final state: {final_state}");

    // ── Step 6: Retrieve the artifact ────────────────────────────────────
    println!("\n[6/7] Retrieving the agent's answer artifact...");

    let list_resp = client
        .get(format!(
            "http://127.0.0.1:9094/api/v1/tasks/{task_id}/artifacts"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .expect("list artifacts");
    let list_body: serde_json::Value = list_resp.json().await.unwrap_or_default();
    let artifacts = list_body["data"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if artifacts.is_empty() {
        eprintln!("FAIL: agent produced no artifacts");
        std::process::exit(1);
    }
    let report_path = artifacts
        .iter()
        .find_map(|a| a["path"].as_str().filter(|p| p.ends_with(".md")))
        .or_else(|| artifacts[0]["path"].as_str())
        .expect("no artifact path")
        .to_string();
    println!("       Fetching: {report_path}");

    let resp = client
        .get(format!(
            "http://127.0.0.1:9094/api/v1/tasks/{task_id}/artifacts/{report_path}"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .expect("get artifact");
    let bytes = resp.bytes().await.expect("artifact bytes");
    let text = String::from_utf8_lossy(&bytes);

    println!("\n╭─ Agent's answer ───────────────────────────────────────╮");
    for line in text.lines() {
        println!("│ {line}");
    }
    println!("╰────────────────────────────────────────────────────────╯");

    // ── Step 7: Verify the answer is grounded ───────────────────────────
    println!("\n[7/7] Verifying the answer references the actual policy...");

    let lower = text.to_lowercase();
    let mut pass = true;

    let checks: &[(&str, &str)] = &[
        ("30", "mentions the 30-day cap"),
        ("january", "mentions the January 1 forfeit"),
        ("/mnt/hr/pto.md", "cites the policy source path"),
    ];
    for (needle, label) in checks {
        if lower.contains(needle) {
            println!("       [PASS] {label}");
        } else {
            println!("       [WARN] does not {label}");
            pass = false;
        }
    }

    if final_state == "completed" {
        println!("       [PASS] Task completed");
    } else {
        println!("       [FAIL] Task ended with state: {final_state}");
        pass = false;
    }

    // ── Flush + query Aniani traces ────────────────────────────────────
    println!("\n[Traces] Flushing OTLP spans to Aniani...");
    if let Err(e) = trace_provider.force_flush() {
        println!("       Flush error: {e}");
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let obs = client
        .get("http://localhost:4320/api/v1/diagnose")
        .query(&[("service", SERVICE_NAME)])
        .send()
        .await;
    match obs {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let health = body["health_score"].as_f64().unwrap_or(0.0);
            let p99 = body["health_factors"]["latency_p99_ms"]
                .as_f64()
                .unwrap_or(0.0);
            println!("       Health: {health:.0}/100, p99 latency: {p99:.1}ms");
            let slow = body["slowest_traces"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            println!("       Slowest traces:");
            for t in slow.iter().take(8) {
                let tid = t["traceID"].as_str().unwrap_or("?");
                let root = t["rootSpanName"].as_str().unwrap_or("?");
                let span_count = t["spanCount"].as_u64().unwrap_or(0);
                let dur_ms = t["durationMs"].as_f64().unwrap_or(0.0);
                println!("         {root:<16} trace={tid}  spans={span_count}  {dur_ms:.1}ms");
            }
            if let Some(suggested) = body["suggested_queries"].as_array() {
                for sq in suggested.iter().take(3) {
                    if let Some(q) = sq["query"].as_str() {
                        println!("       Suggested: {q}");
                    }
                }
            }
        }
        Ok(r) => println!("       Aniani returned {}", r.status()),
        Err(e) => println!("       Aniani unreachable: {e}"),
    }
    if let Err(e) = trace_provider.shutdown() {
        eprintln!("       Trace provider shutdown error: {e}");
    }

    println!("\n╭───────────────────────────────────────────────────╮");
    if pass {
        println!("│  RESULT: real Claude used semantic_search +      │");
        println!("│  memory_read_chunk to ground its answer in       │");
        println!("│  pre-seeded HR policies via the memory subsystem │");
    } else {
        println!("│  RESULT: agent ran but verification gaps remain  │");
    }
    println!("╰───────────────────────────────────────────────────╯");
    println!("  Task ID:    {task_id}");
    println!("  Memory dir: {}", memory_dir.display());

    if !pass {
        std::process::exit(1);
    }
}
