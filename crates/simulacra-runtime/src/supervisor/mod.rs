mod actor;
mod restart;
mod results;
mod spawn;
mod types;

pub use types::{
    BoxTaskFuture, CancellationToken, MessagePriority, RestartStrategy, SpawnConfig, SpawnResult,
    SupervisorMessage, SupervisorPayload, TaskFactory,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crate::exit_reason::exit_reason_to_snake_case;
use crate::{ActivitySink, AgentLoopOutput, NoopActivitySink, RuntimeError};
use simulacra_types::{ActivityEvent, AgentId, CapabilityToken, ResourceBudget};
use tokio::task::JoinHandle;

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
}
