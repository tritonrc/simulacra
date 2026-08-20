#![cfg(feature = "spawn")]

//! Live, per-run S060 observability acceptance check against local Aniani.
//!
//! Run with:
//! `cargo test -p simulacra-runtime --features spawn --test s060_slice4_aniani -- --ignored --nocapture`

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use rust_decimal::Decimal;
use serde_json::Value;
use simulacra_config::SimulacraConfig;
use simulacra_runtime::{
    AcpChildFuture, AcpChildRequest, AcpChildRuntime, ActivitySink, AgentInputQueue, AgentLoop,
    AgentLoopConfig, AgentLoopOutput, AgentSupervisor, AgentTaskFactory, CancellationToken,
    InMemoryJournalStorage, JoinChildAgentTool, NoopActivitySink, NoopContextStrategy,
    ProviderKind, SpawnAgentTool,
};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ExitReason, FinishReason, JournalStorage, Message, Provider,
    ProviderError, ProviderResponse, ResourceBudget, Role, TokenUsage, Tool, ToolCallMessage,
    ToolDefinition,
};
use simulacra_vfs::MemoryFs;
use tracing::Instrument;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

// OTLP export is explicitly flushed before every query, but Aniani indexes the
// three signals asynchronously. Keep the live R010 gate bounded while allowing
// a slow local indexer enough time to make this run's uniquely-correlated
// trace, log, and metric queryable.
const ANIANI_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const ANIANI_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(250);
const ANIANI_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

fn unique_run_values() -> (String, String, String) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after Unix epoch")
        .as_nanos();
    let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let suffix = format!("{:x}-{:x}-{sequence:x}", std::process::id(), nanos);
    (
        format!("simulacra-s060-{suffix}"),
        format!("session-{suffix}"),
        format!("S060-SECRET-{suffix}"),
    )
}

fn aniani_port() -> u16 {
    std::env::var("ANIANI_PORT")
        .unwrap_or_else(|_| "4320".into())
        .parse()
        .expect("ANIANI_PORT should be a u16")
}

fn aniani_reachable(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}")
            .parse()
            .expect("Aniani socket address"),
        Duration::from_secs(2),
    )
    .is_ok()
}

fn form_encode(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn aniani_get(port: u16, path: &str, field: &str, query: &str) -> Value {
    let query_string = format!("{field}={}", form_encode(query));
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .expect("local Aniani must remain reachable during the live test");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set Aniani read timeout");
    write!(
        stream,
        "GET {path}?{query_string} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("write Aniani query");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read Aniani response");
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response should contain headers");
    assert!(headers.contains(" 200 "), "Aniani query failed: {response}");
    serde_json::from_str(body).expect("Aniani response should be JSON")
}

fn result_series(response: &Value) -> &[Value] {
    response
        .pointer("/data/result")
        .and_then(Value::as_array)
        .expect("PromQL/LogQL response should contain data.result")
}

fn has_trace_for_service(response: &Value, service: &str) -> bool {
    response
        .get("traces")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|trace| {
            trace
                .get("rootServiceName")
                .and_then(Value::as_str)
                .is_some_and(|found| found == service)
        })
}

fn wait_for_query(
    port: u16,
    path: &str,
    field: &str,
    query: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let start = Instant::now();
    let mut last = Value::Null;
    let mut retry_delay = ANIANI_INITIAL_RETRY_DELAY;
    while start.elapsed() < ANIANI_QUERY_TIMEOUT {
        last = aniani_get(port, path, field, query);
        if predicate(&last) {
            return last;
        }

        let remaining = ANIANI_QUERY_TIMEOUT.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(retry_delay.min(remaining));
        retry_delay = (retry_delay * 2).min(ANIANI_MAX_RETRY_DELAY);
    }
    panic!(
        "Aniani did not satisfy query {query:?} within {ANIANI_QUERY_TIMEOUT:?}; last response: {last}"
    )
}

struct LogEventVisitor<'a> {
    record: &'a mut SdkLogRecord,
}

impl tracing::field::Visit for LogEventVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.record.set_body(format!("{value:?}").into());
        } else {
            self.record.add_attribute(
                opentelemetry::Key::new(field.name()),
                AnyValue::from(format!("{value:?}")),
            );
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.record.set_body(value.to_owned().into());
        } else {
            self.record.add_attribute(
                opentelemetry::Key::new(field.name()),
                AnyValue::from(value.to_owned()),
            );
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record
            .add_attribute(opentelemetry::Key::new(field.name()), value);
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record
            .add_attribute(opentelemetry::Key::new(field.name()), value);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        let value = i64::try_from(value)
            .map(AnyValue::Int)
            .unwrap_or_else(|_| AnyValue::String(value.to_string().into()));
        self.record
            .add_attribute(opentelemetry::Key::new(field.name()), value);
    }
}

struct OtelLogLayer {
    logger: SdkLogger,
}

impl OtelLogLayer {
    fn new(provider: &SdkLoggerProvider) -> Self {
        Self {
            logger: provider.logger("simulacra-s060-live-test"),
        }
    }
}

impl<S> Layer<S> for OtelLogLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let severity = match *metadata.level() {
            tracing::Level::ERROR => Severity::Error,
            tracing::Level::WARN => Severity::Warn,
            tracing::Level::INFO => Severity::Info,
            tracing::Level::DEBUG => Severity::Debug,
            tracing::Level::TRACE => Severity::Trace,
        };
        let mut record = self.logger.create_log_record();
        record.set_target(metadata.target());
        record.set_event_name(metadata.name());
        record.set_severity_number(severity);
        record.set_severity_text(metadata.level().as_str());
        event.record(&mut LogEventVisitor {
            record: &mut record,
        });
        self.logger.emit(record);
    }
}

struct OtelProviders {
    tracer: SdkTracerProvider,
    meter: SdkMeterProvider,
    logger: SdkLoggerProvider,
}

fn init_otlp(port: u16, service: &str, session: &str) -> (OtelProviders, impl tracing::Subscriber) {
    let endpoint = format!("http://127.0.0.1:{port}");
    let resource = Resource::builder()
        .with_service_name(service.to_owned())
        .with_attribute(KeyValue::new("simulacra.session.id", session.to_owned()))
        .build();

    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .build()
        .expect("build OTLP span exporter");
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();
    let trace_layer =
        tracing_opentelemetry::layer().with_tracer(tracer.tracer("simulacra-s060-live-test"));

    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()
        .expect("build OTLP metric exporter");
    let meter = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource.clone())
        .build();
    global::set_meter_provider(meter.clone());

    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .build()
        .expect("build OTLP log exporter");
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource)
        .build();
    let log_layer = OtelLogLayer::new(&logger);

    (
        OtelProviders {
            tracer,
            meter,
            logger,
        },
        tracing_subscriber::registry()
            .with(trace_layer)
            .with(log_layer),
    )
}

fn assistant_response(message: Message, model: &str) -> ProviderResponse {
    ProviderResponse {
        message,
        token_usage: TokenUsage {
            input_tokens: 2,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("s060-live-response".into()),
        model: model.into(),
    }
}

fn tool_call_response(id: &str, name: &str, arguments: Value) -> ProviderResponse {
    let mut response = assistant_response(
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: id.into(),
                name: name.into(),
                arguments,
            }],
            tool_call_id: None,
            provider_content: vec![],
        },
        "parent-model",
    );
    response.finish_reason = FinishReason::ToolUse;
    response
}

fn latest_tool_json(messages: &[Message]) -> Value {
    let content = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .expect("parent provider should receive the previous tool result")
        .content
        .clone();
    serde_json::from_str(&content).expect("tool result should be JSON")
}

struct ParentProvider {
    stage: Mutex<usize>,
    secret: String,
    children: Arc<Mutex<Vec<(String, String)>>>,
}

impl Provider for ParentProvider {
    fn chat<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut stage = self.stage.lock().expect("parent provider stage lock");
            let response = match *stage {
                0 => tool_call_response(
                    "spawn-native",
                    "spawn_agent",
                    serde_json::json!({
                        "placement": "in_process",
                        "instructions": format!("native instructions {}", self.secret),
                        "task": format!("native task {}", self.secret),
                        "budget": {
                            "max_tokens": 64,
                            "max_turns": 2,
                            "max_cost": "1",
                            "max_sub_agents": 1
                        }
                    }),
                ),
                1 => {
                    let ack = latest_tool_json(messages);
                    assert_eq!(ack["placement"], "in_process");
                    let child_id = ack["child_id"]
                        .as_str()
                        .expect("native acknowledgement child_id")
                        .to_owned();
                    self.children
                        .lock()
                        .expect("child capture lock")
                        .push(("in_process".into(), child_id.clone()));
                    tool_call_response(
                        "join-native",
                        "join_child_agent",
                        serde_json::json!({"child_id": child_id}),
                    )
                }
                2 => tool_call_response(
                    "spawn-acp",
                    "spawn_agent",
                    serde_json::json!({
                        "placement": "workspace",
                        "instructions": format!("ACP instructions {}", self.secret),
                        "task": format!("ACP task {}", self.secret),
                        "budget": {
                            "max_tokens": 64,
                            "max_turns": 2,
                            "max_cost": "1",
                            "max_sub_agents": 1
                        }
                    }),
                ),
                3 => {
                    let ack = latest_tool_json(messages);
                    assert_eq!(ack["placement"], "workspace");
                    let child_id = ack["child_id"]
                        .as_str()
                        .expect("ACP acknowledgement child_id")
                        .to_owned();
                    self.children
                        .lock()
                        .expect("child capture lock")
                        .push(("workspace".into(), child_id.clone()));
                    tool_call_response(
                        "join-acp",
                        "join_child_agent",
                        serde_json::json!({"child_id": child_id}),
                    )
                }
                4 => assistant_response(
                    Message {
                        role: Role::Assistant,
                        content: "both children joined".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                        provider_content: vec![],
                    },
                    "parent-model",
                ),
                unexpected => panic!("unexpected parent provider stage {unexpected}"),
            };
            *stage += 1;
            Ok(response)
        })
    }
}

struct ChildProvider;

impl Provider for ChildProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            Ok(assistant_response(
                Message {
                    role: Role::Assistant,
                    content: "native child complete".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                },
                "child-model",
            ))
        })
    }
}

struct FailingChildProvider {
    error: String,
}

impl Provider for FailingChildProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move { Err(ProviderError::Other(self.error.clone())) })
    }
}

struct RecordingAcpRuntime {
    requests: Arc<Mutex<Vec<AcpChildRequest>>>,
}

impl AcpChildRuntime for RecordingAcpRuntime {
    fn start_child(
        &self,
        request: AcpChildRequest,
        _cancellation: CancellationToken,
        _activity_sink: Arc<dyn ActivitySink>,
        _input_queue: AgentInputQueue,
    ) -> AcpChildFuture {
        self.requests
            .lock()
            .expect("ACP request lock")
            .push(request);
        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![Message {
                    role: Role::Assistant,
                    content: "ACP child complete".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_content: vec![],
                }],
                token_usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
                reported_tool_uses: Some(0),
                used_turns: 1,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

fn s060_config() -> SimulacraConfig {
    let config: SimulacraConfig = toml::from_str(
        r#"
[project]
name = "s060-live-aniani"

[agent_types.root]
model = "parent-model"
allowed_child_placements = ["in_process", "workspace"]

[child_placements.in_process]
backend = "native"
model = "child-model"

[child_placements.workspace]
backend = "acp"
acp_profile = "workspace-test-profile"
"#,
    )
    .expect("S060 live config should parse");
    config.validate().expect("S060 live config should validate");
    config
}

fn spawn_budget() -> ResourceBudget {
    ResourceBudget::new(2_048, 16, Decimal::new(10, 0), 2)
}

fn assert_live_trace(port: u16, service: &str, child_id: &str, placement: &str, backend: &str) {
    let query = format!(
        r#"{{ name = "s060_live_parent" }} >> {{ name = "create_agent" && span.gen_ai.agent.name = "{child_id}" && span.simulacra.child.placement = "{placement}" && span.simulacra.child.backend = "{backend}" }}"#
    );
    wait_for_query(port, "/api/search", "q", &query, |response| {
        has_trace_for_service(response, service)
    });
}

/// The normal parent-agent path is `invoke_agent -> tool_invoke(spawn_agent)
/// -> create_agent`; the failure fixture below calls the tool directly, so its
/// `create_agent` span is instead a direct descendant of `s060_live_parent`.
fn assert_live_tool_spawn_topology(
    port: u16,
    service: &str,
    child_id: &str,
    placement: &str,
    backend: &str,
) {
    let query = format!(
        r#"{{ name = "tool_invoke" && span.gen_ai.tool.name = "spawn_agent" }} > {{ name = "create_agent" && span.gen_ai.agent.name = "{child_id}" && span.simulacra.child.placement = "{placement}" && span.simulacra.child.backend = "{backend}" }}"#
    );
    wait_for_query(port, "/api/search", "q", &query, |response| {
        has_trace_for_service(response, service)
    });
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the real local Aniani endpoint on ANIANI_PORT (default 4320)"]
async fn s060_a42_live_run_exports_native_and_acp_telemetry_without_secret_labels_or_logs() {
    let port = aniani_port();
    assert!(
        aniani_reachable(port),
        "local Aniani is required at 127.0.0.1:{port}; start it per R010"
    );
    let (service, session, secret) = unique_run_values();
    let (providers, subscriber) = init_otlp(port, &service, &session);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let capability = CapabilityToken {
        spawn_placements: vec!["in_process".into(), "workspace".into()],
        ..Default::default()
    };
    let shared_budget = Arc::new(Mutex::new(spawn_budget()));
    let journal = Arc::new(InMemoryJournalStorage::new());
    let activity: Arc<dyn ActivitySink> = Arc::new(NoopActivitySink);
    let acp_requests = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(AgentTaskFactory {
        config: s060_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::clone(&activity),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: Some(Arc::new(|_kind, _model| Ok(Box::new(ChildProvider)))),
        acp_child_runtime: Some(Arc::new(RecordingAcpRuntime {
            requests: Arc::clone(&acp_requests),
        })),
    });
    let mut supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&shared_budget),
        factory,
    );
    supervisor.set_journal_storage(Arc::clone(&journal) as Arc<dyn JournalStorage>);
    let parent_id = AgentId(format!("parent-{session}"));
    supervisor.set_root_agent_id(parent_id.clone());
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = tokio::spawn(async move { supervisor.run_actor_loop(receiver).await });

    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(SpawnAgentTool {
            sender: sender.clone(),
            allowed_placements: capability.spawn_placements.clone(),
            activity_sink: Arc::clone(&activity),
            parent_id: parent_id.clone(),
            parent_budget: Arc::clone(&shared_budget),
            guidance: None,
        }))
        .expect("register real spawn tool");
    tools
        .register(Box::new(JoinChildAgentTool {
            sender: sender.clone(),
            caller_id: parent_id.clone(),
        }))
        .expect("register real join tool");

    let children = Arc::new(Mutex::new(Vec::new()));
    let provider = ParentProvider {
        stage: Mutex::new(0),
        secret: secret.clone(),
        children: Arc::clone(&children),
    };
    let mut parent_loop = AgentLoop::new(
        AgentLoopConfig {
            agent_id: parent_id,
            system_prompt: "orchestrate two bounded children".into(),
            model: "parent-model".into(),
            max_turns: 8,
            capability: capability.clone(),
            context_token_limit: None,
        },
        Box::new(provider),
        tools,
        Box::new(NoopContextStrategy),
        Arc::clone(&journal) as Arc<dyn JournalStorage>,
        ResourceBudget::new(4_096, 8, Decimal::new(20, 0), 2),
        Some(Arc::clone(&activity)),
        None,
    );

    let parent_span = tracing::info_span!(
        "s060_live_parent",
        simulacra.session.id = session.as_str(),
        service.name = service.as_str()
    );
    let output = parent_loop
        .run("spawn and join one child in each configured placement")
        .instrument(parent_span.clone())
        .await
        .expect("real parent loop should spawn and join both children");
    assert_eq!(output.exit_reason, ExitReason::Complete);
    drop(parent_loop);
    drop(sender);
    actor.await.expect("supervisor actor should stop cleanly");
    // Aniani indexes a trace only after its root span ends. Finish this root
    // before flushing and polling so the bounded retry measures ingestion,
    // rather than waiting on a span this test is still deliberately holding.
    drop(parent_span);

    let children = children.lock().expect("child capture lock").clone();
    assert_eq!(children.len(), 2, "parent must observe two real spawn acks");
    let native_child = children
        .iter()
        .find(|(placement, _)| placement == "in_process")
        .map(|(_, id)| id.clone())
        .expect("native child id");
    let acp_child = children
        .iter()
        .find(|(placement, _)| placement == "workspace")
        .map(|(_, id)| id.clone())
        .expect("ACP child id");
    assert_ne!(native_child, acp_child);

    {
        let acp_requests = acp_requests.lock().expect("ACP request lock");
        assert_eq!(acp_requests.len(), 1);
        assert_eq!(acp_requests[0].child_id.0, acp_child);
        assert_eq!(acp_requests[0].placement, "workspace");
        assert_eq!(
            acp_requests[0].instructions.as_deref(),
            Some(format!("ACP instructions {secret}").as_str())
        );
        assert_eq!(acp_requests[0].task, format!("ACP task {secret}"));
    }

    providers.logger.force_flush().expect("flush OTLP logs");
    providers.meter.force_flush().expect("flush OTLP metrics");
    providers.tracer.force_flush().expect("flush OTLP traces");
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_live_trace(port, &service, &native_child, "in_process", "native");
    assert_live_trace(port, &service, &acp_child, "workspace", "acp");
    assert_live_tool_spawn_topology(port, &service, &native_child, "in_process", "native");
    assert_live_tool_spawn_topology(port, &service, &acp_child, "workspace", "acp");

    for (child_id, instruction_length) in [
        (&native_child, format!("native instructions {secret}").len()),
        (&acp_child, format!("ACP instructions {secret}").len()),
    ] {
        let query = format!(r#"{{service="{service}"}}"#);
        let logs = wait_for_query(port, "/loki/api/v1/query", "query", &query, |response| {
            let encoded = serde_json::to_string(result_series(response)).unwrap_or_default();
            !result_series(response).is_empty()
                && encoded.contains(child_id)
                && encoded.contains("instruction_length_bytes")
                && encoded.contains(&instruction_length.to_string())
        });
        assert!(
            !result_series(&logs).is_empty(),
            "current child must have an instruction-length log"
        );
    }

    let secret_query = format!(r#"{{service="{service}"}}"#);
    let leaked_logs = aniani_get(port, "/loki/api/v1/query", "query", &secret_query);
    let encoded_logs = serde_json::to_string(result_series(&leaked_logs)).expect("serialize logs");
    assert!(
        !encoded_logs.contains(&secret),
        "this run's raw instruction/task secret leaked into logs: {leaked_logs}"
    );

    let metric_query = format!(r#"simulacra_agent_turns{{"simulacra.agent.id"="{native_child}"}}"#);
    let metrics = wait_for_query(port, "/api/v1/query", "query", &metric_query, |response| {
        !result_series(response).is_empty()
    });
    let metric_series = result_series(&metrics);
    let encoded_metrics = serde_json::to_string(metric_series).expect("serialize metric series");
    assert!(encoded_metrics.contains(&native_child));
    assert!(!encoded_metrics.contains(&secret));
    for forbidden in ["task", "instructions", "skill", "focus"] {
        assert!(
            !metric_series.iter().any(|series| {
                series
                    .get("metric")
                    .and_then(Value::as_object)
                    .is_some_and(|labels| labels.keys().any(|key| key.contains(forbidden)))
            }),
            "forbidden metric label {forbidden} in this run: {metrics}"
        );
    }

    // The live check deliberately drives the real native child execution path
    // through Provider::chat rather than manufacturing a tracing event. This
    // complements the deterministic in-process capture tests with an OTLP /
    // LogQL assertion that the exported failure log is both categorized and
    // redacted.
    let failure_parent_id = AgentId(format!("parent-failure-{session}"));
    let failure_budget = Arc::new(Mutex::new(spawn_budget()));
    let failure_journal = Arc::new(InMemoryJournalStorage::new());
    let provider_error = format!("provider failure {secret}");
    let provider_error_for_factory = provider_error.clone();
    let failure_factory = Arc::new(AgentTaskFactory {
        config: s060_config(),
        provider_kind: ProviderKind::Anthropic,
        vfs: Arc::new(MemoryFs::new()),
        journal: Arc::clone(&failure_journal) as Arc<dyn JournalStorage>,
        activity_sink: Arc::new(NoopActivitySink),
        parent_capability: capability.clone(),
        allowed_mcp_servers: None,
        supervisor_sender: None,
        pipeline: None,
        script_executor: None,
        child_cell_configurator: None,
        child_tool_registrar: None,
        child_provider_factory: Some(Arc::new(move |_kind, _model| {
            Ok(Box::new(FailingChildProvider {
                error: provider_error_for_factory.clone(),
            }) as Box<dyn Provider>)
        })),
        acp_child_runtime: None,
    });
    let mut failure_supervisor = AgentSupervisor::with_task_factory_and_shared_budget(
        capability.clone(),
        Arc::clone(&failure_budget),
        failure_factory,
    );
    failure_supervisor.set_root_agent_id(failure_parent_id.clone());
    failure_supervisor.set_journal_storage(Arc::clone(&failure_journal) as Arc<dyn JournalStorage>);
    let (failure_sender, failure_receiver) = tokio::sync::mpsc::channel(8);
    let failure_actor =
        tokio::spawn(async move { failure_supervisor.run_actor_loop(failure_receiver).await });
    let failure_spawn = SpawnAgentTool {
        sender: failure_sender.clone(),
        allowed_placements: vec!["in_process".into()],
        activity_sink: Arc::new(NoopActivitySink),
        parent_id: failure_parent_id.clone(),
        parent_budget: Arc::clone(&failure_budget),
        guidance: None,
    };
    let failure_join = JoinChildAgentTool {
        sender: failure_sender.clone(),
        caller_id: failure_parent_id,
    };
    let failure_parent_span = tracing::info_span!(
        "s060_live_parent",
        simulacra.session.id = session.as_str(),
        service.name = service.as_str()
    );
    let failure_ack = failure_spawn
        .call(
            serde_json::json!({
                "placement": "in_process",
                "instructions": format!("failure instructions {secret}"),
                "task": format!("failure task {secret}"),
                "budget": {
                    "max_tokens": 64,
                    "max_turns": 2,
                    "max_cost": "1",
                    "max_sub_agents": 1
                }
            }),
            &capability,
        )
        .instrument(failure_parent_span.clone())
        .await
        .expect("native provider failure still returns a running acknowledgement");
    let failed_child_id = failure_ack["child_id"]
        .as_str()
        .expect("failure acknowledgement child id")
        .to_owned();
    let failure_terminal = failure_join
        .call(
            serde_json::json!({"child_id": failed_child_id}),
            &capability,
        )
        .instrument(failure_parent_span.clone())
        .await
        .expect("native provider failure should settle through join");
    assert_eq!(failure_terminal["status"], "failed");
    assert!(
        failure_terminal["message"]
            .as_str()
            .is_some_and(|message| message.contains(&provider_error)),
        "the terminal result must prove the real Provider::chat failure occurred"
    );
    drop(failure_spawn);
    drop(failure_join);
    drop(failure_sender);
    failure_actor
        .await
        .expect("failure supervisor actor should stop cleanly");
    drop(failure_parent_span);

    providers.logger.force_flush().expect("flush failure log");
    providers
        .meter
        .force_flush()
        .expect("flush failure metrics");
    providers.tracer.force_flush().expect("flush failure trace");
    assert_live_trace(port, &service, &failed_child_id, "in_process", "native");
    let failure_query = format!(r#"{{service="{service}"}}"#);
    let failure_logs = wait_for_query(
        port,
        "/loki/api/v1/query",
        "query",
        &failure_query,
        |response| {
            let encoded = serde_json::to_string(result_series(response)).unwrap_or_default();
            encoded.contains(&failed_child_id)
                && encoded.contains("WARN")
                && encoded.contains("error_category")
                && encoded.contains("provider")
        },
    );
    let encoded_failure_logs =
        serde_json::to_string(result_series(&failure_logs)).expect("serialize failure logs");
    assert!(
        !encoded_failure_logs.contains(&provider_error)
            && !encoded_failure_logs.contains(&format!("failure task {secret}"))
            && !encoded_failure_logs.contains(&format!("failure instructions {secret}")),
        "exported provider-failure WARN must redact raw error, task, and instructions: {failure_logs}"
    );
    assert!(
        encoded_failure_logs.contains("error_category")
            && encoded_failure_logs.contains("provider"),
        "exported provider-failure WARN must retain only the bounded provider category: {failure_logs}"
    );
    // A provider failure occurs before its child completes a turn, so it need
    // not create a child-specific turn series. Query every turn metric from
    // this uniquely-correlated service instead: that keeps the assertion
    // meaningful whether a failed child records zero or later gains a turn
    // metric, while requiring no failure category on any exported metric.
    let failure_metric_query = format!(r#"simulacra_agent_turns{{service="{service}"}}"#);
    let failure_metrics = wait_for_query(
        port,
        "/api/v1/query",
        "query",
        &failure_metric_query,
        |response| !result_series(response).is_empty(),
    );
    assert!(
        result_series(&failure_metrics).iter().any(|series| {
            series
                .pointer("/metric/simulacra.agent.id")
                .and_then(Value::as_str)
                .is_some_and(|agent_id| agent_id == native_child)
        }),
        "the uniquely-correlated service must retain its successful child metric: {failure_metrics}"
    );
    assert!(
        result_series(&failure_metrics).iter().all(|series| {
            series
                .get("metric")
                .and_then(Value::as_object)
                .is_some_and(|labels| !labels.contains_key("error_category"))
        }),
        "a failed child must not add error_category as a metric label: {failure_metrics}"
    );

    providers.logger.shutdown().expect("shutdown OTLP logger");
    providers.meter.shutdown().expect("shutdown OTLP meter");
    providers.tracer.shutdown().expect("shutdown OTLP tracer");
}
