//! S048 dev server — boots simulacra-server with NoAuth + frontend + GraphQL.
//!
//! Usage:
//!   cargo run -p simulacra-server --example dev_server
//!
//! Then open http://127.0.0.1:8080 in a browser.
//!
//! What this wires together:
//!   - NoAuthProvider for both REST and GraphQL (dev_mode-equivalent)
//!   - In-memory catalog seeded with one example agent so the list view
//!     has something to show
//!   - Empty webhook/schedule lists
//!   - frontend_router() at /, graphql_router() at /graphql, REST at /api/v1/*

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};

use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, ChannelRepository, MemoryPoolRepository, SkillRepository,
    TenantRepository,
};
use simulacra_catalog::{Catalog, NewAgent};
use simulacra_graphql::auth::{GraphQLAuthProvider, NoAuthGraphQLProvider};
use simulacra_graphql::context::TenantResolver as GraphQLTenantResolver;
use simulacra_graphql::schema::{MutationRoot, QueryRoot};
use simulacra_graphql::tool_catalog::ToolCatalog;

use simulacra_server::auth::AuthProvider;
use simulacra_server::tenant::{BudgetPoolConfig, TenantConfig, TenantResolver};
use simulacra_server::{
    AppState, DefaultToolCatalog, GraphQLMount, NoAuthProvider, ServerConfig, SimulacraEngine,
    TaskManager, build_router,
};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};

struct OtelGuard {
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        let _ = self.meter_provider.force_flush();
        let _ = self.tracer_provider.force_flush();
        let _ = self.tracer_provider.shutdown();
    }
}

fn init_otlp_observability() -> Result<OtelGuard, Box<dyn std::error::Error>> {
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let endpoint =
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or("http://localhost:4320".into());
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name("simulacra-server-dev")
        .build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .build()?;
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()?;
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();
    global::set_meter_provider(meter_provider.clone());

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("INFO"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer("simulacra-server-dev")),
        )
        .try_init()?;

    println!("  - OTLP:    {endpoint}");

    Ok(OtelGuard {
        tracer_provider,
        meter_provider,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _otel = init_otlp_observability()?;

    // 1. Open an in-memory catalog and seed one tenant + one agent so the
    //    list view isn't empty on first open.
    let catalog = Catalog::open_in_memory()?;
    seed_default_tenant_and_agent(&catalog).await?;

    // 2. Cast each repo accessor into the trait-object Arc the rest of the
    //    stack (engine, GraphQL schema) consumes.
    let agents_repo: Arc<dyn AgentRepository> = Arc::new(catalog.agents());
    let skills_repo: Arc<dyn SkillRepository> = Arc::new(catalog.skills());
    let pools_repo: Arc<dyn MemoryPoolRepository> = Arc::new(catalog.memory_pools());
    let channels_repo: Arc<dyn ChannelRepository> = Arc::new(catalog.channels());
    let files_repo: Arc<dyn AgentFileRepository> = Arc::new(catalog.agent_files());
    let tenants_repo: Arc<dyn TenantRepository> = Arc::new(catalog.tenants());

    // 3. Build the engine off the same catalog so REST + GraphQL agree.
    //    Set SIMULACRA_DEV_MCP_URL=http://127.0.0.1:PORT/mcp to expose a real
    //    configured MCP server in the tool picker without advertising a
    //    phantom server when none is running.
    let dev_mcp_url = std::env::var("SIMULACRA_DEV_MCP_URL").ok();
    let dev_mcp_servers = dev_mcp_url
        .as_ref()
        .map(|_| vec!["fetcher".to_string()])
        .unwrap_or_default();
    let mut config_tenants = HashMap::new();
    config_tenants.insert(
        "default".to_string(),
        simulacra_config::TenantConfig {
            agent_type: "example-agent".to_string(),
            integrations: None,
            mcp_servers: Some(dev_mcp_servers.clone()),
        },
    );
    let simulacra_config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "simulacra-dev".into(),
            description: None,
        },
        agent_types: Default::default(),
        integrations: Default::default(),
        tenants: config_tenants,
        mcp: dev_mcp_url.map(|url| simulacra_config::McpConfig {
            servers: vec![simulacra_config::McpServerConfig {
                name: "fetcher".to_string(),
                transport: Some("http".to_string()),
                url: Some(url),
                module: None,
                env: None,
                network: vec![],
                wasi: None,
            }],
        }),
        task: None,
        vfs: simulacra_config::VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: simulacra_config::CatalogConfig::default(),
    };
    let default_tenant = tenants_repo.get_by_namespace("default").await?;
    let tool_catalog = DefaultToolCatalog::from_config_for_tenants(
        &simulacra_config,
        [(default_tenant.id.clone(), "default".to_string())],
    );
    let engine = Arc::new(SimulacraEngine::new(
        simulacra_config,
        None,
        Arc::clone(&agents_repo),
        Arc::clone(&skills_repo),
        Arc::clone(&pools_repo),
        Arc::clone(&tenants_repo),
    )?);

    // 4. REST TenantResolver — backed by an in-process HashMap of TenantConfig
    //    (separate from the catalog's tenant table). Seed the same "default"
    //    tenant the NoAuthProvider resolves to, otherwise /api/v1/* handlers
    //    return 403 with "no tenant resolved for identity".
    let mut tenants_map = HashMap::new();
    tenants_map.insert(
        "default".to_string(),
        TenantConfig {
            namespace: "default".into(),
            agent_type: "example-agent".into(),
            vfs_root: "/tmp/simulacra-dev".into(),
            budget_pool: BudgetPoolConfig {
                max_tokens: 100_000,
                max_cost: "5.00".into(),
            },
            hooks: vec![],
            integrations: vec![],
            mcp_servers: dev_mcp_servers,
        },
    );

    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants_map, Some("default".into())));
    let auth: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider::new("dev@local", "default"));
    let state = AppState::with_engine(task_manager, resolver, auth, engine);

    // 5. GraphQL mount. The schema receives only the repo data — the
    //    per-request GraphQLContext (tenant_id + principal) is injected by
    //    the auth middleware inside graphql_router.
    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::clone(&agents_repo))
    .data(Arc::clone(&skills_repo))
    .data(Arc::clone(&pools_repo))
    .data(Arc::clone(&channels_repo))
    .data(Arc::clone(&files_repo))
    .data(Arc::new(tool_catalog) as Arc<dyn ToolCatalog>)
    .finish();

    let graphql_auth: Arc<dyn GraphQLAuthProvider> =
        Arc::new(NoAuthGraphQLProvider::new("dev@local", "default"));
    let tenant_resolver = GraphQLTenantResolver::new(Arc::clone(&tenants_repo));

    let graphql_mount = GraphQLMount {
        schema,
        auth: graphql_auth,
        tenant_resolver,
    };

    // 6. Assemble the full router and serve it.
    let router = build_router(state, vec![], Some(graphql_mount)).layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
            .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
    );

    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 8080,
    };

    println!(
        "dev_server listening on http://{}:{}",
        config.host, config.port
    );
    println!("  - UI:      http://{}:{}/", config.host, config.port);
    println!(
        "  - GraphQL: http://{}:{}/graphql",
        config.host, config.port
    );
    println!(
        "  - REST:    http://{}:{}/api/v1/*",
        config.host, config.port
    );

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn seed_default_tenant_and_agent(
    catalog: &Catalog,
) -> Result<(), Box<dyn std::error::Error>> {
    let tenants = catalog.tenants();
    let tenant = tenants.get_or_create("default", Some("Default")).await?;

    let agents = catalog.agents();
    // Capability strings the engine actually understands (see
    // engine.rs::build_capability_token_from_resolved). Anything outside this
    // table — `py:exec`, `fs:read`, `fs:write`, etc. — is silently dropped
    // and gives the LLM no real tool, only a name to fight over.
    //
    // Built-in tools: file_read, file_write, file_edit, list_dir (always on,
    // gated by paths_read/paths_write patterns, default `/**`), shell_exec
    // (gated by `shell:exec`), js_exec (gated by `javascript`), py_exec
    // (gated by `python`; only registered when the `python` Cargo feature is
    // compiled in — on by default for simulacra-server).
    let capabilities: Vec<String> = vec!["shell:exec".into(), "javascript".into(), "python".into()];
    let skill_ids: [simulacra_catalog::SkillId; 0] = [];
    let channel_ids: [simulacra_catalog::ChannelId; 0] = [];
    let system_prompt = "You are a demo agent running inside the Simulacra runtime. \
        Your available tools are: file_read, file_write, file_edit, list_dir, \
        shell_exec, js_exec (JavaScript via QuickJS), and py_exec (Python via \
        the Monty runtime). \
        shell_exec runs against a virtual POSIX shell that supports: \
        echo, cat, ls (with -l/-a/-la flags), mkdir, cp, mv, rm, grep, sed, head, \
        tail, wc, find, sort, uniq, cut, tr, tee, curl, wget, cd, pwd, env, which, \
        export, plus the operators `&&`, `||`, `;`, `|`, and redirects. Cwd and env \
        vars are persistent across calls. \
        shell_exec also routes `node <script.js>`, `node -e <code>`, \
        `node -` for stdin, `python <script.py>`, `python -c <code>`, and `python -` \
        for stdin through the sandboxed \
        QuickJS/Monty runtimes. \
        js_exec is single-shot per call: globals, prototypes, and module \
        singleton state do not persist between invocations. \
        In js_exec, do file I/O with fs.readFileSync(path), \
        fs.writeFileSync(path, text), fs.appendFileSync(path, text), \
        fs.readdirSync(path), fs.statSync(path), fs.renameSync(old, new), \
        and fs.unlinkSync(path). \
        py_exec is single-shot per call (no globals between invocations) and \
        does not run on CPython — it's the Monty interpreter, so only its supported \
        stdlib is available (`sys`, `os`, `typing`, `asyncio`, `re`, `datetime`, `json`). \
        In py_exec only, do file/network I/O with these \
        host bridges as bare names (NOT inside a module): write_file(path, text), \
        read_file(path), list_dir(path), http_get(url), http_post(url, body), \
        env(name). You can also use pathlib.Path(path).read_text(), write_text(text), \
        iterdir(), exists(), is_file(), and is_dir(); these are mediated by the \
        sandbox. Do NOT write `from simulacra import …` or `import simulacra` — there \
        is no `simulacra` module. \
        When asked to run code, pick the language that matches the request and \
        use the matching tool name exactly. \
        Write final outputs to /proc/mailbox/<filename> with file_write — \
        anything written there is persisted as a downloadable artifact and \
        appears in the run UI's artifacts sidebar.";
    agents
        .create(
            &tenant.id,
            NewAgent {
                name: "example-agent",
                description: Some("Demo agent seeded by dev_server example."),
                system_prompt,
                model: "claude-opus-4-7",
                max_turns: Some(30),
                max_tokens: Some(120_000),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &capabilities,
                channel_ids: &channel_ids,
            },
        )
        .await?;

    Ok(())
}
