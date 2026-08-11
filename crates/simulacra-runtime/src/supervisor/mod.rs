mod actor;
mod child_queries;
mod dispatch;
mod restart;
mod results;
mod spawn;
mod types;

#[cfg(feature = "spawn")]
pub(crate) use spawn::status_from_spawn_result;
pub(crate) use types::final_assistant_message;
pub use types::{
    BoxTaskFuture, CancellationToken, ChildAgentStatus, ChildMetadata, ChildResultInspection,
    ChildRosterEntry, ChildStatus, ChildTerminalResult, MessagePriority, RestartStrategy, SpawnAck,
    SpawnConfig, SpawnResult, SupervisorMessage, SupervisorPayload, TaskFactory, WaitChildResult,
    WaitChildrenResult,
};

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::exit_reason::exit_reason_to_snake_case;
use crate::{
    ActivitySink, AgentInputQueue, AgentLoopOutput, ChildInputHandle, NoopActivitySink,
    RuntimeError,
};
use simulacra_config::AgentBackend;
use simulacra_types::{ActivityEvent, AgentId, CapabilityToken, ResourceBudget};
use tokio::task::JoinHandle;

type SpawnSpanKey = (String, String);

static PENDING_SPAWN_PARENT_SPANS: std::sync::LazyLock<
    Mutex<HashMap<SpawnSpanKey, tracing::Span>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "spawn")]
pub(crate) fn register_spawn_parent_span(
    parent_id: &AgentId,
    child_id: &AgentId,
    span: tracing::Span,
) {
    lock_mutex(&PENDING_SPAWN_PARENT_SPANS, "pending_spawn_parent_spans")
        .insert((parent_id.0.clone(), child_id.0.clone()), span);
}

pub(crate) fn take_spawn_parent_span(
    parent_id: &AgentId,
    child_id: &AgentId,
) -> Option<tracing::Span> {
    lock_mutex(&PENDING_SPAWN_PARENT_SPANS, "pending_spawn_parent_spans")
        .remove(&(parent_id.0.clone(), child_id.0.clone()))
}

type ChildJoinSender = tokio::sync::oneshot::Sender<Result<ChildTerminalResult, String>>;
type ChildWaitSender = tokio::sync::oneshot::Sender<Result<WaitChildResult, String>>;
type ChildrenWaitSender = tokio::sync::oneshot::Sender<Result<WaitChildrenResult, String>>;

struct ChildRunState {
    metadata: ChildMetadata,
    result: Option<ChildTerminalResult>,
    result_delivered: bool,
    join_waiters: Vec<ChildJoinSender>,
    wait_waiters: Vec<ChildWaiter>,
}

/// Authoritative budget state for one agent while it is supervised.
///
/// Actual usage remains in `budget` so existing runtime consumers observe the
/// same `ResourceBudget`. Outstanding child reservations are deliberately kept
/// beside it: a reservation constrains future delegation without pretending
/// that the reserved amount has already been consumed.
struct AgentBudgetAccount {
    budget: Arc<Mutex<ResourceBudget>>,
    reserved_tokens: u64,
    reserved_turns: u32,
    reserved_cost: rust_decimal::Decimal,
}

impl AgentBudgetAccount {
    fn new(budget: Arc<Mutex<ResourceBudget>>) -> Self {
        Self {
            budget,
            reserved_tokens: 0,
            reserved_turns: 0,
            reserved_cost: rust_decimal::Decimal::ZERO,
        }
    }
}

struct BudgetReservation {
    parent: Arc<Mutex<AgentBudgetAccount>>,
    tokens: u64,
    turns: u32,
    cost: rust_decimal::Decimal,
}

struct ChildWaiter {
    id: u64,
    child_ids: Vec<AgentId>,
    sender: Arc<Mutex<Option<ChildWaiterSender>>>,
}

enum ChildWaiterSender {
    Single(ChildWaitSender),
    Any(ChildrenWaitSender),
}

impl Clone for ChildWaiter {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            child_ids: self.child_ids.clone(),
            sender: Arc::clone(&self.sender),
        }
    }
}

fn lock_mutex<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(mutex = name, "recovering poisoned supervisor mutex");
            poisoned.into_inner()
        }
    }
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
    root_agent_id: Mutex<Option<AgentId>>,
    parent_budget: Arc<Mutex<ResourceBudget>>,
    root_budget_account: Arc<Mutex<AgentBudgetAccount>>,
    /// Every child id that this supervisor has accepted. IDs are deliberately
    /// retained after terminal delivery and explicit close so an opaque host
    /// id can never be recycled into a different lifecycle.
    accepted_child_ids: Mutex<HashSet<AgentId>>,
    child_budget_accounts: Arc<Mutex<HashMap<AgentId, Arc<Mutex<AgentBudgetAccount>>>>>,
    budget_reservations: Arc<Mutex<HashMap<AgentId, BudgetReservation>>>,
    retry_counts: Mutex<HashMap<AgentId, usize>>,
    /// Shared retry counts accessible from spawned tasks.
    retry_counts_shared: Arc<Mutex<HashMap<AgentId, usize>>>,
    children: Mutex<HashMap<AgentId, JoinHandle<()>>>,
    cancellation_tokens: Arc<Mutex<HashMap<AgentId, CancellationToken>>>,
    child_inputs: Arc<Mutex<HashMap<AgentId, ChildInputHandle>>>,
    child_results: Arc<Mutex<HashMap<AgentId, ChildRunState>>>,
    wait_counter: Arc<AtomicU64>,
    task_factory: Option<Arc<dyn TaskFactory>>,
    #[allow(dead_code)]
    spawn_configs: Mutex<HashMap<AgentId, SpawnConfig>>,
    /// Optional JournalStorage for recording sub-agent lifecycle entries.
    /// When set, the supervisor appends SubAgentSpawned before child execution
    /// and SubAgentCompleted after child completion/failure, per S018.
    /// Child journal entries are written under the child_id and can be
    /// correlated to the parent by child_id.
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
        let parent_budget = Arc::new(Mutex::new(parent_budget));
        Self {
            parent_capability,
            root_agent_id: Mutex::new(None),
            parent_budget: Arc::clone(&parent_budget),
            root_budget_account: Arc::new(Mutex::new(AgentBudgetAccount::new(parent_budget))),
            accepted_child_ids: Mutex::new(HashSet::new()),
            child_budget_accounts: Arc::new(Mutex::new(HashMap::new())),
            budget_reservations: Arc::new(Mutex::new(HashMap::new())),
            retry_counts: Mutex::new(HashMap::new()),
            retry_counts_shared: Arc::new(Mutex::new(HashMap::new())),
            children: Mutex::new(HashMap::new()),
            cancellation_tokens: Arc::new(Mutex::new(HashMap::new())),
            child_inputs: Arc::new(Mutex::new(HashMap::new())),
            child_results: Arc::new(Mutex::new(HashMap::new())),
            wait_counter: Arc::new(AtomicU64::new(1)),
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
        Self::with_task_factory_and_shared_budget(
            parent_capability,
            Arc::new(Mutex::new(parent_budget)),
            task_factory,
        )
    }

    /// Create a new supervisor with a task factory and a shared parent budget.
    pub fn with_task_factory_and_shared_budget(
        parent_capability: CapabilityToken,
        parent_budget: Arc<Mutex<ResourceBudget>>,
        task_factory: Arc<dyn TaskFactory>,
    ) -> Self {
        let root_budget_account = Arc::new(Mutex::new(AgentBudgetAccount::new(Arc::clone(
            &parent_budget,
        ))));
        Self {
            parent_capability,
            root_agent_id: Mutex::new(None),
            parent_budget,
            root_budget_account,
            accepted_child_ids: Mutex::new(HashSet::new()),
            child_budget_accounts: Arc::new(Mutex::new(HashMap::new())),
            budget_reservations: Arc::new(Mutex::new(HashMap::new())),
            retry_counts: Mutex::new(HashMap::new()),
            retry_counts_shared: Arc::new(Mutex::new(HashMap::new())),
            children: Mutex::new(HashMap::new()),
            cancellation_tokens: Arc::new(Mutex::new(HashMap::new())),
            child_inputs: Arc::new(Mutex::new(HashMap::new())),
            child_results: Arc::new(Mutex::new(HashMap::new())),
            wait_counter: Arc::new(AtomicU64::new(1)),
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

    /// Bind the root caller identity before the actor accepts model-visible
    /// messages. Descendants are authenticated from accepted child metadata.
    pub fn set_root_agent_id(&mut self, root_agent_id: AgentId) {
        *lock_mutex(&self.root_agent_id, "root_agent_id") = Some(root_agent_id);
    }
}
