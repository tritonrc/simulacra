// S037 Wave D demo: real Claude agents sharing memory across runs.
//
// Three coworker personas, each with distinct memory scopes, covering
// the virtual-coworker loops from spec §15:
//
//   Atlas (individual research) — writes to /var/memory/self/ and
//   recalls its own prior notes across runs. Target for Loop 1:
//   personal-memory persistence within a single persona's own subtree.
//
//   Sol (customer success) — writes observations about customers to
//   /var/memory/entities/customers/. Demonstrates entity-memory
//   capture and (with the failure subtree) Loop 2: failure avoidance.
//
//   Nova (ops generalist) — reads from /var/memory/entities to pick
//   up Sol's context; writes conversation logs to
//   /var/memory/conversations. Demonstrates the cross-coworker
//   entity-memory handoff (Loop 3) — Sol and Nova never communicate
//   directly; the memory subtree IS the communication channel.
//
// Three memory-enabled agent types with distinct sandboxed scopes;
// capability enforcement means each persona can only read/write its
// own lanes. The shared SqliteVectorIndex is the substrate that
// threads information across them.
//
// Prereq:
//   ANTHROPIC_API_KEY=sk-...  (real LLM calls)
//   Aniani on localhost:4320 (optional — traces)
//
// Run:
//   ANTHROPIC_API_KEY=sk-... cargo run -p simulacra-server --example s037_virtual_coworkers

use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, MarkdownSectionChunker, MemoryStore, SqliteMemoryStore, SqliteVectorIndex,
    VectorIndex,
};
use simulacra_server::*;
use simulacra_types::TenantId;
use std::collections::HashMap;
use std::sync::Arc;

const SERVICE_NAME: &str = "simulacra-s037-virtual-coworkers";
const PORT: u16 = 9095;

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

    println!("\n╭───────────────────────────────────────────────────────────╮");
    println!("│  S037 Wave D — Virtual Coworkers, One Shared Memory       │");
    println!("│  Atlas (self) · Sol (entities) → Nova (ops) via /var/memory │");
    println!("╰───────────────────────────────────────────────────────────╯\n");

    // ── Step 1: Memory subsystem ─────────────────────────────────────────
    let memory_dir = std::env::temp_dir().join("simulacra-s037-virtual-coworkers");
    let _ = std::fs::remove_dir_all(&memory_dir);
    println!("[1/8] Memory store at {}", memory_dir.display());

    let embedder_concrete = Arc::new(DefaultEmbedder::load_default().expect("default embedder"));
    let embedder: Arc<dyn Embedder> = embedder_concrete.clone();
    let memory_store: Arc<dyn MemoryStore> =
        Arc::new(SqliteMemoryStore::new(&memory_dir).expect("memory store"));
    let vector_index: Arc<dyn VectorIndex> = Arc::new(
        SqliteVectorIndex::new(&memory_dir, embedder_concrete.id().clone()).expect("vector index"),
    );

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
    println!(
        "       Embedder: {} (dim {})",
        embedder.id(),
        embedder.dim()
    );
    println!("       BackgroundEmbedder: spawned");

    // ── Step 2: Build the Simulacra API server with three coworker agent types
    println!("\n[2/8] Building simulacra-server on 127.0.0.1:{PORT}");
    println!("       Three memory-enabled agent types: 'atlas', 'sol', 'nova'");

    let task_manager = Arc::new(TaskManager::new());
    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "demo-key".into(),
            subject: "virtual-coworkers".into(),
            tenant_namespace: Some("demo".into()),
            scopes: vec![],
        }]));

    let mut tenants = HashMap::new();
    tenants.insert(
        "demo".to_string(),
        TenantConfig {
            namespace: "demo".into(),
            agent_type: "sol".into(), // tenant default; we override per task
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

    let mut agent_types = HashMap::new();

    // ── Atlas — individual-research agent. Writes to /var/memory/self/
    //    and recalls its own prior notes across runs. Demonstrates
    //    Loop 1 (spec §15): personal memory persistence. Scopes are
    //    tight — Atlas only sees its own self-notes and cannot read or
    //    write the entity subtree that Sol/Nova share.
    agent_types.insert(
        "atlas".to_string(),
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
                    search_scopes: vec!["/var/memory/self".into()],
                    write_scopes: vec!["/var/memory/self".into()],
                }),
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    // ── Sol — customer success agent. Writes to entities/customers/. ────
    //
    // Note: paths_write does NOT need to include /var/memory/** any more.
    // After the capability sandbox fix, memory paths are gated EXCLUSIVELY
    // by MemoryCapability.write_scopes — the generic paths_write glob is
    // not consulted for /var/memory/** or /mnt/**.
    agent_types.insert(
        "sol".to_string(),
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
                    search_scopes: vec!["/var/memory/entities".into(), "/var/memory/self".into()],
                    write_scopes: vec!["/var/memory/entities".into(), "/var/memory/self".into()],
                }),
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    // ── Nova — ops generalist agent. Reads entities/customers/ AND knows
    //    nothing about Sol's run. ─────────────────────────────────────────
    agent_types.insert(
        "nova".to_string(),
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
                    search_scopes: vec![
                        "/var/memory/entities".into(),
                        "/var/memory/conversations".into(),
                        // Loop 4 (RAG): admin-ingested HR policies land under
                        // /mnt/hr-policies/** via /api/v1/ingestion and Nova
                        // must be able to search them.
                        "/mnt/hr-policies".into(),
                    ],
                    write_scopes: vec!["/var/memory/conversations".into()],
                }),
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "s037-virtual-coworkers".into(),
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

    let artifact_store: Arc<dyn simulacra_types::ArtifactStore> = Arc::new(
        LocalDiskArtifactStore::new(&std::path::PathBuf::from(
            "/tmp/simulacra-s037-virtual-coworkers-artifacts",
        ))
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

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{PORT}"))
        .await
        .expect("failed to bind");
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!("       Simulacra API server ready.");

    let client = reqwest::Client::new();

    // ── Step 3: Sol runs first — handle a customer ticket ───────────────
    println!("\n[3/8] ┌─────────────────────────────────────┐");
    println!("       │ SOL (customer success) — TASK 1     │");
    println!("       └─────────────────────────────────────┘");

    let sol_task = serde_json::json!({
        "agent_type": "sol",
        "task": concat!(
            "You are Sol, the customer success agent. A new support ticket just arrived:\n\n",
            "  Customer:  Acme Corp\n",
            "  Reporter:  jane.smith@acme.com (VP of Engineering)\n",
            "  Severity:  P1\n",
            "  Subject:   API throttling on /v1/contacts endpoint\n",
            "  Body:\n",
            "    Our nightly sync job started failing on Monday. We get 429 Too Many Requests after\n",
            "    about 50 calls. We're a Tier-A customer with a 10K req/hour quota. We've checked our\n",
            "    code and we're definitely not exceeding 10K. Can you investigate?\n\n",
            "    PS — we had a similar issue in Q3 that took two weeks to resolve. Please don't make\n",
            "    us go through that again.\n\n",
            "Your job:\n",
            "1. Document this ticket as an entity memory file at\n",
            "   /var/memory/entities/customers/acme-corp.md (use file_write).\n",
            "   The file should be a Markdown record with:\n",
            "   - The customer name and tier\n",
            "   - The reporter's name and role\n",
            "   - A summary of the current incident in plain English\n",
            "   - A 'History' section noting that they had a similar Q3 incident that took 2 weeks\n",
            "     (this is important context for future agents who handle this customer)\n",
            "   - A 'Sentiment' section: this customer is frustrated about the historical pattern\n",
            "   - A 'Status' line saying you opened the investigation\n",
            "2. Stop. Do NOT actually investigate the API throttling — just record what you know.\n",
            "   Your job is to capture the context so future coworkers can pick up where you left off.\n"
        )
    });

    println!("\n       Posting Sol's task...");
    let sol_create = client
        .post(format!("http://127.0.0.1:{PORT}/api/v1/tasks/create"))
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&sol_task)
        .send()
        .await
        .expect("sol create");
    let sol_body: serde_json::Value = sol_create.json().await.unwrap_or_default();
    if !sol_body["ok"].as_bool().unwrap_or(false) {
        eprintln!("FAIL: Sol task creation: {sol_body}");
        std::process::exit(1);
    }
    let sol_task_id = sol_body["data"]["task_id"]
        .as_str()
        .expect("no task id")
        .to_string();
    println!("       Sol task ID: {sol_task_id}");

    // ── Step 4: Wait for Sol ────────────────────────────────────────────
    println!("\n[4/8] Waiting for Sol to document the ticket (up to 120s)...");
    let sol_state = wait_for_task(&client, &sol_task_id).await;
    println!("       Sol final state: {sol_state}");
    if sol_state != "completed" {
        eprintln!("FAIL: Sol did not complete");
        std::process::exit(1);
    }

    // Verify Sol's memory write actually landed via the store directly.
    let acme_path =
        simulacra_types::MemoryPath::parse("/var/memory/entities/customers/acme-corp.md")
            .expect("acme path");
    let acme_check = memory_store.get(&TenantId::parse("demo").unwrap(), &acme_path);
    match &acme_check {
        Ok((bytes, version)) => {
            println!(
                "       ✓ Sol's note exists in memory store: {} bytes, version {version}",
                bytes.len()
            );
        }
        Err(e) => {
            eprintln!("FAIL: Sol's memory write missing: {e}");
            std::process::exit(1);
        }
    }

    // Wait for the BackgroundEmbedder to index Sol's note.
    println!("       Waiting for background embedder to index Sol's note...");
    let entities_scope = simulacra_types::MemoryPath::parse("/var/memory/entities").unwrap();
    let probe = embedder
        .embed(&["wait probe"])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let hits = vector_index
            .search(
                &TenantId::parse("demo").unwrap(),
                &entities_scope,
                &probe,
                embedder.id(),
                10,
                None,
            )
            .unwrap_or_default();
        if !hits.is_empty() {
            println!(
                "       ✓ Sol's note is now searchable ({} chunks indexed)",
                hits.len()
            );
            break;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("FAIL: indexing timed out");
            std::process::exit(1);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ── Step 5: Nova runs second — knows nothing about Sol's run ─────────
    println!("\n[5/8] ┌─────────────────────────────────────┐");
    println!("       │ NOVA (ops generalist) — TASK 2      │");
    println!("       │ Brand new task. No shared context.  │");
    println!("       └─────────────────────────────────────┘");

    let nova_task = serde_json::json!({
        "agent_type": "nova",
        "task": concat!(
            "You are Nova, the ops generalist. Brian (the engineering lead) just messaged you in Slack:\n\n",
            "  Brian: \"hey nova — heads up, jane at acme corp called me directly this morning.\n",
            "         she's pretty upset. apparently they've been hitting api throttling issues.\n",
            "         do we know anything about acme's history with us? what's the context here?\n",
            "         try to give me the full picture so i can call her back informed.\"\n\n",
            "Do EXACTLY these steps in order. Do not skip, reorder, or repeat:\n",
            "\n",
            "STEP 1 — Call `semantic_search` ONCE with scope \"/var/memory/entities\" and\n",
            "         query \"acme corp customer history\". Pick the top hit.\n",
            "\n",
            "STEP 2 — Call `memory_read_chunk` AT MOST TWICE total. Pick the one or two\n",
            "         most relevant hit_ids from step 1 and read them. DO NOT call\n",
            "         memory_read_chunk three or more times. If two chunks don't give you\n",
            "         enough context, say so in your response — don't keep reading.\n",
            "\n",
            "STEP 3 — Call `file_write` ONCE to create /proc/mailbox/brian-acme-context.md.\n",
            "         This is your final answer to Brian. It MUST be a Markdown file and\n",
            "         MUST include:\n",
            "           - What we know about Acme Corp specifically (customer tier, contract, etc.)\n",
            "           - The historical Q3 incident pattern (this is why Jane is upset)\n",
            "           - A note that the current throttling incident is being investigated\n",
            "           - A citation line referencing the memory path you read from\n",
            "         Write Slack-style prose, not a bulleted checklist.\n",
            "\n",
            "STEP 4 — Stop. Do NOT call any more tools. End your turn.\n",
            "\n",
            "HARD CONSTRAINT: You have 15 turns total. You should complete all four steps\n",
            "in 4-5 turns. If you find yourself about to call memory_read_chunk a third\n",
            "time, STOP and proceed to STEP 3 with what you already have.\n",
            "\n",
            "IMPORTANT: You have NOT seen this customer before. Everything you know must come\n",
            "from the memory subsystem. If semantic_search returns nothing useful, say so\n",
            "honestly in the file at STEP 3."
        )
    });

    println!("\n       Posting Nova's task...");
    let nova_create = client
        .post(format!("http://127.0.0.1:{PORT}/api/v1/tasks/create"))
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&nova_task)
        .send()
        .await
        .expect("nova create");
    let nova_body: serde_json::Value = nova_create.json().await.unwrap_or_default();
    if !nova_body["ok"].as_bool().unwrap_or(false) {
        eprintln!("FAIL: Nova task creation: {nova_body}");
        std::process::exit(1);
    }
    let nova_task_id = nova_body["data"]["task_id"]
        .as_str()
        .expect("no task id")
        .to_string();
    println!("       Nova task ID: {nova_task_id}");

    // ── Step 6: Wait for Nova ───────────────────────────────────────────
    println!("\n[6/8] Waiting for Nova to use semantic_search + memory_read_chunk (up to 120s)...");
    let nova_state = wait_for_task(&client, &nova_task_id).await;
    println!("       Nova final state: {nova_state}");

    // ── Step 7: Retrieve Nova's answer ──────────────────────────────────
    println!("\n[7/8] Retrieving Nova's response to Brian...");
    let list = client
        .get(format!(
            "http://127.0.0.1:{PORT}/api/v1/tasks/{nova_task_id}/artifacts"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .expect("list");
    let list_body: serde_json::Value = list.json().await.unwrap_or_default();
    let artifacts = list_body["data"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let nova_artifact = artifacts
        .iter()
        .find_map(|a| a["path"].as_str().filter(|p| p.ends_with(".md")))
        .or_else(|| artifacts.first().and_then(|a| a["path"].as_str()))
        .expect("nova artifact missing")
        .to_string();
    let nova_resp = client
        .get(format!(
            "http://127.0.0.1:{PORT}/api/v1/tasks/{nova_task_id}/artifacts/{nova_artifact}"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .expect("get nova artifact");
    let nova_text = nova_resp.text().await.unwrap_or_default();

    println!("\n╭─ Nova's response to Brian ─────────────────────────────╮");
    for line in nova_text.lines() {
        println!("│ {line}");
    }
    println!("╰────────────────────────────────────────────────────────╯");

    // Also retrieve and show Sol's note for comparison.
    let (sol_bytes, _) = acme_check.unwrap();
    let sol_text = String::from_utf8_lossy(&sol_bytes);
    println!("\n╭─ Sol's note (for comparison — Nova never saw this directly) ╮");
    for line in sol_text.lines() {
        println!("│ {line}");
    }
    println!("╰──────────────────────────────────────────────────────────────╯");

    // ── Step 8: Verify the cross-coworker memory loop ──────────────────
    println!("\n[8/8] Verifying Nova's answer references Sol's specific context...");

    // Track per-loop results so a failure in one loop does not abort the
    // others. Final combined banner exits 1 if any loop failed.
    let mut loop_results: Vec<(&str, bool, String)> = Vec::new();

    let lower = nova_text.to_lowercase();
    let mut loop3_pass = nova_state == "completed";

    let checks: &[(&[&str], &str)] = &[
        (
            &["q3", "previous", "prior", "history", "historical"],
            "references historical context Sol captured",
        ),
        (&["acme"], "names the customer (Acme)"),
        (
            &["jane", "vp", "engineering"],
            "references the reporter Sol recorded",
        ),
        (
            &["throttl", "429", "rate", "api", "investigat"],
            "references the actual technical issue",
        ),
        (
            &[
                "/var/memory/entities/customers/acme-corp.md",
                "memory",
                "acme-corp.md",
            ],
            "cites the source memory path",
        ),
    ];
    for (needles, label) in checks {
        let hit = needles.iter().any(|n| lower.contains(n));
        if hit {
            println!("       [PASS] {label}");
        } else {
            println!("       [WARN] does not {label}");
            loop3_pass = false;
        }
    }

    if nova_state == "completed" {
        println!("       [PASS] Nova task completed");
    } else {
        println!("       [FAIL] Nova ended in state: {nova_state}");
        loop3_pass = false;
    }

    loop_results.push((
        "Loop 3 — Cross-agent entity memory (Sol → Nova)",
        loop3_pass,
        format!("sol={sol_task_id} nova={nova_task_id}"),
    ));

    // ═══════════════════════════════════════════════════════════════════
    //  Loop 1 — Atlas individual learning (spec §15 / assertion 1200)
    //
    //  Day 1: Atlas writes a personal insight to /var/memory/self/.
    //  Day 2: A fresh Atlas task (no LLM context carry-over) searches its
    //  own prior notes and summarizes the finding into a mailbox artifact.
    // ═══════════════════════════════════════════════════════════════════
    println!("\n╭──────────────────────────────────────────────────────────╮");
    println!("│  LOOP 1 — Atlas individual learning (assertion 1200)     │");
    println!("│  day 1 note  →  /var/memory/self/  →  day 2 recall       │");
    println!("╰──────────────────────────────────────────────────────────╯");

    // ── Loop 1 Phase A: Atlas day 1 writes the insight.
    println!("\n[L1/A] ┌─────────────────────────────────────┐");
    println!("        │ ATLAS (day 1) — record an insight   │");
    println!("        └─────────────────────────────────────┘");

    let atlas_day1_body = serde_json::json!({
        "agent_type": "atlas",
        "task": concat!(
            "You are Atlas. While reviewing deploy logs, you noticed a recurring pattern:\n",
            "the nightly backup job silently skips rows where `last_modified IS NULL`.\n",
            "\n",
            "Record this finding to /var/memory/self/insights/backup-null-timestamps.md\n",
            "using `file_write`. Structure the file as:\n",
            "  - A brief title line\n",
            "  - One paragraph describing the observation\n",
            "  - Three lines of actionable implications (one per line)\n",
            "\n",
            "Do EXACTLY ONE `file_write` call. Then stop. Do not investigate further,\n",
            "do not call any other tools, do not write a second file.\n"
        )
    });
    let atlas_day1_id = match post_task(&client, &atlas_day1_body).await {
        Ok(id) => id,
        Err(e) => {
            println!("        [FAIL] Atlas day-1 task creation: {e}");
            loop_results.push((
                "Loop 1 — Atlas individual learning",
                false,
                format!("task creation failed: {e}"),
            ));
            String::new()
        }
    };

    let mut loop1_pass = !atlas_day1_id.is_empty();
    let mut atlas_day1_state = String::from("<skipped>");
    let atlas_insight_path =
        simulacra_types::MemoryPath::parse("/var/memory/self/insights/backup-null-timestamps.md")
            .expect("atlas insight path");

    if loop1_pass {
        println!("        Atlas day-1 task ID: {atlas_day1_id}");
        println!("        Waiting for Atlas day-1 (up to 120s)...");
        atlas_day1_state = wait_for_task(&client, &atlas_day1_id).await;
        println!("        Atlas day-1 final state: {atlas_day1_state}");
        if atlas_day1_state != "completed" {
            println!("        [FAIL] Atlas day-1 did not complete");
            loop1_pass = false;
        } else {
            println!("        [PASS] Atlas day-1 task completed");
        }

        match memory_store.get(&TenantId::parse("demo").unwrap(), &atlas_insight_path) {
            Ok((bytes, version)) => {
                println!(
                    "        [PASS] Atlas insight exists in memory store: {} bytes, v{version}",
                    bytes.len()
                );
            }
            Err(e) => {
                println!(
                    "        [FAIL] Atlas insight missing at {}: {e}",
                    atlas_insight_path.as_str()
                );
                loop1_pass = false;
            }
        }

        // Wait for the BackgroundEmbedder to index Atlas's note before the
        // day-2 search so semantic_search has something to return.
        println!("        Waiting for background embedder to index Atlas's note...");
        let self_scope = simulacra_types::MemoryPath::parse("/var/memory/self").unwrap();
        if !wait_for_index(&vector_index, embedder.as_ref(), &self_scope, 15).await {
            println!("        [FAIL] Atlas insight was not indexed within 15s");
            loop1_pass = false;
        } else {
            println!("        [PASS] Atlas insight is searchable");
        }
    }

    // ── Loop 1 Phase B: Atlas day 2 recalls the insight in a fresh task.
    println!("\n[L1/B] ┌─────────────────────────────────────┐");
    println!("        │ ATLAS (day 2) — recall via memory   │");
    println!("        │ Fresh task, no LLM context carry    │");
    println!("        └─────────────────────────────────────┘");

    let atlas_day2_body = serde_json::json!({
        "agent_type": "atlas",
        "task": concat!(
            "You are Atlas. A teammate just asked about the nightly backup job.\n",
            "Before answering, search your own memory for any prior notes you've\n",
            "written about it.\n",
            "\n",
            "Do EXACTLY these steps in order:\n",
            "\n",
            "STEP 1 — Call `semantic_search` ONCE with scope \"/var/memory/self\"\n",
            "         and query \"backup nightly job issue\". Pick the top hit.\n",
            "\n",
            "STEP 2 — Call `memory_read_chunk` AT MOST ONCE on the top hit_id.\n",
            "\n",
            "STEP 3 — Call `file_write` ONCE to create\n",
            "         /proc/mailbox/atlas-backup-summary.md. The file MUST:\n",
            "           - State the specific finding you recalled\n",
            "           - Cite the memory path you read from (include the full\n",
            "             path like /var/memory/self/insights/backup-null-timestamps.md)\n",
            "\n",
            "STEP 4 — Stop. End your turn. Do not call any more tools.\n",
            "\n",
            "HARD CONSTRAINT: Do not exceed 5 turns total.\n",
            "\n",
            "IMPORTANT: You have NOT seen this finding in the current conversation.\n",
            "Everything you know must come from semantic_search + memory_read_chunk.\n"
        )
    });

    let mut atlas_day2_id = String::new();
    let mut atlas_day2_state = String::from("<skipped>");
    let mut atlas_day2_text = String::new();

    if loop1_pass {
        match post_task(&client, &atlas_day2_body).await {
            Ok(id) => atlas_day2_id = id,
            Err(e) => {
                println!("        [FAIL] Atlas day-2 task creation: {e}");
                loop1_pass = false;
            }
        }
    }

    if loop1_pass {
        println!("        Atlas day-2 task ID: {atlas_day2_id}");
        println!("        Waiting for Atlas day-2 (up to 120s)...");
        atlas_day2_state = wait_for_task(&client, &atlas_day2_id).await;
        println!("        Atlas day-2 final state: {atlas_day2_state}");
        if atlas_day2_state != "completed" {
            println!("        [FAIL] Atlas day-2 did not complete");
            loop1_pass = false;
        } else {
            println!("        [PASS] Atlas day-2 task completed");
        }

        match fetch_markdown_artifact(&client, &atlas_day2_id).await {
            Ok((path, text)) => {
                println!("        Retrieved artifact: {path} ({} bytes)", text.len());
                atlas_day2_text = text;
            }
            Err(e) => {
                println!("        [FAIL] Could not retrieve Atlas day-2 artifact: {e}");
                loop1_pass = false;
            }
        }
    }

    if !atlas_day2_text.is_empty() {
        println!("\n╭─ Atlas day-2 response (recalled from memory) ───────────╮");
        for line in atlas_day2_text.lines() {
            println!("│ {line}");
        }
        println!("╰─────────────────────────────────────────────────────────╯");

        let lower = atlas_day2_text.to_lowercase();
        let atlas_checks: &[(&[&str], &str)] = &[
            (&["null"], "mentions 'null' (the core finding)"),
            (&["backup"], "mentions 'backup' (the topic)"),
            (
                &[
                    "/var/memory/self/insights/backup-null-timestamps.md",
                    "backup-null-timestamps.md",
                ],
                "cites the memory path of the day-1 note",
            ),
        ];
        for (needles, label) in atlas_checks {
            let hit = needles.iter().any(|n| lower.contains(n));
            if hit {
                println!("        [PASS] {label}");
            } else {
                println!("        [WARN] does not {label}");
                loop1_pass = false;
            }
        }
    }

    loop_results.push((
        "Loop 1 — Atlas individual learning",
        loop1_pass,
        format!(
            "day1={atlas_day1_id}({atlas_day1_state}) day2={atlas_day2_id}({atlas_day2_state})"
        ),
    ));

    // ═══════════════════════════════════════════════════════════════════
    //  Loop 2 — Sol failure avoidance (spec §15 / assertion 1201)
    //
    //  Attempt 1: Sol records a failure mode into /var/memory/entities/failures/.
    //  Attempt 2: A fresh Sol task searches failure memory FIRST and picks
    //  a different approach than the one that failed.
    //
    //  Judgment call: the spec says "failure memory" without specifying a
    //  subtree. We scope failures under /var/memory/entities/failures/ so
    //  they live inside Sol's existing write_scope (/var/memory/entities)
    //  without changing agent config. Other teams could pick /var/memory/self
    //  for persona-scoped failures; we chose entity-scoped because the
    //  failure is about a customer interaction.
    // ═══════════════════════════════════════════════════════════════════
    println!("\n╭──────────────────────────────────────────────────────────╮");
    println!("│  LOOP 2 — Sol failure avoidance (assertion 1201)         │");
    println!("│  attempt 1 failure → memory → attempt 2 different fix    │");
    println!("╰──────────────────────────────────────────────────────────╯");

    // ── Loop 2 Phase A: Sol records the failed attempt.
    println!("\n[L2/A] ┌─────────────────────────────────────┐");
    println!("        │ SOL (attempt 1) — record failure    │");
    println!("        └─────────────────────────────────────┘");

    let sol_fail_body = serde_json::json!({
        "agent_type": "sol",
        "task": concat!(
            "You are Sol. Customer `Globex Inc` reports that our SDK's\n",
            "`client.upload(stream)` method hangs indefinitely on files over\n",
            "500MB. You tried increasing the socket timeout to 30 minutes —\n",
            "this did NOT fix it; the hang is at the TLS handshake, not the\n",
            "transfer.\n",
            "\n",
            "Record this failure mode to\n",
            "/var/memory/entities/failures/globex-sdk-upload.md using `file_write`.\n",
            "The file MUST contain:\n",
            "  - A title line\n",
            "  - Customer name: Globex Inc\n",
            "  - Attempted fix: increased socket timeout to 30 minutes\n",
            "  - Why it failed: hang is at TLS handshake, not transfer\n",
            "  - An open-question line describing what to try next\n",
            "\n",
            "Do EXACTLY ONE `file_write` call. Then stop. Do not investigate\n",
            "further, do not call any other tools, do not write a second file.\n"
        )
    });

    let sol_fail_id = match post_task(&client, &sol_fail_body).await {
        Ok(id) => id,
        Err(e) => {
            println!("        [FAIL] Sol attempt-1 task creation: {e}");
            loop_results.push((
                "Loop 2 — Sol failure avoidance",
                false,
                format!("task creation failed: {e}"),
            ));
            String::new()
        }
    };

    let mut loop2_pass = !sol_fail_id.is_empty();
    let mut sol_fail_state = String::from("<skipped>");
    let failure_path =
        simulacra_types::MemoryPath::parse("/var/memory/entities/failures/globex-sdk-upload.md")
            .expect("failure path");

    if loop2_pass {
        println!("        Sol attempt-1 task ID: {sol_fail_id}");
        println!("        Waiting for Sol attempt-1 (up to 120s)...");
        sol_fail_state = wait_for_task(&client, &sol_fail_id).await;
        println!("        Sol attempt-1 final state: {sol_fail_state}");
        if sol_fail_state != "completed" {
            println!("        [FAIL] Sol attempt-1 did not complete");
            loop2_pass = false;
        } else {
            println!("        [PASS] Sol attempt-1 task completed");
        }

        match memory_store.get(&TenantId::parse("demo").unwrap(), &failure_path) {
            Ok((bytes, version)) => {
                println!(
                    "        [PASS] Failure note exists in memory store: {} bytes, v{version}",
                    bytes.len()
                );
            }
            Err(e) => {
                println!(
                    "        [FAIL] Failure note missing at {}: {e}",
                    failure_path.as_str()
                );
                loop2_pass = false;
            }
        }

        println!("        Waiting for background embedder to index the failure note...");
        let failures_scope = simulacra_types::MemoryPath::parse("/var/memory/entities").unwrap();
        if !wait_for_index(&vector_index, embedder.as_ref(), &failures_scope, 15).await {
            println!("        [FAIL] Failure note was not indexed within 15s");
            loop2_pass = false;
        } else {
            println!("        [PASS] Failure note is searchable");
        }
    }

    // ── Loop 2 Phase B: Sol attempt 2 checks failure memory first.
    println!("\n[L2/B] ┌─────────────────────────────────────┐");
    println!("        │ SOL (attempt 2) — check first       │");
    println!("        └─────────────────────────────────────┘");

    let sol_retry_body = serde_json::json!({
        "agent_type": "sol",
        "task": concat!(
            "You are Sol. Customer `Globex Inc` is back with the\n",
            "`client.upload(stream)` hang on 500MB+ files. Before proposing\n",
            "a fix, check what has already been tried.\n",
            "\n",
            "Do EXACTLY these steps in order:\n",
            "\n",
            "STEP 1 — Call `semantic_search` ONCE with scope\n",
            "         \"/var/memory/entities/failures\" and query\n",
            "         \"globex sdk upload hang\". Pick the top hit.\n",
            "\n",
            "STEP 2 — Call `memory_read_chunk` AT MOST ONCE on the top hit.\n",
            "\n",
            "STEP 3 — Call `file_write` ONCE to create\n",
            "         /proc/mailbox/globex-next-approach.md. The file MUST:\n",
            "           (a) acknowledge the prior failed attempt (socket timeout)\n",
            "           (b) propose a DIFFERENT approach — e.g. chunked multipart\n",
            "               upload, pre-signed URL, connection pooling, or\n",
            "               disabling TLS session resumption\n",
            "           (c) cite /var/memory/entities/failures/globex-sdk-upload.md\n",
            "               as the source for the prior-failure context\n",
            "\n",
            "STEP 4 — Stop. End your turn.\n",
            "\n",
            "HARD CONSTRAINT: Do not exceed 5 turns total. Do NOT re-propose\n",
            "increasing the socket timeout — that has already been tried and\n",
            "did not work.\n"
        )
    });

    let mut sol_retry_id = String::new();
    let mut sol_retry_state = String::from("<skipped>");
    let mut sol_retry_text = String::new();

    if loop2_pass {
        match post_task(&client, &sol_retry_body).await {
            Ok(id) => sol_retry_id = id,
            Err(e) => {
                println!("        [FAIL] Sol attempt-2 task creation: {e}");
                loop2_pass = false;
            }
        }
    }

    if loop2_pass {
        println!("        Sol attempt-2 task ID: {sol_retry_id}");
        println!("        Waiting for Sol attempt-2 (up to 120s)...");
        sol_retry_state = wait_for_task(&client, &sol_retry_id).await;
        println!("        Sol attempt-2 final state: {sol_retry_state}");
        if sol_retry_state != "completed" {
            println!("        [FAIL] Sol attempt-2 did not complete");
            loop2_pass = false;
        } else {
            println!("        [PASS] Sol attempt-2 task completed");
        }

        match fetch_markdown_artifact(&client, &sol_retry_id).await {
            Ok((path, text)) => {
                println!("        Retrieved artifact: {path} ({} bytes)", text.len());
                sol_retry_text = text;
            }
            Err(e) => {
                println!("        [FAIL] Could not retrieve Sol attempt-2 artifact: {e}");
                loop2_pass = false;
            }
        }
    }

    if !sol_retry_text.is_empty() {
        println!("\n╭─ Sol attempt-2 proposal (memory-informed) ─────────────╮");
        for line in sol_retry_text.lines() {
            println!("│ {line}");
        }
        println!("╰────────────────────────────────────────────────────────╯");

        let lower = sol_retry_text.to_lowercase();
        let sol_retry_checks: &[(&[&str], &str)] = &[
            (
                &["timeout", "socket"],
                "acknowledges the prior failed approach (socket timeout)",
            ),
            (
                &[
                    "chunk",
                    "multipart",
                    "pre-signed",
                    "presigned",
                    "pool",
                    "resumption",
                ],
                "proposes a different approach",
            ),
            (
                &[
                    "/var/memory/entities/failures/globex-sdk-upload.md",
                    "globex-sdk-upload",
                ],
                "cites the failure-memory source",
            ),
        ];
        for (needles, label) in sol_retry_checks {
            let hit = needles.iter().any(|n| lower.contains(n));
            if hit {
                println!("        [PASS] {label}");
            } else {
                println!("        [WARN] does not {label}");
                loop2_pass = false;
            }
        }
    }

    loop_results.push((
        "Loop 2 — Sol failure avoidance",
        loop2_pass,
        format!(
            "attempt1={sol_fail_id}({sol_fail_state}) attempt2={sol_retry_id}({sol_retry_state})"
        ),
    ));

    // ═══════════════════════════════════════════════════════════════════
    //  Loop 4 — Nova RAG (spec §15 / assertion 1203)
    //
    //  Admin ingests HR policies under /mnt/hr-policies/ via
    //  /api/v1/ingestion. Nova then answers an employee question with a
    //  citation back to the policy source.
    // ═══════════════════════════════════════════════════════════════════
    println!("\n╭──────────────────────────────────────────────────────────╮");
    println!("│  LOOP 4 — Nova RAG (assertion 1203)                      │");
    println!("│  admin ingest → /mnt/hr-policies → Nova answers + cites  │");
    println!("╰──────────────────────────────────────────────────────────╯");

    // ── Loop 4 Phase A: admin ingests three HR policy markdown docs.
    println!("\n[L4/A] ┌─────────────────────────────────────┐");
    println!("        │ ADMIN — /api/v1/ingestion (merge)   │");
    println!("        └─────────────────────────────────────┘");

    let hr_files: [(&str, &str); 3] = [
        (
            "pto.md",
            "Full-time employees receive 20 PTO days per year, accruing at 1.67 days per month. PTO carries over up to 10 days into the next calendar year. Requests must be submitted at least 5 business days in advance via the HR portal.",
        ),
        (
            "remote.md",
            "Remote work is allowed up to 3 days per week for employees past their 90-day probation period. Quarterly in-person team sync is mandatory. Remote work requests do not require approval.",
        ),
        (
            "expenses.md",
            "Employees may expense work-related meals up to $50/day and travel up to $500 without pre-approval. Larger expenses require manager sign-off via the expense portal.",
        ),
    ];

    let mut loop4_pass = true;
    let ingestion_result = ingest_hr_policies(&client, &hr_files).await;
    match &ingestion_result {
        Ok(written) => {
            println!("        [PASS] Ingestion returned HTTP 200");
            if written.len() == hr_files.len() {
                println!(
                    "        [PASS] Ingestion wrote all {} files: {:?}",
                    written.len(),
                    written
                );
            } else {
                println!(
                    "        [FAIL] Ingestion wrote {}/{} files: {:?}",
                    written.len(),
                    hr_files.len(),
                    written
                );
                loop4_pass = false;
            }
        }
        Err(e) => {
            println!("        [FAIL] Ingestion request failed: {e}");
            loop4_pass = false;
        }
    }

    if loop4_pass {
        println!("        Waiting for background embedder to index HR policies...");
        let hr_scope = simulacra_types::MemoryPath::parse("/mnt/hr-policies").unwrap();
        if !wait_for_index(&vector_index, embedder.as_ref(), &hr_scope, 15).await {
            println!("        [FAIL] HR policies were not indexed within 15s");
            loop4_pass = false;
        } else {
            println!("        [PASS] HR policies are searchable");
        }
    }

    // ── Loop 4 Phase B: Nova answers an employee question with a citation.
    println!("\n[L4/B] ┌─────────────────────────────────────┐");
    println!("        │ NOVA — answer employee RAG question │");
    println!("        └─────────────────────────────────────┘");

    let nova_rag_body = serde_json::json!({
        "agent_type": "nova",
        "task": concat!(
            "You are Nova, the ops generalist. An employee just asked you:\n",
            "  \"can I work remotely 4 days a week?\"\n",
            "\n",
            "Do EXACTLY these steps in order:\n",
            "\n",
            "STEP 1 — Call `semantic_search` ONCE with scope \"/mnt/hr-policies\"\n",
            "         and query \"remote work policy days allowed\".\n",
            "         Pick the top hit.\n",
            "\n",
            "STEP 2 — Call `memory_read_chunk` AT MOST ONCE on the top hit.\n",
            "\n",
            "STEP 3 — Call `file_write` ONCE to create\n",
            "         /proc/mailbox/employee-remote-work-answer.md.\n",
            "         The answer MUST:\n",
            "           - Quote or state the specific allowed limit (e.g. 3 days)\n",
            "           - Directly answer yes/no to the 4-day question\n",
            "             (the correct answer is NO — policy caps at 3)\n",
            "           - Include a citation line referencing the memory path\n",
            "             you read from (e.g. /mnt/hr-policies/remote.md)\n",
            "\n",
            "STEP 4 — Stop. End your turn.\n",
            "\n",
            "HARD CONSTRAINT: Do not exceed 5 turns total.\n"
        )
    });

    let mut nova_rag_id = String::new();
    let mut nova_rag_state = String::from("<skipped>");
    let mut nova_rag_text = String::new();

    if loop4_pass {
        match post_task(&client, &nova_rag_body).await {
            Ok(id) => nova_rag_id = id,
            Err(e) => {
                println!("        [FAIL] Nova RAG task creation: {e}");
                loop4_pass = false;
            }
        }
    }

    if loop4_pass {
        println!("        Nova RAG task ID: {nova_rag_id}");
        println!("        Waiting for Nova RAG task (up to 120s)...");
        nova_rag_state = wait_for_task(&client, &nova_rag_id).await;
        println!("        Nova RAG final state: {nova_rag_state}");
        if nova_rag_state != "completed" {
            println!("        [FAIL] Nova RAG did not complete");
            loop4_pass = false;
        } else {
            println!("        [PASS] Nova RAG task completed");
        }

        match fetch_markdown_artifact(&client, &nova_rag_id).await {
            Ok((path, text)) => {
                println!("        Retrieved artifact: {path} ({} bytes)", text.len());
                nova_rag_text = text;
            }
            Err(e) => {
                println!("        [FAIL] Could not retrieve Nova RAG artifact: {e}");
                loop4_pass = false;
            }
        }
    }

    if !nova_rag_text.is_empty() {
        println!("\n╭─ Nova RAG response to employee ─────────────────────────╮");
        for line in nova_rag_text.lines() {
            println!("│ {line}");
        }
        println!("╰─────────────────────────────────────────────────────────╯");

        let lower = nova_rag_text.to_lowercase();
        let nova_checks: &[(&[&str], &str)] = &[
            (&["3", "three"], "quotes the allowed limit (3 days)"),
            (
                &["no", "not allowed", "cannot", "can't", "can not"],
                "answers no to the 4-day question",
            ),
            (
                &[
                    "/mnt/hr-policies/remote.md",
                    "/mnt/hr-policies",
                    "remote.md",
                ],
                "cites the policy source path",
            ),
        ];
        for (needles, label) in nova_checks {
            let hit = needles.iter().any(|n| lower.contains(n));
            if hit {
                println!("        [PASS] {label}");
            } else {
                println!("        [WARN] does not {label}");
                loop4_pass = false;
            }
        }
    }

    loop_results.push((
        "Loop 4 — Nova RAG on ingested HR policies",
        loop4_pass,
        format!("nova_rag={nova_rag_id}({nova_rag_state})"),
    ));

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
            println!("       Top traces:");
            for t in slow.iter().take(8) {
                let tid = t["traceID"].as_str().unwrap_or("?");
                let root = t["rootSpanName"].as_str().unwrap_or("?");
                let span_count = t["spanCount"].as_u64().unwrap_or(0);
                let dur_ms = t["durationMs"].as_f64().unwrap_or(0.0);
                println!("         {root:<20} trace={tid}  spans={span_count}  {dur_ms:.1}ms");
            }
        }
        Ok(r) => println!("       Aniani returned {}", r.status()),
        Err(e) => println!("       Aniani unreachable: {e}"),
    }
    if let Err(e) = trace_provider.shutdown() {
        eprintln!("       Trace provider shutdown error: {e}");
    }

    // ── Final combined banner across all four virtual-coworker loops ─────
    let all_pass = loop_results.iter().all(|(_, ok, _)| *ok);
    println!("\n╭──────────────────────────────────────────────────────────╮");
    if all_pass {
        println!("│  RESULT: all virtual-coworker loops PASSED               │");
    } else {
        println!("│  RESULT: some virtual-coworker loops had gaps            │");
    }
    println!("╰──────────────────────────────────────────────────────────╯");
    for (label, ok, detail) in &loop_results {
        let tag = if *ok { "PASS" } else { "FAIL" };
        println!("  [{tag}] {label}");
        println!("         {detail}");
    }
    println!("  Memory dir:  {}", memory_dir.display());

    if !all_pass {
        std::process::exit(1);
    }
}

/// Post a task to the simulacra-server /api/v1/tasks/create endpoint and return
/// the task_id, or an error string describing why it could not be created.
async fn post_task(client: &reqwest::Client, body: &serde_json::Value) -> Result<String, String> {
    let resp = client
        .post(format!("http://127.0.0.1:{PORT}/api/v1/tasks/create"))
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("http error: {e}"))?;
    let status = resp.status();
    let parsed: serde_json::Value = resp.json().await.unwrap_or_default();
    if !parsed["ok"].as_bool().unwrap_or(false) {
        return Err(format!("status={status} body={parsed}"));
    }
    parsed["data"]["task_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("task_id missing in response: {parsed}"))
}

/// Fetch the first `.md` artifact from a task (falling back to the first
/// artifact if no markdown is present). Returns `(path, text)` or an error.
async fn fetch_markdown_artifact(
    client: &reqwest::Client,
    task_id: &str,
) -> Result<(String, String), String> {
    let list = client
        .get(format!(
            "http://127.0.0.1:{PORT}/api/v1/tasks/{task_id}/artifacts"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .map_err(|e| format!("list http error: {e}"))?;
    let list_body: serde_json::Value = list.json().await.unwrap_or_default();
    let artifacts = list_body["data"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if artifacts.is_empty() {
        return Err("no artifacts produced".into());
    }
    let path = artifacts
        .iter()
        .find_map(|a| a["path"].as_str().filter(|p| p.ends_with(".md")))
        .or_else(|| artifacts.first().and_then(|a| a["path"].as_str()))
        .ok_or_else(|| "artifact list had no path field".to_string())?
        .to_string();
    let resp = client
        .get(format!(
            "http://127.0.0.1:{PORT}/api/v1/tasks/{task_id}/artifacts/{path}"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .map_err(|e| format!("get http error: {e}"))?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("body read error: {e}"))?;
    Ok((path, text))
}

/// Wait for the BackgroundEmbedder to index something under `scope` so
/// `semantic_search` can return a hit. Returns true on success, false on
/// timeout. Mirrors the inline block used by Loop 3 so every loop waits
/// the same way.
async fn wait_for_index(
    vector_index: &Arc<dyn VectorIndex>,
    embedder: &dyn Embedder,
    scope: &simulacra_types::MemoryPath,
    timeout_secs: u64,
) -> bool {
    let tenant = TenantId::parse("demo").unwrap();
    let probe = match embedder.embed(&["wait probe"]) {
        Ok(mut v) if !v.is_empty() => v.remove(0),
        _ => return false,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let hits = vector_index
            .search(&tenant, scope, &probe, embedder.id(), 10, None)
            .unwrap_or_default();
        if !hits.is_empty() {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// POST /api/v1/ingestion with a synthetic hr-policies corpus. Returns the
/// list of written paths from the server response (used to assert that all
/// three files landed).
async fn ingest_hr_policies(
    client: &reqwest::Client,
    files: &[(&str, &str)],
) -> Result<Vec<String>, String> {
    use base64::Engine as _;
    let files_json: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, body)| {
            serde_json::json!({
                "path": path,
                "content": base64::engine::general_purpose::STANDARD.encode(body.as_bytes()),
            })
        })
        .collect();
    let body = serde_json::json!({
        "source": "hr-policies",
        "mode": "merge",
        "files": files_json,
    });
    let resp = client
        .post(format!("http://127.0.0.1:{PORT}/api/v1/ingestion"))
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("http error: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("status={status} body={text}"));
    }
    let parsed: serde_json::Value = resp.json().await.unwrap_or_default();
    if !parsed["ok"].as_bool().unwrap_or(false) {
        return Err(format!("non-ok response: {parsed}"));
    }
    let written = parsed["data"]["written"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(written
        .iter()
        .filter_map(|w| w["path"].as_str().map(String::from))
        .collect())
}

async fn wait_for_task(client: &reqwest::Client, task_id: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if std::time::Instant::now() > deadline {
            return "timeout".into();
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let resp = client
            .get(format!(
                "http://127.0.0.1:{PORT}/api/v1/tasks/{task_id}/status"
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
            print!("       state: {s}    \r");
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            if matches!(s.as_str(), "completed" | "failed" | "killed" | "cancelled") {
                println!();
                return s;
            }
        }
    }
}
