use super::*;

/// A boxed future that resolves to an agent loop result.
pub type BoxTaskFuture =
    Pin<Box<dyn Future<Output = Result<AgentLoopOutput, RuntimeError>> + Send + 'static>>;

/// Factory for creating agent tasks. Allows the supervisor to spawn child
/// agents without knowing the concrete task implementation.
pub trait TaskFactory: Send + Sync {
    /// Validate that the factory can accept this spawn before the supervisor
    /// returns a live child handle.
    fn validate_spawn_config(&self, _config: &SpawnConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Create a task future for the given spawn configuration and cancellation token.
    fn create_task(&self, config: SpawnConfig, cancellation: CancellationToken) -> BoxTaskFuture;

    /// Create a task future with a queue for cooperative child steering.
    ///
    /// Existing factories that do not run a real `AgentLoop` can ignore queued
    /// input by relying on this default implementation.
    fn create_task_with_input(
        &self,
        config: SpawnConfig,
        cancellation: CancellationToken,
        input_queue: AgentInputQueue,
    ) -> BoxTaskFuture {
        let task = self.create_task(config, cancellation);
        Box::pin(async move {
            let _input_queue = input_queue;
            task.await
        })
    }
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

/// Terminal child result retained for direct supervisor callers and join handling.
pub type SpawnResult = Result<AgentLoopOutput, RuntimeError>;

/// Immediate acknowledgement returned once a child spawn is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAck {
    pub child_id: AgentId,
    pub agent_type: String,
}

/// Cached terminal child result returned by join_child_agent.
#[derive(Debug, Clone)]
pub struct ChildTerminalResult {
    pub child_id: AgentId,
    pub agent_type: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub tool_uses: u64,
    pub result: Result<AgentLoopOutput, String>,
}

/// Stable metadata retained for each accepted child handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildMetadata {
    pub child_id: AgentId,
    pub agent_type: String,
    pub task: String,
    pub parent_id: AgentId,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// Lightweight child status returned by child_status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildStatus {
    pub child_id: AgentId,
    pub agent_type: String,
    pub status: String,
    pub ready: bool,
    pub elapsed_ms: u64,
}

/// Result returned by a bounded wait_child_agent request.
#[derive(Debug, Clone)]
pub struct WaitChildResult {
    pub child_id: AgentId,
    pub agent_type: Option<String>,
    pub status: String,
    pub ready: bool,
    pub terminal: Option<ChildTerminalResult>,
}

/// Result returned by a bounded wait_child_agent request over multiple children.
#[derive(Debug, Clone)]
pub struct WaitChildrenResult {
    pub child_ids: Vec<AgentId>,
    pub status: String,
    pub ready: bool,
    pub terminal: Option<ChildTerminalResult>,
}

/// Payload variants for supervisor messages.
#[derive(Debug)]
pub enum SupervisorPayload {
    /// Agent completed successfully.
    Completed,
    /// Agent failed with the given reason.
    Failed(String),
    /// Spawn a new child agent. The oneshot sender receives an accepted-spawn
    /// acknowledgement; terminal results are collected later via JoinChild.
    Spawn(
        Box<SpawnConfig>,
        tokio::sync::oneshot::Sender<Result<SpawnAck, RuntimeError>>,
    ),
    /// Join a live or completed child agent by id.
    JoinChild(
        AgentId,
        tokio::sync::oneshot::Sender<Result<ChildTerminalResult, String>>,
    ),
    /// Cancel a live child agent by id.
    CancelChild(AgentId, tokio::sync::oneshot::Sender<Result<(), String>>),
    /// Queue steering input for a live child agent.
    SteerChild(
        AgentId,
        String,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
    /// Inspect a live or completed child handle by id.
    ChildStatus(
        AgentId,
        tokio::sync::oneshot::Sender<Result<ChildStatus, String>>,
    ),
    /// Wait for a child up to a bounded timeout without consuming the result.
    WaitChild(
        AgentId,
        Duration,
        tokio::sync::oneshot::Sender<Result<WaitChildResult, String>>,
    ),
    /// Wait for any child in a set up to a bounded timeout without consuming results.
    WaitChildren(
        Vec<AgentId>,
        Duration,
        tokio::sync::oneshot::Sender<Result<WaitChildrenResult, String>>,
    ),
    /// Release a terminal child handle and cached result.
    CloseChild(AgentId, tokio::sync::oneshot::Sender<Result<(), String>>),
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
