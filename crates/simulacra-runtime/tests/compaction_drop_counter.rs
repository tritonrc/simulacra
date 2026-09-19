//! The compaction drop counter, read in a process of its own.
//!
//! `RuntimeMeters` caches its instruments in a process-wide `OnceLock`, and
//! `opentelemetry::global::meter` binds an instrument to whichever
//! `MeterProvider` is global at that moment. Inside the lib-test binary a
//! sibling test always reaches the meters first and binds them to the no-op
//! provider, so the counter reads zero no matter what production does. Here the
//! binary holds only these two tests: the provider below is installed before
//! anything touches the meters, and no other test can add to the counter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use opentelemetry::global;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use rust_decimal::Decimal;
use simulacra_runtime::{AgentLoop, AgentLoopConfig, InMemoryJournalStorage};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ContextStrategy, FinishReason, Message, Provider, ProviderError,
    ProviderResponse, ResourceBudget, Role, TokenUsage, Tool, ToolCallMessage, ToolDefinition,
    ToolError,
};

const DROPPED: &str = "simulacra.context.messages_dropped";
const TURNS: &str = "simulacra.agent.turns";

// ── Metric capture ───────────────────────────────────────────────

/// One exported data point: its value and its attribute set. The attributes
/// are kept because a total cannot see them — a counter labelled with the agent
/// id sums to exactly the same number as an unlabelled one.
#[derive(Clone, Debug)]
struct Point {
    value: u64,
    attributes: Vec<String>,
}

#[derive(Clone, Default)]
struct CounterCapture(Arc<Mutex<HashMap<String, Vec<Point>>>>);

impl PushMetricExporter for CounterCapture {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let mut series: HashMap<String, Vec<Point>> = HashMap::new();
        for scope in metrics.scope_metrics() {
            for metric in scope.metrics() {
                if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                    let points = series.entry(metric.name().to_string()).or_default();
                    for point in sum.data_points() {
                        points.push(Point {
                            value: point.value(),
                            attributes: point
                                .attributes()
                                .map(|kv| format!("{}={}", kv.key, kv.value))
                                .collect(),
                        });
                    }
                }
            }
        }
        *self.0.lock().unwrap() = series;
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

struct Telemetry {
    provider: SdkMeterProvider,
    capture: CounterCapture,
}

impl Telemetry {
    fn install() -> &'static Self {
        static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
        TELEMETRY.get_or_init(|| {
            let capture = CounterCapture::default();
            // A long interval keeps the background reader out of the way; every
            // read below goes through force_flush instead.
            let reader = PeriodicReader::builder(capture.clone())
                .with_interval(Duration::from_secs(3600))
                .build();
            let provider = SdkMeterProvider::builder().with_reader(reader).build();
            global::set_meter_provider(provider.clone());
            Telemetry { provider, capture }
        })
    }

    /// Cumulative totals, so a read is a point in a running series rather than
    /// a value that starts at zero.
    fn total(&self, name: &str) -> u64 {
        self.points(name).iter().map(|point| point.value).sum()
    }

    fn points(&self, name: &str) -> Vec<Point> {
        self.provider
            .force_flush()
            .expect("the test reader should flush on demand");
        self.capture
            .0
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
}

/// Both tests read deltas off the same cumulative counters, so they must not
/// overlap.
async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

// ── Fakes ────────────────────────────────────────────────────────

struct CannedProvider {
    responses: Mutex<Vec<ProviderResponse>>,
}

impl Provider for CannedProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async {
            let mut responses = self
                .responses
                .lock()
                .map_err(|e| ProviderError::Other(format!("lock poisoned: {e}")))?;
            if responses.is_empty() {
                return Err(ProviderError::Other("no more canned responses".into()));
            }
            Ok(responses.remove(0))
        })
    }
}

fn response(tool_calls: Vec<ToolCallMessage>, finish_reason: FinishReason) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls,
            tool_call_id: None,
            provider_content: vec![],
        },
        token_usage: TokenUsage::default(),
        finish_reason,
        provider_response_id: None,
        model: "test-model".into(),
    }
}

/// Two tool turns then a text turn, which walks the conversation past the
/// compaction call site at lengths 2, 4 and 6.
fn two_tool_turns_then_text() -> CannedProvider {
    let call = |id: &str| {
        response(
            vec![ToolCallMessage {
                id: id.into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
            }],
            FinishReason::ToolUse,
        )
    };
    CannedProvider {
        responses: Mutex::new(vec![
            call("tc-1"),
            call("tc-2"),
            response(vec![], FinishReason::EndTurn),
        ]),
    }
}

struct EchoTool;

impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echoes input".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(arguments) })
    }
}

/// Keeps the system prompt plus the `keep_recent` most recent messages. The
/// conversation here always opens with the system prompt, so there is no
/// prompt-less branch to handle.
struct TruncatingContext {
    keep_recent: usize,
}

impl ContextStrategy for TruncatingContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        let (system, rest) = messages
            .split_first()
            .expect("the agent loop always seeds a system prompt");
        assert_eq!(system.role, Role::System);
        let mut kept = vec![system.clone()];
        kept.extend_from_slice(&rest[rest.len().saturating_sub(self.keep_recent)..]);
        kept
    }
}

struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}

fn build_loop(strategy: Box<dyn ContextStrategy>) -> AgentLoop {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(EchoTool))
        .expect("test tool registration should succeed");
    AgentLoop::new(
        AgentLoopConfig {
            agent_id: AgentId("test-agent".into()),
            system_prompt: "You are a test agent.".into(),
            model: "test-model".into(),
            max_turns: 10,
            capability: CapabilityToken::default(),
            context_token_limit: None,
        },
        Box::new(two_tool_turns_then_text()),
        tools,
        strategy,
        Arc::new(InMemoryJournalStorage::new()),
        ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5),
        None,
        None,
    )
}

// ── Tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_truncating_strategy_counts_the_messages_it_dropped() {
    let _exclusive = exclusive().await;
    let telemetry = Telemetry::install();
    let before = telemetry.total(DROPPED);

    let mut agent = build_loop(Box::new(TruncatingContext { keep_recent: 3 }));
    agent
        .run("one")
        .await
        .expect("three canned responses should complete the run");

    // The third compaction is the only one that drops: 6 messages in, 4 out.
    // Do not shrink this fixture. 6, 4 and 2 are mutually distinct, so a
    // counter fed the wrong quantity fails here: 1 per drop event, the input
    // length, or the output length are all visible as a wrong delta. A
    // two-message conversation can only drop 1, which `add(1)` also produces.
    assert_eq!(
        telemetry.total(DROPPED) - before,
        2,
        "{DROPPED} must move by the number of messages dropped, not merely exist"
    );

    // The agent id embeds a conversation id, so labelling this counter with it
    // would open an unbounded series per conversation. A total cannot see that:
    // only the data points can.
    let points = telemetry.points(DROPPED);
    assert_eq!(
        points.len(),
        1,
        "{DROPPED} must be a single series, got {points:?}"
    );
    assert!(
        points[0].attributes.is_empty(),
        "{DROPPED} must carry no attributes, got {points:?}"
    );
}

#[tokio::test]
async fn a_passthrough_strategy_counts_nothing() {
    let _exclusive = exclusive().await;
    let telemetry = Telemetry::install();
    let dropped_before = telemetry.total(DROPPED);
    let turns_before = telemetry.total(TURNS);

    let mut agent = build_loop(Box::new(PassthroughContext));
    agent
        .run("one")
        .await
        .expect("three canned responses should complete the run");

    // Without this control an unreadable metric pipeline would also produce a
    // zero delta below.
    assert_eq!(
        telemetry.total(TURNS) - turns_before,
        3,
        "{TURNS} must show all three turns, or nothing here was measured"
    );
    assert_eq!(
        telemetry.total(DROPPED) - dropped_before,
        0,
        "a strategy that drops nothing must leave {DROPPED} untouched"
    );
}
