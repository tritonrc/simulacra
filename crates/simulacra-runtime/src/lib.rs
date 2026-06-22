//! Simulacra runtime crate.
//!
//! Provides session management, journal storage, agent supervision, the agent
//! loop, and guardrail traits. This is the top-level orchestration layer that
//! composes sandbox, provider, context, and MCP capabilities.

mod activity_sink;
mod agent_loop;
mod error;
mod guardrail;
mod journal;
mod journal_sqlite;
mod replay;
mod session;
mod session_file;
mod session_sqlite;
mod spawn_tool;
mod vfs_hook;

// Re-export all public types at the crate root.
pub use activity_sink::{
    ActivitySink, ChannelActivitySink, ForwardingActivitySink, NoopActivitySink,
};
pub use agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopOutput, TurnResult};
pub use error::RuntimeError;
pub use guardrail::{GuardrailDecision, InputGuardrail, OutputGuardrail};
pub use journal::{CountingJournalStorage, InMemoryJournalStorage};
pub use journal_sqlite::SqliteJournalStorage;
pub use replay::JournalReplayIterator;
pub use session::{InMemorySessionStorage, Session, SessionStorage};
pub use session_file::FileSessionStorage;
pub use session_sqlite::SqliteSessionStorage;
pub use spawn_tool::{
    AgentTaskFactory, ChildCellConfigurator, ChildToolRegistrar, DEFAULT_SYSTEM_PROMPT,
    NoopContextStrategy, ProviderKind, SpawnAgentTool,
};
pub use vfs_hook::HookedVfsLayer;

// ---------------------------------------------------------------------------
// Agent Supervisor
// ---------------------------------------------------------------------------

use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use simulacra_types::{ActivityEvent, AgentId, CapabilityToken, ResourceBudget};
use tokio::task::JoinHandle;

/// A boxed future that resolves to an agent loop result.
pub type BoxTaskFuture =
    Pin<Box<dyn Future<Output = Result<AgentLoopOutput, RuntimeError>> + Send + 'static>>;

/// Factory for creating agent tasks. Allows the supervisor to spawn child
/// agents without knowing the concrete task implementation.
pub trait TaskFactory: Send + Sync {
    /// Create a task future for the given spawn configuration and cancellation token.
    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture;
}

/// Restart strategy applied when a supervised agent fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Retry the agent once, then propagate the failure.
    RetryOnce,
    /// Retry the agent twice, then propagate the failure.
    RetryTwiceThenFail,
    /// Snapshot journal state before propagating the failure.
    SnapshotAndFail,
    /// Do not restart — let the agent crash.
    LetCrash,
}

/// Priority levels for supervisor messages.
///
/// Ordering: Signal (highest) > Supervision > Command > Work (lowest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePriority {
    /// Highest priority — cancellation signals, shutdown.
    Signal = 3,
    /// Supervision events — child failure notifications.
    Supervision = 2,
    /// Commands — spawn requests, config changes.
    Command = 1,
    /// Regular work — agent task results.
    Work = 0,
}

impl Ord for MessagePriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

impl PartialOrd for MessagePriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A message destined for the supervisor actor.
#[derive(Debug)]
pub struct SupervisorMessage {
    pub priority: MessagePriority,
    pub agent_id: AgentId,
    pub payload: SupervisorPayload,
}

/// Result sent back to the spawn_agent caller when a child completes.
pub type SpawnResult = Result<AgentLoopOutput, RuntimeError>;

/// Payload variants for supervisor messages.
#[derive(Debug)]
pub enum SupervisorPayload {
    /// Agent completed successfully.
    Completed,
    /// Agent failed with the given reason.
    Failed(String),
    /// Spawn a new child agent. The oneshot sender receives the child result
    /// so the parent can await it synchronously (S018: spawn_agent is blocking).
    Spawn(Box<SpawnConfig>, tokio::sync::oneshot::Sender<SpawnResult>),
    /// Cancel a running agent.
    Cancel,
}

impl Eq for SupervisorMessage {}

impl PartialEq for SupervisorMessage {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Ord for SupervisorMessage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl PartialOrd for SupervisorMessage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Cooperative cancellation token.
///
/// When cancelled, the agent receives a signal and has a grace period
/// to finish its current operation before being forcefully terminated.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    grace_period: Duration,
}

impl CancellationToken {
    /// Create a new cancellation token with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            grace_period,
        }
    }

    /// Signal cancellation.
    pub fn signal(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Check whether cancellation has been signalled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The grace period allowed after signal before forceful termination.
    pub fn grace(&self) -> Duration {
        self.grace_period
    }
}

/// Configuration for spawning a child agent.
///
/// For interactive sub-agent delegation (S018), `agent_type` and `task` carry
/// the configured child type name and the delegated task text passed to
/// `AgentLoop::run(task)`. The child receives a fresh Provider instance
/// selected from the `agent_type` configuration, with its own system_prompt,
/// and its effective capabilities are the intersection of parent, child-type
/// config, and any optional override.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub capability: Option<CapabilityToken>,
    pub budget: ResourceBudget,
    pub restart_strategy: RestartStrategy,
    /// The configured child agent type name from simulacra.toml.
    /// `None` when the parent spawns a generic agent with an inline system_prompt.
    #[allow(dead_code)]
    pub agent_type: Option<String>,
    /// The delegated task text passed to the child AgentLoop::run(task).
    #[allow(dead_code)]
    pub task: String,
    /// Inline system prompt for generic sub-agents (max 8 KB).
    /// Present only when `agent_type` is `None`.
    #[allow(dead_code)]
    pub system_prompt: Option<String>,
    /// Model capability tier (e.g. "reasoning", "balanced", "fast").
    /// Resolved to a concrete model via `[tiers]` in simulacra.toml.
    #[allow(dead_code)]
    pub tier: Option<String>,
    /// Resolved tier label for observability. When `tier` is omitted for a
    /// generic child, this records the parent's tier name without changing the
    /// inherited model selection.
    pub resolved_tier: Option<String>,
}

/// Supervises agent lifecycle — spawn, cancel, restart.
///
/// Enforces capability attenuation on spawn: child CapabilityToken must be a
/// subset of the parent's token. Validates child budget against parent budget.
/// Applies restart strategies on agent failure.
///
/// The supervisor is an actor-style loop built on raw tokio primitives:
/// it receives `SupervisorMessage` values over an `mpsc::Receiver`, dispatches
/// them through a `tokio::select!` loop, and tracks child tasks in a
/// `HashMap<AgentId, JoinHandle<()>>`. Child agents are launched via
/// `tokio::spawn` and communicate back via `mpsc::channel` / `mpsc::Sender`.
pub struct AgentSupervisor {
    parent_capability: CapabilityToken,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    retry_counts: Mutex<HashMap<AgentId, usize>>,
    /// Shared retry counts accessible from spawned tasks.
    retry_counts_shared: Arc<Mutex<HashMap<AgentId, usize>>>,
    children: Mutex<HashMap<AgentId, JoinHandle<()>>>,
    cancellation_tokens: Mutex<HashMap<AgentId, CancellationToken>>,
    task_factory: Option<Arc<dyn TaskFactory>>,
    #[allow(dead_code)]
    spawn_configs: Mutex<HashMap<AgentId, SpawnConfig>>,
    /// Optional JournalStorage for recording sub-agent lifecycle entries.
    /// When set, the supervisor appends SubAgentSpawned before child execution
    /// and SubAgentCompleted after child completion/failure, per S018.
    /// Child journal entries are written under the child_id and can be
    /// correlated to the parent by child_id.
    #[allow(dead_code)]
    journal_storage: Option<Arc<dyn simulacra_types::JournalStorage>>,
    /// S019: Activity sink for emitting ActivityEvent::ChildSpawned and
    /// ActivityEvent::ChildFinished with aggregated stats (tool_uses, token_count, duration_ms).
    activity_sink: Arc<dyn ActivitySink>,
}

impl std::fmt::Debug for AgentSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSupervisor")
            .field("parent_capability", &self.parent_capability)
            .field("parent_budget", &self.parent_budget)
            .finish_non_exhaustive()
    }
}

impl AgentSupervisor {
    /// Create a new supervisor with the parent's capability and budget.
    pub fn new(parent_capability: CapabilityToken, parent_budget: ResourceBudget) -> Self {
        Self {
            parent_capability,
            parent_budget: Arc::new(Mutex::new(parent_budget)),
            retry_counts: Mutex::new(HashMap::new()),
            retry_counts_shared: Arc::new(Mutex::new(HashMap::new())),
            children: Mutex::new(HashMap::new()),
            cancellation_tokens: Mutex::new(HashMap::new()),
            task_factory: None,
            spawn_configs: Mutex::new(HashMap::new()),
            journal_storage: None,
            activity_sink: Arc::new(NoopActivitySink),
        }
    }

    /// Create a new supervisor with a task factory for spawning child agents.
    pub fn with_task_factory(
        parent_capability: CapabilityToken,
        parent_budget: ResourceBudget,
        task_factory: Arc<dyn TaskFactory>,
    ) -> Self {
        Self {
            parent_capability,
            parent_budget: Arc::new(Mutex::new(parent_budget)),
            retry_counts: Mutex::new(HashMap::new()),
            retry_counts_shared: Arc::new(Mutex::new(HashMap::new())),
            children: Mutex::new(HashMap::new()),
            cancellation_tokens: Mutex::new(HashMap::new()),
            task_factory: Some(task_factory),
            spawn_configs: Mutex::new(HashMap::new()),
            journal_storage: None,
            activity_sink: Arc::new(NoopActivitySink),
        }
    }

    /// Set the journal storage backend for recording sub-agent lifecycle entries.
    pub fn set_journal_storage(&mut self, journal: Arc<dyn simulacra_types::JournalStorage>) {
        self.journal_storage = Some(journal);
    }

    /// Set the activity sink used for supervisor-owned lifecycle events.
    pub fn set_activity_sink(&mut self, activity_sink: Arc<dyn ActivitySink>) {
        self.activity_sink = activity_sink;
    }

    /// Validate a spawn request and perform all pre-spawn side effects:
    /// budget checks, capability checks, spawn_types check, budget increment,
    /// journal write, tracing span + info log, and activity event emission.
    ///
    /// This is the single source of truth for spawn validation. Both
    /// `spawn_agent()` and `dispatch_message_into()` call this method to
    /// ensure the actor loop path runs the same checks as the direct path.
    fn validate_and_prepare_spawn(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        let agent_name = config.agent_id.0.as_str();
        let parent = config.parent_id.0.as_str();
        let capabilities = format!("{:?}", config.capability.as_ref());

        let child_agent_type = config.agent_type.as_deref().unwrap_or("generic");
        let spawn_mode = if config.agent_type.is_some() {
            "configured"
        } else {
            "generic"
        };
        let tier_label = config
            .resolved_tier
            .as_deref()
            .or(config.tier.as_deref())
            .unwrap_or("balanced");
        let _span = tracing::info_span!(
            "create_agent",
            "gen_ai.operation.name" = "create_agent",
            "gen_ai.agent.name" = agent_name,
            "simulacra.parent.agent_id" = parent,
            "simulacra.child.agent_type" = child_agent_type,
            "simulacra.agent.spawn_mode" = spawn_mode,
            "simulacra.agent.tier" = tier_label,
        )
        .entered();

        // Budget checks first: reject early if the child's budget request
        // exceeds the parent's remaining headroom. This ensures budget
        // enforcement even when spawn_types or capabilities would also reject.
        {
            let budget = self.parent_budget.lock().unwrap();
            // max_sub_agents == 0 means unlimited (S006/S018). Only check
            // when the parent has a finite sub-agent limit.
            if budget.max_sub_agents > 0 && budget.used_sub_agents >= budget.max_sub_agents {
                return Err(RuntimeError::BudgetExhausted(
                    simulacra_types::BudgetExhausted {
                        resource: "sub_agents".into(),
                        used: budget.used_sub_agents.to_string(),
                        limit: budget.max_sub_agents.to_string(),
                    },
                ));
            }

            // max_tokens == 0 means unlimited (S006/S018). Only check
            // remaining token headroom when the parent has a finite limit.
            if budget.max_tokens > 0 {
                let parent_remaining_tokens = budget.max_tokens.saturating_sub(budget.used_tokens);
                if config.budget.max_tokens > parent_remaining_tokens {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "tokens".into(),
                            used: budget.used_tokens.to_string(),
                            limit: budget.max_tokens.to_string(),
                        },
                    ));
                }
            }

            // max_turns == 0 means unlimited (S006/S018). Only check
            // remaining turn headroom when the parent has a finite limit.
            if budget.max_turns > 0 {
                let parent_remaining_turns = budget.max_turns.saturating_sub(budget.used_turns);
                if config.budget.max_turns > parent_remaining_turns {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "turns".into(),
                            used: budget.used_turns.to_string(),
                            limit: budget.max_turns.to_string(),
                        },
                    ));
                }
            }

            // max_cost == 0 means unlimited (S006/S018). Only check
            // remaining cost headroom when the parent has a finite limit.
            if !budget.max_cost.is_zero() {
                let parent_remaining_cost = budget.max_cost - budget.used_cost;
                if config.budget.max_cost > parent_remaining_cost {
                    return Err(RuntimeError::BudgetExhausted(
                        simulacra_types::BudgetExhausted {
                            resource: "cost".into(),
                            used: budget.used_cost.to_string(),
                            limit: budget.max_cost.to_string(),
                        },
                    ));
                }
            }
        }

        if let Err(exhausted) = config.budget.check_budget() {
            return Err(RuntimeError::BudgetExhausted(exhausted));
        }

        // The parent must have the child's agent_type in its spawn_types.
        // An empty spawn_types list means "no restriction" (same convention as
        // max_sub_agents == 0 meaning unlimited).
        // Only enforced for named agent_type spawns; generic (None) agents
        // skip the spawn_types check.
        if let Some(ref at) = config.agent_type
            && !self.parent_capability.spawn_types.is_empty()
            && !self.parent_capability.spawn_types.contains(at)
        {
            return Err(RuntimeError::CapabilityViolation(format!(
                "agent_type {:?} is not in parent spawn_types {:?}",
                at, self.parent_capability.spawn_types
            )));
        }

        if let Some(ref cap) = config.capability
            && !cap.is_subset_of(&self.parent_capability)
        {
            return Err(RuntimeError::CapabilityViolation(
                "child capability is not a subset of parent capability".into(),
            ));
        }

        // S018: Journal SubAgentSpawned before child execution begins.
        // The child_id links the parent journal to the child's own journal
        // stream in JournalStorage.
        if let Some(ref journal) = self.journal_storage {
            let spawned_entry = simulacra_types::JournalEntry {
                schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                agent_id: config.parent_id.clone(),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                entry: simulacra_types::JournalEntryKind::SubAgentSpawned {
                    child_id: config.agent_id.clone(),
                    agent_type: config
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "generic".to_string()),
                    system_prompt: if config.agent_type.is_none() {
                        Some(
                            config
                                .system_prompt
                                .clone()
                                .filter(|prompt| !prompt.is_empty())
                                .unwrap_or_else(|| {
                                    crate::spawn_tool::DEFAULT_SYSTEM_PROMPT.to_string()
                                }),
                        )
                    } else {
                        None
                    },
                },
            };
            journal
                .append(spawned_entry)
                .map_err(|source| RuntimeError::JournalAppendFailed {
                    entry_kind: "SubAgentSpawned",
                    source,
                })?;
        }

        self.parent_budget.lock().unwrap().used_sub_agents += 1;

        tracing::info!(
            "gen_ai.agent.name" = agent_name,
            parent = parent,
            capabilities = capabilities.as_str(),
            "agent spawned"
        );

        // S019: Emit ActivityEvent::ChildSpawned before the child starts
        self.activity_sink.emit(ActivityEvent::ChildSpawned {
            child_id: config.agent_id.0.clone(),
            agent_type: config
                .agent_type
                .clone()
                .unwrap_or_else(|| "generic".to_string()),
            task: config.task.clone(),
        });

        Ok(())
    }

    /// Spawn a child agent under supervision.
    ///
    /// Validates:
    /// - Child CapabilityToken is_subset_of parent token (capability attenuation).
    /// - Child budget does not exceed parent budget (check_budget, used_sub_agents, max_sub_agents).
    /// - Emits a `create_agent` span with `gen_ai.operation.name` and `gen_ai.agent.name`.
    /// - Logs spawn at INFO with agent name, parent, and capabilities.
    pub fn spawn_agent(&mut self, config: SpawnConfig) -> Result<CancellationToken, RuntimeError> {
        self.validate_and_prepare_spawn(&config)?;

        let token = CancellationToken::new(Duration::from_secs(5));

        // WARNING 1 fix: spawn_agent must have a task factory. Returning Ok(token)
        // without running any task was misleading — callers have no way to know
        // the spawn silently did nothing. This is a programmer error at wiring
        // time; fail fast instead of pretending success.
        let Some(ref factory) = self.task_factory else {
            return Err(RuntimeError::SpawnMissingTask);
        };

        let agent_id = config.agent_id.clone();
        let agent_id_for_map = agent_id.clone();
        let parent_id_str = config.parent_id.0.clone();
        let child_agent_type = config
            .agent_type
            .clone()
            .unwrap_or_else(|| "generic".to_string());
        let mut task_future = factory.create_task(config, token.clone());
        let parent_budget = Arc::clone(&self.parent_budget);
        let journal = self.journal_storage.clone();

        // Try polling the future once synchronously. If the task factory
        // resolves immediately (as in tests or simple delegation), we
        // handle the result on the caller's thread so tracing events are
        // emitted through the caller's subscriber.
        let immediate = {
            let waker = noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            Pin::as_mut(&mut task_future).poll(&mut cx)
        };

        let spawn_start = std::time::Instant::now();
        let sink = Arc::clone(&self.activity_sink);

        if let std::task::Poll::Ready(result) = immediate {
            // WARNING 1 fix: if the child immediately errored, propagate that
            // error instead of returning Ok(token). We still call
            // `process_child_result` so journaling, activity events, and
            // tracing fire for the failure — the caller sees the error too.
            let was_err = result.is_err();
            // Clone the error for propagation before process_child_result consumes it.
            let err_for_return = match &result {
                Ok(_) => None,
                Err(e) => Some(RuntimeError::Session(format!(
                    "child {} (agent_type={}) failed immediately: {e}",
                    agent_id.0, child_agent_type
                ))),
            };
            Self::process_child_result(
                result,
                &agent_id,
                &parent_id_str,
                &child_agent_type,
                &parent_budget,
                &journal,
                &sink,
                spawn_start,
            );
            let handle: JoinHandle<()> = tokio::spawn(async {});
            self.children
                .lock()
                .unwrap()
                .insert(agent_id_for_map, handle);
            if was_err {
                return Err(err_for_return.expect("err_for_return set when was_err"));
            }
            return Ok(token);
        }

        // Future is pending — spawn it as a background task.
        let dispatch = tracing::dispatcher::get_default(|d| d.clone());
        let handle: JoinHandle<()> = tokio::spawn(async move {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            let result = task_future.await;
            Self::process_child_result(
                result,
                &agent_id,
                &parent_id_str,
                &child_agent_type,
                &parent_budget,
                &journal,
                &sink,
                spawn_start,
            );
        });
        self.children
            .lock()
            .unwrap()
            .insert(agent_id_for_map, handle);

        Ok(token)
    }

    /// Process a child task result: roll up budget, journal, emit tracing and
    /// S019 ActivityEvent::ChildFinished with aggregated stats (tool_uses, token_count, duration_ms).
    #[allow(clippy::too_many_arguments)]
    fn process_child_result(
        result: Result<AgentLoopOutput, RuntimeError>,
        agent_id: &AgentId,
        parent_id_str: &str,
        child_agent_type: &str,
        parent_budget: &Arc<Mutex<ResourceBudget>>,
        journal: &Option<Arc<dyn simulacra_types::JournalStorage>>,
        sink: &Arc<dyn ActivitySink>,
        spawn_start: std::time::Instant,
    ) {
        match result {
            Ok(output) => {
                let token_total = output.token_usage.total();
                let tool_uses = output.used_turns;
                let token_count = token_total;
                let duration_ms = spawn_start.elapsed().as_millis() as u64;

                let mut budget = parent_budget.lock().unwrap();
                budget.used_tokens +=
                    output.token_usage.input_tokens + output.token_usage.output_tokens;
                budget.used_turns += output.used_turns;
                budget.used_cost += output.used_cost;

                // S018: Journal SubAgentCompleted { success: true }
                if let Some(j) = journal {
                    let completed_entry = simulacra_types::JournalEntry {
                        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                        agent_id: AgentId(parent_id_str.to_string()),
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        entry: simulacra_types::JournalEntryKind::SubAgentCompleted {
                            child_id: AgentId(agent_id.0.clone()),
                            success: true,
                        },
                    };
                    let _ = j.append(completed_entry);
                }

                let exit_reason_str = spawn_tool::exit_reason_to_snake_case(&output.exit_reason);

                // S019: Emit ActivityEvent::ChildFinished with aggregated stats
                sink.emit(ActivityEvent::ChildFinished {
                    child_id: agent_id.0.clone(),
                    agent_type: child_agent_type.to_string(),
                    exit_reason: exit_reason_str.clone(),
                    duration_ms,
                    tool_uses,
                    token_count,
                });

                tracing::info!(
                    child_id = agent_id.0.as_str(),
                    parent_id = parent_id_str,
                    exit_reason = exit_reason_str.as_str(),
                    token_total = token_total,
                    "child agent completed"
                );
            }
            Err(err) => {
                let duration_ms = spawn_start.elapsed().as_millis() as u64;

                // S018: Journal SubAgentCompleted { success: false }
                if let Some(j) = journal {
                    let failed_entry = simulacra_types::JournalEntry {
                        schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                        agent_id: AgentId(parent_id_str.to_string()),
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        entry: simulacra_types::JournalEntryKind::SubAgentCompleted {
                            child_id: AgentId(agent_id.0.clone()),
                            success: false,
                        },
                    };
                    let _ = j.append(failed_entry);
                }

                // S019: Emit ChildFinished on failure too
                sink.emit(ActivityEvent::ChildFinished {
                    child_id: agent_id.0.clone(),
                    agent_type: child_agent_type.to_string(),
                    exit_reason: format!("Error: {err}"),
                    duration_ms,
                    tool_uses: 0,
                    token_count: 0,
                });

                tracing::warn!(
                    child_id = agent_id.0.as_str(),
                    parent_id = parent_id_str,
                    agent_type = child_agent_type,
                    failure_reason = %err,
                    "child agent failed"
                );
            }
        }
    }

    /// Cancel a running agent by signalling its cancellation token.
    ///
    /// The agent receives a cooperative cancellation signal and has a grace
    /// period to finish its current work before forceful termination.
    pub fn cancel_agent(&self, token: &CancellationToken) {
        token.signal();
        let _grace = token.grace();
        tracing::info!("agent cancellation signalled");
    }

    /// Deduct a child agent's resource usage from the parent budget.
    ///
    /// S006: When a child agent completes, its consumed tokens, turns, and cost
    /// must be rolled up into the parent so the parent's remaining budget
    /// accurately reflects total work performed.
    pub fn deduct_child_usage(&self, config: &SpawnConfig) {
        let mut budget = self.parent_budget.lock().unwrap();
        budget.used_tokens += config.budget.used_tokens;
        budget.used_turns += config.budget.used_turns;
        budget.used_cost += config.budget.used_cost;
    }

    /// Returns a snapshot of the parent budget.
    pub fn parent_budget(&self) -> ResourceBudget {
        self.parent_budget.lock().unwrap().clone()
    }

    /// Handle a child agent's successful completion.
    ///
    /// Deducts the child's resource usage (tokens, turns, cost) from the parent
    /// budget so the parent's remaining budget accurately reflects total work.
    pub fn handle_completion(&self, config: &SpawnConfig) {
        self.deduct_child_usage(config);
        tracing::info!(
            "gen_ai.agent.name" = config.agent_id.0.as_str(),
            "child agent completed, budget deducted from parent"
        );
    }

    /// Apply a restart strategy after an agent failure.
    ///
    /// Tracks retry counts per agent. Returns true if the agent should be
    /// restarted, false if the failure should propagate.
    ///
    /// For `RestartStrategy::SnapshotAndFail`, the caller should invoke
    /// `save_checkpoint(` on the journal to persist a `Checkpoint` before
    /// propagating.
    ///
    /// Logs the restart at WARN with agent name, strategy, and failure reason.
    pub fn handle_failure(
        &self,
        agent_id: &AgentId,
        strategy: &RestartStrategy,
        failure_reason: &str,
    ) -> bool {
        let strategy_name = match strategy {
            RestartStrategy::RetryOnce => "retry_once",
            RestartStrategy::RetryTwiceThenFail => "retry_twice_then_fail",
            RestartStrategy::SnapshotAndFail => "snapshot_and_fail",
            RestartStrategy::LetCrash => "let_crash",
        };

        let should_restart = match strategy {
            RestartStrategy::RetryOnce => {
                let mut counts = self.retry_counts.lock().unwrap();
                let count = counts.entry(agent_id.clone()).or_insert(0);
                if *count < 1 {
                    *count += 1;
                    true
                } else {
                    false
                }
            }
            RestartStrategy::RetryTwiceThenFail => {
                let mut counts = self.retry_counts.lock().unwrap();
                let count = counts.entry(agent_id.clone()).or_insert(0);
                if *count < 2 {
                    *count += 1;
                    true
                } else {
                    false
                }
            }
            RestartStrategy::SnapshotAndFail => {
                self.snapshot_before_fail(agent_id, failure_reason);
                false
            }
            RestartStrategy::LetCrash => false,
        };

        if should_restart {
            tracing::warn!(
                "gen_ai.agent.name" = agent_id.0.as_str(),
                strategy = strategy_name,
                failure_reason = failure_reason,
                "agent restart triggered"
            );
        }

        should_restart
    }

    /// Save a journal Checkpoint before propagating a SnapshotAndFail failure.
    ///
    /// Behaves the same as `LetCrash` when no journal is wired. When a journal
    /// is available, persists `CheckpointData` so fork-from-checkpoint recovery
    /// is possible. Messages and vfs_snapshot are empty here because the
    /// supervisor does not hold conversational state directly — the journal
    /// already contains the full history up to this point.
    fn snapshot_before_fail(&self, agent_id: &AgentId, failure_reason: &str) {
        let checkpoint_data = simulacra_types::CheckpointData {
            messages: vec![],
            budget_snapshot: self.parent_budget.lock().unwrap().clone(),
            vfs_snapshot: None,
        };

        // WARNING 2 fix: this path was previously logging "checkpoint saved"
        // while not actually calling `save_checkpoint`. That silently claimed
        // a durable snapshot that didn't exist. We now attempt a real persist
        // when a journal is wired, and log a clear "NOT persisted" message
        // otherwise so operators aren't misled.
        if let Some(ref journal) = self.journal_storage {
            // Serialize the checkpoint into the `snapshot_data` payload and
            // append a Checkpoint journal entry (save_checkpoint writes the
            // entry under the agent's journal so it is recoverable via
            // `JournalStorage::latest_checkpoint` / `fork_from`).
            //
            // We use index 0 as a sentinel here because the supervisor does
            // not track a per-agent turn counter. When the supervisor is
            // wired to track the child's turn index, this value should
            // match the frontier at failure time.
            match journal.save_checkpoint(agent_id, 0, checkpoint_data) {
                Ok(()) => {
                    tracing::info!(
                        "gen_ai.agent.name" = agent_id.0.as_str(),
                        failure_reason = failure_reason,
                        "SnapshotAndFail: checkpoint persisted via journal.save_checkpoint"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "gen_ai.agent.name" = agent_id.0.as_str(),
                        failure_reason = failure_reason,
                        error = %e,
                        "SnapshotAndFail: save_checkpoint failed — falling back to \
                         LetCrash semantics (no durable snapshot)"
                    );
                }
            }
        } else {
            tracing::warn!(
                "gen_ai.agent.name" = agent_id.0.as_str(),
                failure_reason = failure_reason,
                "SnapshotAndFail: checkpoint data built but NOT persisted — \
                 supervisor has no journal_storage wired. Behaviour equivalent to \
                 LetCrash. TODO: wire set_journal_storage at construction."
            );
        }
    }

    /// Run the supervisor actor loop.
    ///
    /// Receives `SupervisorMessage` values from the `mpsc::Receiver` and
    /// dispatches them using `tokio::select!`. Child agents are launched
    /// via `tokio::spawn` and tracked in the children map.
    ///
    /// The loop exits when the channel closes (all senders dropped) OR when
    /// all spawned child tasks have completed and there are no more pending
    /// messages to process.
    pub async fn run_actor_loop(&self, mut rx: tokio::sync::mpsc::Receiver<SupervisorMessage>) {
        use tokio::sync::mpsc::error::TryRecvError;

        let mut priority_queue: BinaryHeap<SupervisorMessage> = BinaryHeap::new();
        let mut task_set = tokio::task::JoinSet::<()>::new();
        let mut ever_dispatched = false;

        loop {
            // If we have active tasks, select on both new messages and task completion
            if !task_set.is_empty() {
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some(supervisor_msg) => {
                                priority_queue.push(supervisor_msg);
                                while let Ok(extra) = rx.try_recv() {
                                    priority_queue.push(extra);
                                }
                                while let Some(queued) = priority_queue.pop() {
                                    self.dispatch_message_into(&mut task_set, queued).await;
                                    ever_dispatched = true;
                                }
                            }
                            None => {
                                // Channel closed — drain remaining tasks before exiting
                                // so that spawned children can send their results back.
                                while task_set.join_next().await.is_some() {}
                                break;
                            }
                        }
                    }
                    _ = task_set.join_next() => {
                        // A task completed. If all tasks are done and no pending
                        // messages, check if we should exit.
                        if task_set.is_empty() {
                            match rx.try_recv() {
                                Ok(msg) => {
                                    priority_queue.push(msg);
                                    while let Ok(extra) = rx.try_recv() {
                                        priority_queue.push(extra);
                                    }
                                    while let Some(queued) = priority_queue.pop() {
                                        self.dispatch_message_into(&mut task_set, queued).await;
                                        ever_dispatched = true;
                                    }
                                }
                                Err(TryRecvError::Empty) if ever_dispatched => break,
                                Err(TryRecvError::Disconnected) => break,
                                Err(TryRecvError::Empty) => {
                                    // Haven't dispatched yet, keep waiting
                                }
                            }
                        }
                    }
                }
            } else if ever_dispatched {
                // All tasks finished and we've dispatched before — check for more
                match rx.try_recv() {
                    Ok(msg) => {
                        priority_queue.push(msg);
                        while let Ok(extra) = rx.try_recv() {
                            priority_queue.push(extra);
                        }
                        while let Some(queued) = priority_queue.pop() {
                            self.dispatch_message_into(&mut task_set, queued).await;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                // No tasks yet, wait for first message
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(supervisor_msg) => {
                                priority_queue.push(supervisor_msg);
                                while let Ok(extra) = rx.try_recv() {
                                    priority_queue.push(extra);
                                }
                                while let Some(queued) = priority_queue.pop() {
                                    self.dispatch_message_into(&mut task_set, queued).await;
                                    ever_dispatched = true;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }

    /// Spawn a task via the factory into the JoinSet and return its handle info.
    fn spawn_task_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        config: SpawnConfig,
        result_tx: tokio::sync::oneshot::Sender<SpawnResult>,
    ) {
        if let Some(ref factory) = self.task_factory {
            let agent_id = config.agent_id.clone();
            let restart_strategy = config.restart_strategy.clone();
            let retry_config = config.clone();
            let token = CancellationToken::new(Duration::from_secs(5));
            let task_future = factory.create_task(config, token.clone());
            let parent_budget = Arc::clone(&self.parent_budget);
            let factory_clone = Arc::clone(factory);
            let retry_counts = Arc::clone(&self.retry_counts_shared);

            self.cancellation_tokens
                .lock()
                .unwrap()
                .insert(agent_id.clone(), token);

            task_set.spawn(async move {
                match task_future.await {
                    Ok(output) => {
                        // Roll up child budget from actual AgentLoopOutput,
                        // not the stale SpawnConfig clone (S018 fix).
                        {
                            let mut budget = parent_budget.lock().unwrap();
                            budget.used_tokens +=
                                output.token_usage.input_tokens + output.token_usage.output_tokens;
                            budget.used_turns += output.used_turns;
                            budget.used_cost += output.used_cost;
                        }
                        // Send result back to the spawn_agent caller.
                        let _ = result_tx.send(Ok(output));
                    }
                    Err(err) => {
                        // Check restart strategy
                        let should_restart = match restart_strategy {
                            RestartStrategy::RetryOnce => {
                                let mut counts = retry_counts.lock().unwrap();
                                let count = counts.entry(agent_id.clone()).or_insert(0);
                                if *count < 1 {
                                    *count += 1;
                                    true
                                } else {
                                    false
                                }
                            }
                            RestartStrategy::RetryTwiceThenFail => {
                                let mut counts = retry_counts.lock().unwrap();
                                let count = counts.entry(agent_id.clone()).or_insert(0);
                                if *count < 2 {
                                    *count += 1;
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };
                        if should_restart {
                            spawn_retry_task(
                                factory_clone,
                                retry_config,
                                parent_budget,
                                retry_counts,
                            );
                        }
                        // Send error back to the spawn_agent caller.
                        let _ = result_tx.send(Err(err));
                    }
                }
            });
        } else {
            // No factory — send error back immediately.
            let _ = result_tx.send(Err(RuntimeError::Session(
                "supervisor has no task factory configured".into(),
            )));
        }
    }
}

/// Create a no-op Waker for synchronous future polling.
fn noop_waker() -> std::task::Waker {
    fn noop_clone(_: *const ()) -> std::task::RawWaker {
        std::task::RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(noop_clone, noop, noop, noop);
    // SAFETY: The vtable functions are valid no-ops and the data pointer is null.
    unsafe { std::task::Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Spawn a retry task that handles its own failures recursively.
///
/// This is extracted as a free function so it can recurse from inside
/// a tokio::spawn (where we can't add to the parent JoinSet).
fn spawn_retry_task(
    factory: Arc<dyn TaskFactory>,
    config: SpawnConfig,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    retry_counts: Arc<Mutex<HashMap<AgentId, usize>>>,
) {
    let agent_id = config.agent_id.clone();
    let restart_strategy = config.restart_strategy.clone();
    let new_token = CancellationToken::new(Duration::from_secs(5));
    let retry_config = config.clone();
    let new_future = factory.create_task(config, new_token);

    tokio::spawn(async move {
        match new_future.await {
            Ok(output) => {
                let mut budget = parent_budget.lock().unwrap();
                budget.used_tokens +=
                    output.token_usage.input_tokens + output.token_usage.output_tokens;
                budget.used_turns += output.used_turns;
                budget.used_cost += output.used_cost;
            }
            Err(_err) => {
                // Check if we should retry again
                let should_retry = match restart_strategy {
                    RestartStrategy::RetryOnce => {
                        let mut counts = retry_counts.lock().unwrap();
                        let count = counts.entry(agent_id.clone()).or_insert(0);
                        if *count < 1 {
                            *count += 1;
                            true
                        } else {
                            false
                        }
                    }
                    RestartStrategy::RetryTwiceThenFail => {
                        let mut counts = retry_counts.lock().unwrap();
                        let count = counts.entry(agent_id.clone()).or_insert(0);
                        if *count < 2 {
                            *count += 1;
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if should_retry {
                    spawn_retry_task(factory, retry_config, parent_budget, retry_counts);
                }
            }
        }
    });
}

impl AgentSupervisor {
    /// Dispatch a single supervisor message, spawning tasks into the given JoinSet.
    ///
    /// For Spawn payloads, runs the same validation, journaling, tracing, and
    /// activity events as the direct `spawn_agent()` path via
    /// `validate_and_prepare_spawn()`.
    async fn dispatch_message_into(
        &self,
        task_set: &mut tokio::task::JoinSet<()>,
        msg: SupervisorMessage,
    ) {
        match msg.payload {
            SupervisorPayload::Spawn(config, result_tx) => {
                if let Err(err) = self.validate_and_prepare_spawn(&config) {
                    let _ = result_tx.send(Err(err));
                    return;
                }
                self.spawn_task_into(task_set, *config, result_tx);
            }
            SupervisorPayload::Cancel => {
                if let Some(token) = self.cancellation_tokens.lock().unwrap().get(&msg.agent_id) {
                    token.signal();
                }
            }
            SupervisorPayload::Completed => {
                // Budget rollup handled in the spawned task
            }
            SupervisorPayload::Failed(_reason) => {
                // Failure restart handled in the spawned task
            }
        }
    }

    /// Cancel a running agent with a grace period before forceful abort.
    ///
    /// Uses `tokio::time::timeout` with the token's `grace()` duration.
    /// If the agent does not shut down within the grace period, the task
    /// handle is forcefully terminated via `abort`.
    #[allow(dead_code)]
    async fn cancel_with_grace(&self, agent_id: &AgentId, token: &CancellationToken) {
        token.signal();
        let grace_duration = token.grace();

        let handle = self.children.lock().unwrap().remove(agent_id);
        if let Some(handle) = handle {
            // Grab an AbortHandle before passing the JoinHandle to the timeout
            // future. If the timeout expires, the JoinHandle is dropped (which
            // detaches the task), so we need the AbortHandle to still be able
            // to cancel it.
            let abort_handle = handle.abort_handle();
            let result = tokio::time::timeout(grace_duration, handle).await;
            if result.is_err() {
                // Grace period expired — forcefully terminate via the
                // AbortHandle we retained. We intentionally do NOT call
                // abort_child here because the handle was already removed
                // from self.children above and abort_child would find
                // nothing, letting the task detach.
                tracing::warn!("agent did not shut down within grace period, aborting");
                abort_handle.abort();
            }
        }
    }

    /// Abort a child task forcefully (used after grace period expiry).
    #[allow(dead_code)]
    fn abort_child(&self, agent_id: &AgentId) {
        if let Some(handle) = self.children.lock().unwrap().remove(agent_id) {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — original tests preserved
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_types::{
        AgentId, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalStorage, Message,
        Role, TokenUsage,
    };

    fn make_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            agent_id: AgentId("agent-1".into()),
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            vfs_snapshot: None,
            created_at: 1000,
            used_tokens: 0,
            used_turns: 0,
        }
    }

    #[test]
    fn session_save_load_roundtrip() {
        let storage = InMemorySessionStorage::new();
        let session = make_session("sess-1");
        storage.save(&session).unwrap();

        let loaded = storage.load("sess-1").unwrap().expect("session not found");
        assert_eq!(loaded.id, "sess-1");
        assert_eq!(loaded.agent_id, AgentId("agent-1".into()));
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.created_at, 1000);
    }

    #[test]
    fn session_load_missing_returns_none() {
        let storage = InMemorySessionStorage::new();
        let result = storage.load("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_save_overwrites() {
        let storage = InMemorySessionStorage::new();
        let mut session = make_session("sess-1");
        storage.save(&session).unwrap();

        session.messages.push(Message {
            role: Role::Assistant,
            content: "world".into(),
            tool_calls: vec![],
            tool_call_id: None,
        });
        storage.save(&session).unwrap();

        let loaded = storage.load("sess-1").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    fn make_journal_entry(agent_id: &str, kind: JournalEntryKind) -> JournalEntry {
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId(agent_id.into()),
            timestamp_ms: 1000,
            entry: kind,
        }
    }

    #[test]
    fn journal_append_and_read_all_roundtrip() {
        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());

        storage
            .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::ShellCommand {
                    command: "echo hi".into(),
                    exit_code: 0,
                },
            ))
            .unwrap();
        // Different agent — should not appear in query
        storage
            .append(make_journal_entry("agent-2", JournalEntryKind::TurnStart))
            .unwrap();

        let entries = storage.read_all(&agent).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn journal_query_token_usage() {
        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());

        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                    },
                    finish_reason: "stop".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 200,
                        output_tokens: 75,
                    },
                    finish_reason: "stop".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();
        // Different agent
        storage
            .append(make_journal_entry(
                "agent-2",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 999,
                        output_tokens: 999,
                    },
                    finish_reason: "stop".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();

        let usage = storage.query_token_usage(&agent).unwrap();
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 125);
        assert_eq!(usage.total(), 425);
    }

    #[test]
    fn journal_query_token_usage_no_entries() {
        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());
        let usage = storage.query_token_usage(&agent).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    // -----------------------------------------------------------------------
    // S005: Checkpoint + fork creates independent journal sharing history
    // -----------------------------------------------------------------------
    #[test]
    fn checkpoint_fork_creates_independent_journal() {
        use rust_decimal::Decimal;
        use simulacra_types::{CheckpointData, ResourceBudget};

        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());

        // Append some entries before the checkpoint
        storage
            .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmRequest {
                    model: "gpt-4".into(),
                    message_count: 2,
                },
            ))
            .unwrap();

        // Save a checkpoint at index 2 (after the 2 entries above)
        let checkpoint_data = CheckpointData {
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            budget_snapshot: ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5),
            vfs_snapshot: None,
        };
        storage.save_checkpoint(&agent, 2, checkpoint_data).unwrap();

        // Append more entries after the checkpoint
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                    },
                    finish_reason: "stop".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();

        // Fork from the checkpoint (index 2 — the checkpoint entry)
        let forked = storage.fork_from(&agent, 2).unwrap();

        // Forked journal shares history up to and including the checkpoint
        assert_eq!(forked.len(), 3); // TurnStart + LlmRequest + Checkpoint
        assert!(matches!(forked[0].entry, JournalEntryKind::TurnStart));
        assert!(matches!(
            forked[1].entry,
            JournalEntryKind::LlmRequest { .. }
        ));
        assert!(matches!(
            forked[2].entry,
            JournalEntryKind::Checkpoint { .. }
        ));

        // The post-checkpoint entry (LlmResponse) is NOT in the forked journal
        // Original journal has 4 entries for this agent
        let all = storage.read_all(&agent).unwrap();
        assert_eq!(all.len(), 4);

        // Forked journal is independent — only 3 entries
        assert_eq!(forked.len(), 3);
    }

    // -----------------------------------------------------------------------
    // S005: Schema version mismatch produces clear error
    // -----------------------------------------------------------------------
    #[test]
    fn schema_version_mismatch_produces_error() {
        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());

        // Append an entry with a future schema version
        let future_entry = JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION + 1,
            agent_id: agent.clone(),
            timestamp_ms: 1000,
            entry: JournalEntryKind::TurnStart,
        };
        // append itself does not validate — it's the storage write path
        storage.append(future_entry).unwrap();

        // read_all should detect the version mismatch
        let result = storage.read_all(&agent);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            simulacra_types::JournalError::SchemaVersionMismatch { expected, got } => {
                assert_eq!(expected, JOURNAL_SCHEMA_VERSION);
                assert_eq!(got, JOURNAL_SCHEMA_VERSION + 1);
            }
            other => panic!("expected SchemaVersionMismatch, got: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // S005: Replay from checkpoint skips entries before checkpoint
    // -----------------------------------------------------------------------
    #[test]
    fn replay_from_checkpoint_skips_earlier_entries() {
        use rust_decimal::Decimal;
        use simulacra_types::{CheckpointData, ResourceBudget};

        let storage = InMemoryJournalStorage::new();
        let agent = AgentId("agent-1".into());

        // 3 entries before the checkpoint
        storage
            .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmRequest {
                    model: "m".into(),
                    message_count: 1,
                },
            ))
            .unwrap();
        storage
            .append(make_journal_entry(
                "agent-1",
                JournalEntryKind::LlmResponse {
                    model: "m".into(),
                    token_usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    finish_reason: "stop".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();

        // Checkpoint at index 3
        let checkpoint_data = CheckpointData {
            messages: vec![],
            budget_snapshot: ResourceBudget::new(100_000, 10, Decimal::new(100, 0), 5),
            vfs_snapshot: None,
        };
        storage.save_checkpoint(&agent, 3, checkpoint_data).unwrap();

        // 1 entry after the checkpoint
        storage
            .append(make_journal_entry("agent-1", JournalEntryKind::TurnStart))
            .unwrap();

        // read_from starting after the checkpoint (index 4) skips everything before
        let entries = storage.read_from(&agent, 4).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));

        // read_from starting at the checkpoint itself (index 3) includes checkpoint + after
        let entries = storage.read_from(&agent, 3).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries[0].entry,
            JournalEntryKind::Checkpoint { .. }
        ));
    }
}
