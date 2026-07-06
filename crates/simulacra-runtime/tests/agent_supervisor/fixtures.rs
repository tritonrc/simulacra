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
            reported_tool_uses: None,
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
            reported_tool_uses: None,
        used_turns: 2,
        used_cost: rust_decimal::Decimal::new(15, 1),
    }
}

// S009 Assertion: Supervisor enforces capability attenuation on spawn.
