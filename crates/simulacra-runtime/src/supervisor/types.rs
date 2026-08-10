use super::*;

/// A boxed future that resolves to an agent loop result.
pub type BoxTaskFuture =
    Pin<Box<dyn Future<Output = Result<AgentLoopOutput, RuntimeError>> + Send + 'static>>;

/// Factory for creating agent tasks. Allows the supervisor to spawn child
/// agents without knowing the concrete task implementation.
pub trait TaskFactory: Send + Sync {
    /// Validate that the factory can accept this spawn before the supervisor
    /// returns a live child handle.
    fn validate_spawn_config(&self, config: &SpawnConfig) -> Result<(), RuntimeError> {
        Err(RuntimeError::Session(format!(
            "unknown child placement {:?}",
            config.placement
        )))
    }

    /// Resolve the runtime backend selected by the opaque placement.
    /// This is reached only after explicit placement validation succeeds.
    fn placement_backend(&self, _config: &SpawnConfig) -> AgentBackend {
        AgentBackend::Native
    }

    /// Apply policy shaping after ordinary validation but before the
    /// supervisor reserves budget or records an accepted spawn.
    fn prepare_spawn_config(&self, _config: &mut SpawnConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Apply policy shaping using the authenticated immediate caller's
    /// effective capability. Placement-aware factories override this method;
    /// placement-agnostic factories retain their existing preparation hook.
    fn prepare_spawn_config_for_caller(
        &self,
        config: &mut SpawnConfig,
        _caller_capability: &CapabilityToken,
    ) -> Result<(), RuntimeError> {
        self.prepare_spawn_config(config)
    }

    /// Run policy completion handling after the child runtime returns and
    /// before its terminal result is cached or journaled by the supervisor.
    fn after_spawn(
        &self,
        _config: &SpawnConfig,
        _result: &SpawnResult,
    ) -> Result<(), RuntimeError> {
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

    /// Create a task using the supervisor's authoritative budget handle for
    /// this child. Native runtimes override this to share live usage with
    /// descendant-spawn accounting; simpler factories may use the value-only
    /// compatibility path.
    fn create_task_with_input_and_budget(
        &self,
        config: SpawnConfig,
        cancellation: CancellationToken,
        input_queue: AgentInputQueue,
        _budget: Arc<Mutex<ResourceBudget>>,
    ) -> BoxTaskFuture {
        self.create_task_with_input(config, cancellation, input_queue)
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
    /// Immutable identity of the immediate caller that submitted this message.
    pub agent_id: AgentId,
    pub payload: SupervisorPayload,
}

/// Terminal child result retained for direct supervisor callers and join handling.
pub type SpawnResult = Result<AgentLoopOutput, RuntimeError>;

/// Immediate acknowledgement returned once a child spawn is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAck {
    pub child_id: AgentId,
    pub placement: String,
    pub backend: AgentBackend,
}

/// Cached terminal child result returned by join_child_agent.
#[derive(Debug, Clone)]
pub struct ChildTerminalResult {
    pub child_id: AgentId,
    pub placement: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub tool_uses: u64,
    pub result: Result<AgentLoopOutput, String>,
}

/// Host-only snapshot of a cached terminal result and its delivery state.
#[derive(Debug, Clone)]
pub struct ChildResultInspection {
    pub terminal: ChildTerminalResult,
    pub result_delivered: bool,
}

pub(crate) fn final_assistant_message(output: &AgentLoopOutput) -> Option<String> {
    output
        .messages
        .iter()
        .rev()
        .find(|message| message.role == simulacra_types::Role::Assistant)
        .map(|message| message.content.clone())
}

/// Stable metadata retained for each accepted child handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildMetadata {
    pub child_id: AgentId,
    pub placement: String,
    pub task: String,
    pub parent_id: AgentId,
    pub capability: CapabilityToken,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// Shared model-visible child lifecycle status used by status and roster probes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildAgentStatus {
    Running,
    Completed(Option<String>),
    Failed(Option<String>),
    Cancelled(Option<String>),
}

/// Lightweight child status returned by child_status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildStatus {
    pub child_id: AgentId,
    pub placement: String,
    pub status: ChildAgentStatus,
    pub ready: bool,
    pub elapsed_ms: u64,
}

/// Child roster entry returned by list_child_agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRosterEntry {
    pub child_id: String,
    pub placement: String,
    pub task: String,
    pub status: ChildAgentStatus,
    pub ready: bool,
    pub elapsed_ms: u64,
}

/// Result returned by a bounded wait_child_agent request.
#[derive(Debug, Clone)]
pub struct WaitChildResult {
    pub child_id: AgentId,
    pub placement: Option<String>,
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
    /// Inspect cached terminal state without delivering it to the parent model.
    InspectChildResult(
        AgentId,
        tokio::sync::oneshot::Sender<Result<ChildResultInspection, String>>,
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
    /// List every live or terminal-unclosed child handle.
    ListChildren(tokio::sync::oneshot::Sender<Result<Vec<ChildRosterEntry>, String>>),
    /// Host-only roster inspection: the same entries as ListChildren but
    /// without acknowledging any terminal-result handoff. Housekeeping sweeps
    /// (e.g. end-of-turn supervisor teardown) must use this so they never
    /// disarm a pending delivery to the parent model.
    InspectChildren(tokio::sync::oneshot::Sender<Result<Vec<ChildRosterEntry>, String>>),
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
/// Placement-backed child request crossing the tool/supervisor boundary.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub agent_id: AgentId,
    pub parent_id: AgentId,
    pub capability: Option<CapabilityToken>,
    pub budget: ResourceBudget,
    pub restart_strategy: RestartStrategy,
    /// Opaque configured child placement key from `child_placements`.
    pub placement: String,
    /// The delegated task text passed to the child AgentLoop::run(task).
    pub task: String,
    /// Optional caller-supplied shaping instructions, preserved byte-for-byte.
    pub instructions: Option<String>,
}
