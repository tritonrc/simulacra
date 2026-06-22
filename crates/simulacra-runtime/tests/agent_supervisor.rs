use rust_decimal::Decimal;
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, AgentLoopOutput, AgentSupervisor, BoxTaskFuture, CancellationToken,
    InMemoryJournalStorage, MessagePriority, RestartStrategy, RuntimeError, SpawnConfig,
    SupervisorMessage, SupervisorPayload, TaskFactory,
};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ContextStrategy, ExitReason, FinishReason, JournalStorage, Message,
    Provider, ProviderError, ProviderResponse, ResourceBudget, Role, TokenUsage, Tool,
    ToolCallMessage, ToolDefinition, ToolError,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
    current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            level: event.metadata().level().to_string(),
            current_span: ctx.lookup_current().map(|span| span.name().to_string()),
            fields,
        });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

#[allow(clippy::type_complexity)]
fn setup_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    (subscriber, spans, events)
}

struct FakeProvider {
    responses: Mutex<Vec<ProviderResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut responses = self
                .responses
                .lock()
                .map_err(|err| ProviderError::Other(format!("lock poisoned: {err}")))?;

            if responses.is_empty() {
                return Err(ProviderError::Other(
                    "FakeProvider: no more canned responses".into(),
                ));
            }

            Ok(responses.remove(0))
        })
    }
}

struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
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

fn text_response(content: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }
}

fn tool_call_response(tool_name: &str, arguments: serde_json::Value) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "tc-1".into(),
                name: tool_name.into(),
                arguments,
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-2".into()),
        model: "test-model".into(),
    }
}

fn default_budget() -> ResourceBudget {
    ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5)
}

fn default_config() -> AgentLoopConfig {
    AgentLoopConfig {
        agent_id: AgentId("test-agent".into()),
        system_prompt: "You are a supervisor test agent.".into(),
        model: "test-model".into(),
        max_turns: 10,
        capability: CapabilityToken::default(),
    }
}

fn build_loop(
    provider: FakeProvider,
    tools: ToolRegistry,
    journal: Arc<dyn JournalStorage>,
    budget: ResourceBudget,
) -> AgentLoop {
    AgentLoop::new(
        default_config(),
        Box::new(provider),
        tools,
        Box::new(PassthroughContext),
        journal,
        budget,
        None,
        None,
    )
}

fn spawn_config(
    agent_id: &str,
    parent_id: &str,
    capability: CapabilityToken,
    budget: ResourceBudget,
    restart_strategy: RestartStrategy,
) -> SpawnConfig {
    SpawnConfig {
        agent_id: AgentId(agent_id.into()),
        parent_id: AgentId(parent_id.into()),
        capability: Some(capability),
        budget,
        restart_strategy,
        agent_type: Some(String::new()),
        task: String::new(),
        system_prompt: None,
        tier: None,
        resolved_tier: None,
    }
}

/// A TaskFactory whose tasks resolve immediately with a benign completed
/// output. Used by tests that care about the spawn bookkeeping (spans,
/// cancellation tokens) but not about what the child actually does.
///
/// Since WARNING 1's fix, `spawn_agent` requires a task factory — this
/// satisfies that requirement without introducing external I/O.
struct NoopTaskFactory;

impl TaskFactory for NoopTaskFactory {
    fn create_task(&self, _config: SpawnConfig, _token: CancellationToken) -> BoxTaskFuture {
        Box::pin(async {
            Ok(AgentLoopOutput {
                exit_reason: ExitReason::Complete,
                messages: vec![],
                token_usage: TokenUsage::default(),
                used_turns: 0,
                used_cost: Decimal::ZERO,
            })
        })
    }
}

#[derive(Clone)]
struct FakeTaskFactory {
    inner: Arc<FakeTaskFactoryInner>,
}

struct FakeTaskFactoryInner {
    plans: Mutex<HashMap<String, VecDeque<FakeTaskPlan>>>,
    started: Mutex<Vec<String>>,
    completed: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<String>>,
    state_changed: Notify,
    running: AtomicUsize,
    max_running: AtomicUsize,
}

enum FakeTaskPlan {
    Complete {
        release: Option<Arc<Notify>>,
        output: AgentLoopOutput,
    },
    Fail {
        error: RuntimeError,
    },
    WaitForCancellation {
        output: AgentLoopOutput,
    },
}

impl FakeTaskFactory {
    fn new() -> Self {
        Self {
            inner: Arc::new(FakeTaskFactoryInner {
                plans: Mutex::new(HashMap::new()),
                started: Mutex::new(Vec::new()),
                completed: Mutex::new(Vec::new()),
                cancelled: Mutex::new(Vec::new()),
                state_changed: Notify::new(),
                running: AtomicUsize::new(0),
                max_running: AtomicUsize::new(0),
            }),
        }
    }

    fn push_plan(&self, agent_id: &str, plan: FakeTaskPlan) {
        self.inner
            .plans
            .lock()
            .unwrap()
            .entry(agent_id.to_string())
            .or_default()
            .push_back(plan);
    }

    async fn wait_for_started_agents(&self, expected: usize) {
        loop {
            if self.inner.started.lock().unwrap().len() >= expected {
                return;
            }

            self.inner.state_changed.notified().await;
        }
    }

    async fn wait_for_completion(&self, agent_id: &str) {
        loop {
            if self
                .inner
                .completed
                .lock()
                .unwrap()
                .iter()
                .any(|completed| completed == agent_id)
            {
                return;
            }

            self.inner.state_changed.notified().await;
        }
    }

    async fn wait_for_cancellation(&self, agent_id: &str) {
        loop {
            if self
                .inner
                .cancelled
                .lock()
                .unwrap()
                .iter()
                .any(|cancelled| cancelled == agent_id)
            {
                return;
            }

            self.inner.state_changed.notified().await;
        }
    }

    fn started_order(&self) -> Vec<String> {
        self.inner.started.lock().unwrap().clone()
    }

    fn completion_count(&self, agent_id: &str) -> usize {
        self.inner
            .completed
            .lock()
            .unwrap()
            .iter()
            .filter(|completed| completed.as_str() == agent_id)
            .count()
    }

    fn max_running(&self) -> usize {
        self.inner.max_running.load(Ordering::SeqCst)
    }

    fn record_start(&self, agent_id: &str) {
        self.inner
            .started
            .lock()
            .unwrap()
            .push(agent_id.to_string());

        let running_now = self.inner.running.fetch_add(1, Ordering::SeqCst) + 1;
        let mut observed_max = self.inner.max_running.load(Ordering::SeqCst);
        while running_now > observed_max
            && self
                .inner
                .max_running
                .compare_exchange(
                    observed_max,
                    running_now,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
        {
            observed_max = self.inner.max_running.load(Ordering::SeqCst);
        }

        self.inner.state_changed.notify_waiters();
    }

    fn record_completion(&self, agent_id: &str) {
        self.inner
            .completed
            .lock()
            .unwrap()
            .push(agent_id.to_string());
        self.inner.running.fetch_sub(1, Ordering::SeqCst);
        self.inner.state_changed.notify_waiters();
    }

    fn record_cancellation(&self, agent_id: &str) {
        self.inner
            .cancelled
            .lock()
            .unwrap()
            .push(agent_id.to_string());
        self.inner.running.fetch_sub(1, Ordering::SeqCst);
        self.inner.state_changed.notify_waiters();
    }
}

impl TaskFactory for FakeTaskFactory {
    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture {
        let agent_id = config.agent_id.0.clone();
        let plan = self
            .inner
            .plans
            .lock()
            .unwrap()
            .get_mut(&agent_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("no fake task plan registered for agent {agent_id}"));
        let factory = self.clone();

        Box::pin(async move {
            factory.record_start(&agent_id);

            match plan {
                FakeTaskPlan::Complete { release, output } => {
                    if let Some(release) = release {
                        release.notified().await;
                    }

                    factory.record_completion(&agent_id);
                    Ok(output)
                }
                FakeTaskPlan::Fail { error } => {
                    factory.inner.running.fetch_sub(1, Ordering::SeqCst);
                    factory.inner.state_changed.notify_waiters();
                    Err(error)
                }
                FakeTaskPlan::WaitForCancellation { output } => {
                    while !cancellation.is_cancelled() {
                        tokio::task::yield_now().await;
                    }

                    factory.record_cancellation(&agent_id);
                    Ok(output)
                }
            }
        })
    }
}

fn completed_output() -> AgentLoopOutput {
    AgentLoopOutput {
        exit_reason: ExitReason::Complete,
        messages: vec![],
        token_usage: TokenUsage {
            input_tokens: 4,
            output_tokens: 3,
        },
        used_turns: 2,
        used_cost: rust_decimal::Decimal::new(15, 1),
    }
}

// S009 Assertion: Supervisor enforces capability attenuation on spawn.
#[test]
fn supervisor_enforces_capability_attenuation_on_spawn() {
    let parent_capability = CapabilityToken::default();
    let child_capability = CapabilityToken {
        shell: true,
        ..CapabilityToken::default()
    };
    let mut supervisor = AgentSupervisor::new(parent_capability, default_budget());

    let err = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            child_capability,
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect_err("spawn_agent should reject child capabilities that exceed the parent token");

    assert!(
        matches!(err, RuntimeError::CapabilityViolation(ref message) if message.contains("subset")),
        "expected a capability violation when the child requests shell access the parent lacks, got {err:?}"
    );
}

// S009 Assertion: Restart strategy is applied on agent failure.
#[test]
fn restart_strategy_is_applied_on_agent_failure() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());

    let retry_once_agent = AgentId("retry-once".into());
    assert!(
        supervisor.handle_failure(
            &retry_once_agent,
            &RestartStrategy::RetryOnce,
            "first failure",
        ),
        "retry_once should restart the agent on its first failure"
    );
    assert!(
        !supervisor.handle_failure(
            &retry_once_agent,
            &RestartStrategy::RetryOnce,
            "second failure",
        ),
        "retry_once should stop restarting after consuming the single retry"
    );

    let retry_twice_agent = AgentId("retry-twice".into());
    assert!(
        supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "first failure",
        ),
        "retry_twice_then_fail should restart the agent on the first failure"
    );
    assert!(
        supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "second failure",
        ),
        "retry_twice_then_fail should restart the agent on the second failure"
    );
    assert!(
        !supervisor.handle_failure(
            &retry_twice_agent,
            &RestartStrategy::RetryTwiceThenFail,
            "third failure",
        ),
        "retry_twice_then_fail should stop restarting after the second retry"
    );

    let snapshot_agent = AgentId("snapshot-agent".into());
    assert!(
        !supervisor.handle_failure(
            &snapshot_agent,
            &RestartStrategy::SnapshotAndFail,
            "snapshot failure",
        ),
        "snapshot_and_fail should propagate the failure instead of retrying"
    );

    let let_crash_agent = AgentId("let-crash-agent".into());
    assert!(
        !supervisor.handle_failure(&let_crash_agent, &RestartStrategy::LetCrash, "boom"),
        "let_crash should propagate the failure without retrying"
    );
}

// S009 Assertion: Cancelled agent receives cancellation signal.
#[tokio::test]
async fn cancelled_agent_receives_cancellation_signal() {
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );
    let token = supervisor
        .spawn_agent(spawn_config(
            "cancelled-agent",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawning a child with inherited capabilities should succeed");

    let observed_cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_cancellation_clone = Arc::clone(&observed_cancellation);
    let task_token = token.clone();

    let child_task = tokio::spawn(async move {
        while !task_token.is_cancelled() {
            tokio::task::yield_now().await;
        }
        observed_cancellation_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    supervisor.cancel_agent(&token);

    tokio::time::timeout(token.grace(), async {
        child_task
            .await
            .expect("the child task should complete after observing cancellation");
    })
    .await
    .expect("the child task should observe cancellation before the grace period expires");

    assert!(
        observed_cancellation.load(std::sync::atomic::Ordering::SeqCst),
        "expected the child task to observe the supervisor's cancellation signal"
    );
}

// S009 Assertion: Child budget does not exceed parent budget.
#[test]
fn child_budget_does_not_exceed_parent_budget() {
    let parent_capability = CapabilityToken::default();
    let mut parent_budget = default_budget();
    parent_budget.max_tokens = 10;
    parent_budget.used_tokens = 5;
    let mut supervisor = AgentSupervisor::new(parent_capability, parent_budget);

    let child_budget = ResourceBudget {
        max_tokens: 6,
        ..default_budget()
    };

    let err = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            CapabilityToken::default(),
            child_budget,
            RestartStrategy::LetCrash,
        ))
        .expect_err("spawn_agent should reject a child budget that exceeds the parent's remaining token budget");

    assert!(
        matches!(err, RuntimeError::BudgetExhausted(ref exhausted) if exhausted.resource == "tokens"),
        "expected spawn_agent to reject the oversized child token budget, got {err:?}"
    );
}

// S009 O11y Assertion: Agent spawn produces a create_agent span with gen_ai.agent.name.
#[tokio::test]
async fn agent_spawn_produces_create_agent_span_with_agent_name() {
    let (subscriber, captured_spans, _captured_events) = setup_capture();
    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );

    let _guard = tracing::subscriber::set_default(subscriber);
    supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawning a child with inherited capabilities should succeed");

    let spans = captured_spans.lock().unwrap();
    let create_agent_span = spans
        .iter()
        .find(|span| {
            span.name == "create_agent"
                && span.fields.get("gen_ai.operation.name") == Some(&"create_agent".to_string())
        })
        .expect("expected a create_agent span to be emitted during spawn");

    assert_eq!(
        create_agent_span.fields.get("gen_ai.agent.name"),
        Some(&"child-agent".to_string())
    );
}

// S009 O11y Assertion: Agent invocation is wrapped in an invoke_agent span.
#[tokio::test]
async fn agent_invocation_is_wrapped_in_invoke_agent_span() {
    let (subscriber, captured_spans, _captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("done")]);
    let mut agent = build_loop(provider, ToolRegistry::new(), journal, default_budget());

    let _guard = tracing::subscriber::set_default(subscriber);
    let _output = agent.run("say hello").await.expect("run should succeed");

    let spans = captured_spans.lock().unwrap();
    let invoke_agent_span = spans
        .iter()
        .find(|span| span.fields.get("gen_ai.operation.name") == Some(&"invoke_agent".to_string()))
        .expect("expected a span with gen_ai.operation.name=invoke_agent");

    assert_eq!(
        invoke_agent_span.fields.get("gen_ai.agent.name"),
        Some(&"test-agent".to_string())
    );
}

// S009 O11y Assertion: simulacra.agent.turns counter tracks turns per agent.
#[tokio::test]
async fn simulacra_agent_turns_counter_tracks_turns_per_agent() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![
        tool_call_response("echo", serde_json::json!({ "msg": "hi" })),
        text_response("done"),
    ]);
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool));
    let mut agent = build_loop(provider, tools, journal, default_budget());

    let _guard = tracing::subscriber::set_default(subscriber);
    let _output = agent
        .run("use the echo tool and finish")
        .await
        .expect("run should succeed");

    let events = captured_events.lock().unwrap();
    let turn_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.agent.turns") == Some(&"1".to_string()))
        .collect::<Vec<_>>();

    assert_eq!(
        turn_events.len(),
        2,
        "expected simulacra.agent.turns to be emitted once per turn for the current agent"
    );
    assert!(
        turn_events
            .iter()
            .all(|event| event.current_span.as_deref() == Some("invoke_agent")),
        "turn metrics should be emitted on the invoke_agent span for per-agent attribution"
    );
}

// S009 O11y Assertion: Agent spawn is logged at INFO with agent name, parent, and capabilities.
#[tokio::test]
async fn agent_spawn_is_logged_at_info_with_agent_name_parent_and_capabilities() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let child_capability = CapabilityToken {
        shell: true,
        ..CapabilityToken::default()
    };
    let mut supervisor = AgentSupervisor::with_task_factory(
        child_capability.clone(),
        default_budget(),
        Arc::new(NoopTaskFactory),
    );

    let _guard = tracing::subscriber::set_default(subscriber);
    supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            child_capability,
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawning a child with inherited capabilities should succeed");

    let events = captured_events.lock().unwrap();
    let spawn_event = events
        .iter()
        .find(|event| {
            event.level == "INFO"
                && event.current_span.as_deref() == Some("create_agent")
                && event.fields.get("gen_ai.agent.name") == Some(&"child-agent".to_string())
                && event.fields.get("parent") == Some(&"parent-agent".to_string())
        })
        .expect("expected an INFO spawn event with agent and parent context");

    assert!(
        spawn_event
            .fields
            .get("capabilities")
            .is_some_and(|value| value.contains("shell: true")),
        "expected the spawn log to include the child's capabilities, got {:?}",
        spawn_event.fields
    );
}

// S009 O11y Assertion: Agent completion is logged at INFO with agent name, exit reason, and token total.
#[tokio::test]
async fn agent_completion_is_logged_at_info_with_agent_name_exit_reason_and_token_total() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let journal = Arc::new(InMemoryJournalStorage::new());
    let provider = FakeProvider::new(vec![text_response("done")]);
    let mut agent = build_loop(provider, ToolRegistry::new(), journal, default_budget());

    let _guard = tracing::subscriber::set_default(subscriber);
    let output = agent.run("say hello").await.expect("run should succeed");

    let events = captured_events.lock().unwrap();
    let completion_event = events
        .iter()
        .find(|event| {
            event.level == "INFO"
                && event.current_span.as_deref() == Some("invoke_agent")
                && event.fields.get("gen_ai.agent.name") == Some(&"test-agent".to_string())
                && event.fields.get("simulacra.agent.exit_reason") == Some(&"Complete".to_string())
                && event.fields.get("simulacra.agent.token_total")
                    == Some(&output.token_usage.total().to_string())
        })
        .expect(
            "expected an INFO completion event with the agent name, exit reason, and token total",
        );

    assert_eq!(
        completion_event.fields.get("simulacra.agent.token_total"),
        Some(&output.token_usage.total().to_string())
    );
}

// S009 O11y Assertion: Agent restart is logged at WARN with agent name, strategy, and failure reason.
#[test]
fn agent_restart_is_logged_at_warn_with_agent_name_strategy_and_failure_reason() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("restarting-child".into());

    let _guard = tracing::subscriber::set_default(subscriber);
    assert!(
        supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "boom"),
        "retry_once should request a restart on the first failure"
    );

    let events = captured_events.lock().unwrap();
    let restart_event = events
        .iter()
        .find(|event| {
            event.level == "WARN"
                && event.fields.get("gen_ai.agent.name") == Some(&"restarting-child".to_string())
                && event.fields.get("strategy") == Some(&"retry_once".to_string())
                && event.fields.get("failure_reason") == Some(&"boom".to_string())
        })
        .expect("expected a WARN restart event with agent name, strategy, and failure reason");

    assert_eq!(
        restart_event.fields.get("strategy"),
        Some(&"retry_once".to_string())
    );
}

// S009 Assertion: retry_once strategy restarts the agent exactly once then fails.
#[test]
fn retry_once_restarts_exactly_once_then_fails() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("retry-once-child".into());

    assert!(
        supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "first failure"),
        "retry_once should restart the child on the first failure"
    );
    assert!(
        !supervisor.handle_failure(&agent_id, &RestartStrategy::RetryOnce, "second failure"),
        "retry_once should stop restarting the child after the single retry is consumed"
    );
}

// S009 Assertion: retry_twice_then_fail strategy restarts at most twice.
#[test]
fn retry_twice_then_fail_restarts_at_most_twice() {
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("retry-twice-child".into());

    assert!(
        supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "first failure",
        ),
        "retry_twice_then_fail should restart the child on the first failure"
    );
    assert!(
        supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "second failure",
        ),
        "retry_twice_then_fail should restart the child on the second failure"
    );
    assert!(
        !supervisor.handle_failure(
            &agent_id,
            &RestartStrategy::RetryTwiceThenFail,
            "third failure",
        ),
        "retry_twice_then_fail should stop restarting the child after the second retry is consumed"
    );
}

// S009 Assertion: let_crash does not restart the child.
#[test]
fn let_crash_does_not_restart() {
    let (subscriber, _captured_spans, captured_events) = setup_capture();
    let supervisor = AgentSupervisor::new(CapabilityToken::default(), default_budget());
    let agent_id = AgentId("let-crash-child".into());

    let _guard = tracing::subscriber::set_default(subscriber);
    let should_restart = supervisor.handle_failure(&agent_id, &RestartStrategy::LetCrash, "boom");

    assert!(
        !should_restart,
        "let_crash should not schedule a restart for a failed child"
    );

    let events = captured_events.lock().unwrap();
    assert!(
        events.iter().all(|event| {
            !(event.level == "WARN"
                && event.fields.get("gen_ai.agent.name") == Some(&"let-crash-child".to_string())
                && event.fields.get("message") == Some(&"agent restart triggered".to_string()))
        }),
        "let_crash should not emit a restart warning when the strategy is to let the child crash"
    );
}

// S009 RED Assertion: spawn_agent should start a child task and observe its completion.
#[tokio::test]
async fn supervisor_spawns_agent_that_runs_to_completion() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "child-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let mut child_budget = default_budget();
    child_budget.used_tokens = 7;
    child_budget.used_turns = 2;
    child_budget.used_cost = Decimal::new(15, 1);

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let _token = supervisor
        .spawn_agent(spawn_config(
            "child-agent",
            "parent-agent",
            CapabilityToken::default(),
            child_budget.clone(),
            RestartStrategy::LetCrash,
        ))
        .expect("spawn should succeed and start the child task");

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("child-agent"),
    )
    .await
    .expect("the fake child task should complete and notify the supervisor");

    assert_eq!(
        factory.completion_count("child-agent"),
        1,
        "expected the supervisor to observe exactly one successful completion"
    );
    assert_eq!(
        supervisor.parent_budget().used_tokens,
        child_budget.used_tokens,
        "expected child completion to flow back into the supervisor and roll up token usage"
    );
}

// S009 RED Assertion: the public actor loop should honor Signal > Command > Work priority.
#[tokio::test]
async fn supervisor_actor_loop_processes_messages_by_priority() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "signal-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );
    factory.push_plan(
        "command-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );
    factory.push_plan(
        "work-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    let signal_tx = tx.clone();
    let command_tx = tx.clone();
    let work_tx = tx.clone();

    let (signal_result_tx, _) = tokio::sync::oneshot::channel();
    let signal_message = SupervisorMessage {
        priority: MessagePriority::Signal,
        agent_id: AgentId("signal-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "signal-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            signal_result_tx,
        ),
    };
    let (command_result_tx, _) = tokio::sync::oneshot::channel();
    let command_message = SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("command-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "command-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            command_result_tx,
        ),
    };
    let (work_result_tx, _) = tokio::sync::oneshot::channel();
    let work_message = SupervisorMessage {
        priority: MessagePriority::Work,
        agent_id: AgentId("work-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "work-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::LetCrash,
            )),
            work_result_tx,
        ),
    };

    let (signal_result, command_result, work_result) = tokio::join!(
        signal_tx.send(signal_message),
        command_tx.send(command_message),
        work_tx.send(work_message),
    );
    signal_result.expect("signal message should send");
    command_result.expect("command message should send");
    work_result.expect("work message should send");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(3))
        .await
        .expect("the actor loop should dispatch all queued messages");

    assert_eq!(
        factory.started_order(),
        vec![
            "signal-agent".to_string(),
            "command-agent".to_string(),
            "work-agent".to_string()
        ],
        "expected the actor loop to dispatch simultaneously queued messages in priority order"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}

// S009 RED Assertion: the supervisor should keep multiple child tasks alive concurrently.
#[tokio::test]
async fn supervisor_manages_multiple_concurrent_agents() {
    let factory = FakeTaskFactory::new();
    let finish_first = Arc::new(Notify::new());
    let finish_third = Arc::new(Notify::new());

    factory.push_plan(
        "agent-one",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&finish_first)),
            output: completed_output(),
        },
    );
    factory.push_plan(
        "agent-two",
        FakeTaskPlan::WaitForCancellation {
            output: completed_output(),
        },
    );
    factory.push_plan(
        "agent-three",
        FakeTaskPlan::Complete {
            release: Some(Arc::clone(&finish_third)),
            output: completed_output(),
        },
    );

    let mut supervisor = AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    );

    let _token_one = supervisor
        .spawn_agent(spawn_config(
            "agent-one",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("first child should spawn");
    let token_two = supervisor
        .spawn_agent(spawn_config(
            "agent-two",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("second child should spawn");
    let _token_three = supervisor
        .spawn_agent(spawn_config(
            "agent-three",
            "parent-agent",
            CapabilityToken::default(),
            default_budget(),
            RestartStrategy::LetCrash,
        ))
        .expect("third child should spawn");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(3))
        .await
        .expect("all three children should start");

    assert_eq!(
        factory.max_running(),
        3,
        "expected the supervisor to allow all three child tasks to run concurrently"
    );

    supervisor.cancel_agent(&token_two);
    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_cancellation("agent-two"),
    )
    .await
    .expect("the cancelled child should observe the cancellation signal");

    finish_first.notify_waiters();
    finish_third.notify_waiters();

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("agent-one"),
    )
    .await
    .expect("the first child should continue running after a sibling is cancelled");
    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("agent-three"),
    )
    .await
    .expect("the third child should continue running after a sibling is cancelled");

    assert_eq!(
        factory.completion_count("agent-one"),
        1,
        "expected the first child to complete normally"
    );
    assert_eq!(
        factory.completion_count("agent-three"),
        1,
        "expected the third child to complete normally"
    );
}

// S009 RED Assertion: a failed child should be restarted by the actor loop per strategy.
#[tokio::test]
async fn supervisor_restarts_failed_agent_via_actor_loop() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "flaky-agent",
        FakeTaskPlan::Fail {
            error: RuntimeError::Session("boom".into()),
        },
    );
    factory.push_plan(
        "flaky-agent",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    let (flaky_result_tx, _) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("flaky-agent".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "flaky-agent",
                "parent-agent",
                CapabilityToken::default(),
                default_budget(),
                RestartStrategy::RetryOnce,
            )),
            flaky_result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    tokio::time::timeout(Duration::from_secs(1), factory.wait_for_started_agents(2))
        .await
        .expect("the failed child should be restarted exactly once");

    assert_eq!(
        factory
            .started_order()
            .into_iter()
            .filter(|agent_id| agent_id == "flaky-agent")
            .count(),
        2,
        "expected retry_once to cause the actor loop to spawn the child a second time after failure"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}

// S009 RED Assertion: child task results should flow back to the supervisor over mpsc.
#[tokio::test]
async fn child_agents_communicate_via_mpsc() {
    let factory = FakeTaskFactory::new();
    factory.push_plan(
        "mpsc-child",
        FakeTaskPlan::Complete {
            release: None,
            output: completed_output(),
        },
    );

    let child_budget = default_budget();

    let supervisor = Arc::new(AgentSupervisor::with_task_factory(
        CapabilityToken::default(),
        default_budget(),
        Arc::new(factory.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = {
        let supervisor = Arc::clone(&supervisor);
        tokio::spawn(async move { supervisor.run_actor_loop(rx).await })
    };

    let (mpsc_result_tx, _) = tokio::sync::oneshot::channel();
    tx.send(SupervisorMessage {
        priority: MessagePriority::Command,
        agent_id: AgentId("mpsc-child".into()),
        payload: SupervisorPayload::Spawn(
            Box::new(spawn_config(
                "mpsc-child",
                "parent-agent",
                CapabilityToken::default(),
                child_budget.clone(),
                RestartStrategy::LetCrash,
            )),
            mpsc_result_tx,
        ),
    })
    .await
    .expect("spawn message should send");

    tokio::time::timeout(
        Duration::from_secs(1),
        factory.wait_for_completion("mpsc-child"),
    )
    .await
    .expect("the child should complete and send its result back to the supervisor");

    // Budget rollup uses actual child AgentLoopOutput.token_usage (S018 fix),
    // not the stale SpawnConfig clone. completed_output() has 4+3=7 tokens.
    assert_eq!(
        supervisor.parent_budget().used_tokens,
        7,
        "expected the supervisor to observe the child's completion message over mpsc and roll up budget usage"
    );

    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor loop should exit once the channel closes")
        .expect("actor loop task should shut down cleanly");
}
