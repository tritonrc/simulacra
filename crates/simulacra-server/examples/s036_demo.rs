// S036 demo: file in → agent reasoning → file out.
//
// Proves the full enterprise task lifecycle:
//   1. Client POSTs task with attached CSV file
//   2. Simulacra seeds the file into /workspace/expenses.csv
//   3. Real agent reads the file, reasons about it, writes a report to /proc/mailbox/
//   4. Artifact is persisted to the durable ArtifactStore
//   5. Client retrieves the report via GET /api/v1/tasks/{id}/artifacts/summary.md
//
// Prereq:
//   ANTHROPIC_API_KEY=sk-...  (real LLM calls)
//   Obsidian on localhost:4320 (optional — traces)
//
// Run:
//   ANTHROPIC_API_KEY=sk-... cargo run -p simulacra-server --example s036_demo

use simulacra_server::*;
use std::collections::HashMap;
use std::sync::Arc;

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
                .with_service_name("simulacra-s036-demo")
                .build(),
        )
        .build();

    let tracer = provider.tracer("simulacra-s036-demo");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr);
    let filter =
        tracing_subscriber::EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| {
            "WARN,simulacra_server=INFO,simulacra_runtime=INFO,simulacra_vfs=DEBUG".into()
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

    // ── Env validation ───────────────────────────────────────────────────
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ERROR: ANTHROPIC_API_KEY must be set.");
        std::process::exit(1);
    }

    println!("\n=== S036 Demo: File In → Agent → File Out ===\n");

    // ── Step 1: Artifact store — use the engine's default location ──────
    // SimulacraEngine defaults to /tmp/simulacra-artifacts. AppState::with_engine reuses
    // the engine's store so HTTP reads and agent writes hit the same backend.
    let artifact_dir = std::path::PathBuf::from("/tmp/simulacra-artifacts");
    // Fresh run — clean any previous demo artifacts for this tenant only.
    let _ = std::fs::remove_dir_all(artifact_dir.join("demo"));
    println!("[1/6] Artifact store: {}", artifact_dir.display());

    // ── Step 2: Build Simulacra API server ───────────────────────────────────
    println!("[2/6] Building Simulacra API server on 127.0.0.1:9093...");

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
    agent_types.insert(
        "default".to_string(),
        simulacra_config::AgentTypeConfig {
            model: "claude-sonnet-4-6".into(),
            system_prompt: None,
            max_turns: Some(25),
            max_tokens: Some(80000),
            max_sub_agents: None,
            capabilities: Some(simulacra_config::CapabilitiesConfig {
                shell: false,
                javascript: true,
                python: false,
                network: vec![],
                mcp: vec![],
                paths_read: vec!["/**".into()],
                paths_write: vec!["/workspace/**".into(), "/proc/mailbox/**".into()],

                memory: None,
            }),
            skills: vec![],
            restart_policy: None,
            can_spawn: vec![],
        },
    );

    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "s036-demo".into(),
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

    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(simulacra_config, None)
            .await
            .expect("engine construction failed"),
    );
    // with_engine reuses engine.artifact_store() — HTTP reads hit the SAME store
    // that agents write to. No wiring divergence possible.
    let state = AppState::with_engine(
        Arc::clone(&task_manager),
        Arc::new(resolver),
        auth,
        Arc::clone(&engine),
    );
    let router = build_router(state, vec![], None);

    let simulacra_listener = tokio::net::TcpListener::bind("127.0.0.1:9093")
        .await
        .expect("failed to bind 9093");
    tokio::spawn(async move {
        axum::serve(simulacra_listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    println!("       Simulacra API server ready.");

    // ── Step 3: Create task WITH attached CSV file ──────────────────────
    println!("[3/6] Creating task with attached CSV (file-in)...");

    let csv_content = "\
vendor,amount,category,date
Acme Corp,12500.00,Software,2026-01-15
Globex Inc,4200.00,Office,2026-01-20
Initech,15750.00,Consulting,2026-02-01
Umbrella Corp,890.00,Travel,2026-02-10
Cyberdyne,22300.00,Software,2026-02-15
Massive Dynamic,560.00,Office,2026-02-28
Wayne Enterprises,18900.00,Consulting,2026-03-05
Stark Industries,750.00,Travel,2026-03-12
Oscorp,13200.00,Software,2026-03-18
Tyrell Corp,3400.00,Office,2026-03-25
";

    let client = reqwest::Client::new();
    let task_body = serde_json::json!({
        "task": concat!(
            "You have been given /workspace/expenses.csv — a list of Q1 vendor expenses.\n\n",
            "Do these steps:\n",
            "1. Read /workspace/expenses.csv using the file_read tool.\n",
            "2. Use js_exec to parse the CSV and compute:\n",
            "   - Total spend\n",
            "   - Any expenses over $10,000 (flagged as large)\n",
            "   - Spend broken down by category (Software, Consulting, Office, Travel)\n",
            "3. Write a Markdown report to /proc/mailbox/q1-expense-report.md with:\n",
            "   - Summary section (totals)\n",
            "   - 'Large Expenses (>$10k)' section listing each flagged expense\n",
            "   - 'Category Breakdown' section\n",
            "4. Generate a bar chart as SVG and write it to /proc/mailbox/category-chart.svg\n",
            "   The SVG should be a horizontal bar chart showing total spend per category.\n",
            "   Requirements:\n",
            "   - viewBox='0 0 600 300', width/height set\n",
            "   - Title text at the top: 'Q1 Spend by Category'\n",
            "   - One <rect> bar per category, width proportional to spend\n",
            "   - Label each bar with the category name and dollar amount\n",
            "   - Use distinct fill colors per category\n",
            "   - Must be valid well-formed XML, starting with <svg xmlns='http://www.w3.org/2000/svg' ...>\n",
            "   Use js_exec to build the SVG string, then file_write to save it.\n",
            "5. Confirm both artifacts were written by reading them back.\n"
        ),
        "files": {
            "expenses.csv": {
                "data": csv_content
            }
        }
    });

    let create_resp = client
        .post("http://127.0.0.1:9093/api/v1/tasks/create")
        .header("Authorization", "ApiKey demo-key")
        .header("Content-Type", "application/json")
        .json(&task_body)
        .send()
        .await
        .expect("task create request failed");

    let create_status = create_resp.status();
    let create_body: serde_json::Value = create_resp.json().await.expect("invalid JSON response");
    println!("       Response: {create_status}");

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
    println!("[4/6] Waiting for agent to process the file (up to 180s)...");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut final_state = String::new();
    loop {
        if std::time::Instant::now() > deadline {
            eprintln!("FAIL: Task did not complete within 90s");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let status_resp = client
            .get(format!(
                "http://127.0.0.1:9093/api/v1/tasks/{task_id}/status"
            ))
            .header("Authorization", "ApiKey demo-key")
            .send()
            .await;

        if let Ok(resp) = status_resp {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let state = body["data"]["state"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            print!("       State: {state}    \r");
            use std::io::Write as _;
            std::io::stdout().flush().ok();
            match state.as_str() {
                "completed" | "failed" | "killed" | "cancelled" => {
                    final_state = state;
                    println!();
                    break;
                }
                _ => continue,
            }
        }
    }

    println!("       Final state: {final_state}");

    // ── Step 5: List artifacts ───────────────────────────────────────────
    println!("[5/6] Listing artifacts via GET /artifacts...");

    let list_resp = client
        .get(format!(
            "http://127.0.0.1:9093/api/v1/tasks/{task_id}/artifacts"
        ))
        .header("Authorization", "ApiKey demo-key")
        .send()
        .await
        .expect("list artifacts request failed");

    let list_body: serde_json::Value = list_resp.json().await.expect("invalid list JSON");
    println!(
        "       {}",
        serde_json::to_string_pretty(&list_body).unwrap()
    );

    let artifacts = list_body["data"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    if artifacts.is_empty() {
        eprintln!("FAIL: No artifacts returned. Agent did not write to /proc/mailbox/");
        std::process::exit(1);
    }

    // ── Step 6: Retrieve both artifacts (file-out) ──────────────────────
    println!("\n[6/6] Retrieving artifact bytes (file-out)...");

    // Helper to fetch an artifact by path.
    async fn fetch_artifact(
        client: &reqwest::Client,
        task_id: &str,
        path: &str,
    ) -> (String, String, Vec<u8>) {
        let resp = client
            .get(format!(
                "http://127.0.0.1:9093/api/v1/tasks/{task_id}/artifacts/{path}"
            ))
            .header("Authorization", "ApiKey demo-key")
            .send()
            .await
            .expect("get artifact request failed");
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(none)")
            .to_string();
        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(none)")
            .to_string();
        let bytes = resp.bytes().await.expect("artifact bytes").to_vec();
        (ct, cd, bytes)
    }

    // Find the Markdown report and the SVG chart.
    let find_by = |suffix: &str| -> Option<String> {
        artifacts
            .iter()
            .find_map(|a| a["path"].as_str().filter(|p| p.ends_with(suffix)))
            .map(String::from)
    };

    let report_path = find_by(".md")
        .or_else(|| artifacts[0]["path"].as_str().map(String::from))
        .expect("no markdown artifact");
    let svg_path = find_by(".svg");

    // Fetch markdown report.
    println!("       Fetching: {report_path}");
    let (ct, cd, bytes) = fetch_artifact(&client, &task_id, &report_path).await;
    println!("       Content-Type:        {ct}");
    println!("       Content-Disposition: {cd}");
    println!("       Size:                {} bytes", bytes.len());
    println!("\n╭─ Markdown report ──────────────────────────────────────╮");
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        println!("│ {line}");
    }
    println!("╰────────────────────────────────────────────────────────╯\n");

    // Fetch SVG chart if present.
    let mut svg_info: Option<(String, Vec<u8>)> = None;
    if let Some(svg_path) = svg_path.clone() {
        println!("       Fetching: {svg_path}");
        let (svg_ct, svg_cd, svg_bytes) = fetch_artifact(&client, &task_id, &svg_path).await;
        println!("       Content-Type:        {svg_ct}");
        println!("       Content-Disposition: {svg_cd}");
        println!("       Size:                {} bytes", svg_bytes.len());

        // Save the SVG locally so the user can open it in a browser.
        let local_path = std::env::temp_dir().join("simulacra-s036-category-chart.svg");
        std::fs::write(&local_path, &svg_bytes).expect("write local svg");
        println!("       Saved locally:       {}", local_path.display());

        // Show the first ~600 chars of the SVG so we can see it's real.
        let svg_text = String::from_utf8_lossy(&svg_bytes);
        let preview_len = svg_text.len().min(600);
        println!("\n╭─ SVG preview (first {preview_len} bytes) ─────────────╮");
        for line in svg_text[..preview_len].lines().take(20) {
            println!("│ {line}");
        }
        if svg_text.len() > preview_len {
            println!("│ ... ({} more bytes)", svg_text.len() - preview_len);
        }
        println!("╰────────────────────────────────────────────────────────╯\n");
        svg_info = Some((svg_ct, svg_bytes));
    } else {
        println!("       [WARN] No SVG artifact found — agent did not generate the chart");
    }

    // ── Verification ─────────────────────────────────────────────────────
    let mut pass = true;

    if final_state != "completed" {
        println!("[FAIL] Task state: {final_state} (expected completed)");
        pass = false;
    } else {
        println!("[PASS] Task completed");
    }

    if bytes.is_empty() {
        println!("[FAIL] Markdown artifact is empty");
        pass = false;
    } else {
        println!("[PASS] Markdown artifact is {} bytes", bytes.len());
    }

    // SVG-specific verification.
    if let Some((svg_ct, svg_bytes)) = svg_info.as_ref() {
        if svg_ct == "image/svg+xml" {
            println!("[PASS] SVG Content-Type is image/svg+xml");
        } else {
            println!("[FAIL] SVG Content-Type is {svg_ct} (expected image/svg+xml)");
            pass = false;
        }
        let svg_text = String::from_utf8_lossy(svg_bytes);
        let svg_lower = svg_text.to_lowercase();
        if svg_lower.contains("<svg") && svg_lower.contains("</svg>") {
            println!("[PASS] SVG is well-formed (has <svg> root element)");
        } else {
            println!("[FAIL] SVG is malformed — missing <svg> root");
            pass = false;
        }
        if svg_lower.contains("<rect") {
            println!("[PASS] SVG contains bar rectangles");
        } else {
            println!("[FAIL] SVG has no <rect> elements");
            pass = false;
        }
        if svg_lower.contains("software") {
            println!("[PASS] SVG references Software category");
        } else {
            println!("[WARN] SVG does not reference Software category by name");
        }
    } else {
        println!("[FAIL] No SVG artifact produced");
        pass = false;
    }

    // Spot-check the markdown report actually reasons about the input data.
    let report_lower = text.to_lowercase();
    let mentions = [
        ("acme", "Acme Corp mentioned"),
        ("cyberdyne", "Cyberdyne mentioned"),
        ("software", "Software category mentioned"),
    ];
    for (needle, label) in mentions {
        if report_lower.contains(needle) {
            println!("[PASS] Report {label}");
        } else {
            println!("[WARN] Report does not mention {needle}");
        }
    }

    // Verify the artifact was written to durable storage on disk.
    let tenant_dir = artifact_dir.join("demo").join(&task_id);
    if tenant_dir.exists() {
        println!(
            "[PASS] Artifact persisted on disk at {}",
            tenant_dir.display()
        );
        let disk_entries: Vec<_> = std::fs::read_dir(&tenant_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        println!("       Files on disk: {disk_entries:?}");
    } else {
        println!(
            "[FAIL] Artifact NOT found on disk at {}",
            tenant_dir.display()
        );
        pass = false;
    }

    // ── Flush and query Obsidian traces ─────────────────────────────────
    println!("\n[Traces] Flushing OTLP spans to Obsidian...");
    if let Err(e) = trace_provider.force_flush() {
        println!("       Flush error: {e}");
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let obs_resp = client
        .get("http://localhost:4320/api/v1/diagnose")
        .query(&[("service", "simulacra-s036-demo")])
        .send()
        .await;
    match obs_resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let health = body["health_score"].as_f64().unwrap_or(0.0);
            let p99 = body["health_factors"]["latency_p99_ms"]
                .as_f64()
                .unwrap_or(0.0);
            let slow = body["slowest_traces"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            println!("       Health: {health:.0}/100, p99 latency: {p99:.1}ms");
            println!("       Slowest traces:");
            for t in slow.iter().take(5) {
                let tid = t["traceID"].as_str().unwrap_or("?");
                let root = t["rootSpanName"].as_str().unwrap_or("?");
                let span_count = t["spanCount"].as_u64().unwrap_or(0);
                let dur_ms = t["durationMs"].as_f64().unwrap_or(0.0);
                println!("         {root:<16} trace={tid}  spans={span_count}  {dur_ms:.1}ms");
            }
            if let Some(suggestions) = body["suggested_queries"].as_array() {
                for sq in suggestions {
                    if let Some(q) = sq["query"].as_str() {
                        println!("       Suggested: {q}");
                    }
                }
            }
        }
        Ok(r) => println!("       Obsidian returned {}", r.status()),
        Err(e) => println!("       Obsidian unreachable: {e}"),
    }

    if let Err(e) = trace_provider.shutdown() {
        eprintln!("       Trace provider shutdown error: {e}");
    }

    println!("\n=== Summary ===");
    println!("  Task ID:        {task_id}");
    println!("  Final state:    {final_state}");
    println!("  Artifacts:      {}", artifacts.len());
    println!("  Report path:    {report_path}");
    println!("  Store location: {}", artifact_dir.display());

    if pass {
        println!("\n  RESULT: S036 DEMO PASSED — file in → agent → file out\n");
    } else {
        println!("\n  RESULT: S036 DEMO FAILED\n");
        std::process::exit(1);
    }
}
