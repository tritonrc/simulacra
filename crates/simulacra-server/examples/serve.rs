// Quick demo server — proves SimulacraEngine runs agents via HTTP API.
//
// Usage:
//   ANTHROPIC_API_KEY=sk-... cargo run -p simulacra-server --example serve
//
// Then:
//   curl -s -X POST http://127.0.0.1:9090/api/v1/tasks/create \
//     -H "Authorization: ApiKey demo-key" \
//     -H "Content-Type: application/json" \
//     -d '{"task": "Read /proc/agent/id and /proc/tools/ and report what you find."}'

use simulacra_server::*;
use std::collections::HashMap;
use std::sync::Arc;

fn init_otel_tracing() {
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
                .with_service_name("simulacra-server")
                .build(),
        )
        .build();

    let tracer = provider.tracer("simulacra-server");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
    let filter = tracing_subscriber::EnvFilter::new("INFO");

    let subscriber = tracing_subscriber::Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer);

    tracing::subscriber::set_global_default(subscriber).ok();
}

#[tokio::main]
async fn main() {
    init_otel_tracing();

    let task_manager = Arc::new(TaskManager::new());

    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "demo-key".into(),
            subject: "brian".into(),
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
                max_tokens: 30000,
                max_cost: "1.00".into(),
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
            backend: Default::default(),
            model: "claude-sonnet-4-6".into(),
            acp_profile: None,
            system_prompt: None,
            max_turns: Some(5),
            max_tokens: Some(30000),
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

    // Build integrations from env vars (if set)
    let mut integrations = HashMap::new();
    if std::env::var("LINEAR_API_KEY").is_ok() {
        integrations.insert(
            "linear".to_string(),
            simulacra_config::IntegrationConfig {
                auth: simulacra_config::AuthMethod::ApiKey {
                    key: "LINEAR_API_KEY".into(),
                    placement: "header".into(),
                },
                base_url: "https://api.linear.app".into(),
                description: Some("Linear project tracking".into()),
                rate_limit_rps: 0,
                skills_path: None,
            },
        );
    }

    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "demo".into(),
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

    // Build IntegrationRegistry if integrations are configured
    let integration_registry = if !simulacra_config.integrations.is_empty() {
        match simulacra_integration::IntegrationRegistry::from_config(
            &simulacra_config.integrations,
        ) {
            Ok(r) => {
                println!("  Integrations: {:?}", r.names());
                // Start background OAuth2 token refresh before wrapping in Arc.
                r.start_background_refresh().await;
                Some(Arc::new(r))
            }
            Err(e) => {
                eprintln!("  Warning: integration registry failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let engine =
        SimulacraEngine::new_with_in_memory_catalog(simulacra_config, integration_registry)
            .await
            .expect("engine construction failed");
    let state = AppState::with_engine(task_manager, Arc::new(resolver), auth, Arc::new(engine));
    let router = build_router(state, vec![], None);

    println!("\n  Simulacra API server running on http://127.0.0.1:9090");
    println!("  Try:");
    println!("    curl -s -X POST http://127.0.0.1:9090/api/v1/tasks/create \\");
    println!("      -H 'Authorization: ApiKey demo-key' \\");
    println!("      -H 'Content-Type: application/json' \\");
    println!("      -d '{{\"task\": \"Read /proc/agent/id and list /proc/tools/\"}}'");
    println!();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9090")
        .await
        .expect("failed to bind");
    axum::serve(listener, router).await.unwrap();
}
