//! Task lifecycle state machine and TaskManager.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::metrics::ServerMeters;

use crate::tenant::TenantConfig;

// ──────────────────────────────────────────────────────────────────────────────
// Clock abstraction for grace-period cleanup
// ──────────────────────────────────────────────────────────────────────────────

/// Default grace period after which terminal `TaskRecord`s are evicted.
///
/// SSE replay relies on the per-task event log; once the SSE stream has had
/// time to drain (an hour is generous for any real client), the record is
/// removed to bound memory usage on long-running servers.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(3600);

/// Abstract clock used by [`TaskManager`] for grace-period bookkeeping.
///
/// Production code uses [`SystemClock`] (wall-clock `Instant::now`). Tests
/// inject [`TestClock`] which advances explicitly via [`TestClock::advance`],
/// avoiding sleeps and tokio timer pause/advance interactions.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Instant;
}

/// Real-time clock backed by `std::time::Instant::now`.
#[derive(Debug, Default, Clone)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Manually-advanced clock for tests.
///
/// Internally an `Arc<Mutex<Instant>>` so cloning yields a shared clock —
/// advancing one clone advances every clone, which mirrors how the real clock
/// behaves and makes the test API ergonomic.
#[derive(Debug, Clone)]
pub struct TestClock {
    inner: Arc<Mutex<Instant>>,
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl TestClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Advance the virtual time by `delta`.
    pub fn advance(&self, delta: Duration) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g += delta;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Task state machine
// ──────────────────────────────────────────────────────────────────────────────

/// Task lifecycle state.
///
/// ```text
/// pending → running → {streaming, waiting_input, waiting_approval, paused}
///        → {completed, failed, killed, cancelled}  (terminal)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Task created, queued for execution.
    Pending,
    /// Agent loop active.
    Running,
    /// Agent producing output (sub-state of running).
    Streaming,
    /// Agent requested user input via `input.required`.
    WaitingInput,
    /// Tool call requires human approval via `tool.approval_required`.
    WaitingApproval,
    /// Explicitly paused by client via `task.pause`.
    Paused,
    /// Agent finished successfully.
    Completed,
    /// Agent encountered unrecoverable error.
    Failed,
    /// Terminated by budget exhaustion or system.
    Killed,
    /// Cancelled by client via `task.cancel`.
    Cancelled,
}

impl TaskState {
    /// Returns true if this is a terminal (non-recoverable) state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Killed | Self::Cancelled
        )
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Streaming => "streaming",
            Self::WaitingInput => "waiting_input",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Cancelled => "cancelled",
        };
        write!(f, "{s}")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Task handle
// ──────────────────────────────────────────────────────────────────────────────

/// A handle to a running or completed task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHandle {
    pub task_id: String,
    pub state: TaskState,
    pub metadata: Value,
    pub description: String,
    pub agent_type: String,
    pub tenant: String,
    /// Monotonic event sequence number for this task.
    pub seq: u64,
    /// WebSocket connection_id that owns this task, if created over WS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Task manager errors
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Error)]
pub enum TaskManagerError {
    #[error("task '{task_id}' not found")]
    NotFound { task_id: String },

    #[error("task '{task_id}' is in terminal state '{state}' — no transitions allowed")]
    TerminalState { task_id: String, state: TaskState },

    #[error("task '{task_id}' cannot transition from '{from}' to '{to}'")]
    InvalidTransition {
        task_id: String,
        from: TaskState,
        to: TaskState,
    },

    #[error("approval denied for tool call '{tool_call_id}': {reason}")]
    ApprovalDenied {
        tool_call_id: String,
        reason: String,
    },

    #[error("lock poisoned")]
    LockPoisoned,

    #[error("subscribe failed: task '{task_id}' not found")]
    SubscribeFailed { task_id: String },

    /// Returned by `provide_input` and `respond_approval`. Human-in-the-loop
    /// continuation requires the engine to pause the agent loop and resume
    /// with the provided input; that wiring is not yet in place (see S031
    /// HITL milestone), so instead of silently flipping the task back to
    /// `Running` with no worker (which silently loses the input), we surface
    /// a clear error so the client learns the feature is not available.
    #[error(
        "{op} is not yet implemented — the agent worker cannot yet be resumed with human input"
    )]
    NotImplemented { op: &'static str },
}

// ──────────────────────────────────────────────────────────────────────────────
// Task event channel
// ──────────────────────────────────────────────────────────────────────────────

/// Per-task event channel that broadcasts live events AND retains a history
/// log for late subscribers.
///
/// Tokio's `broadcast::Sender` does not replay past events; a subscriber that
/// connects after the agent loop has already started would miss everything
/// emitted before subscription. The SSE consumer (browser navigating to
/// `/run/:task_id`) typically attaches *after* the agent has begun running,
/// so without replay the user sees an empty activity feed even though tool
/// calls and tokens were already emitted server-side.
///
/// Concurrency: senders briefly hold the log mutex while pushing and broadcasting,
/// so a concurrent subscriber that also takes the log mutex sees a consistent
/// snapshot — either the event is in history (and not in the receiver) or in
/// the receiver (and not in history), never both, never neither.
#[derive(Clone, Debug)]
pub struct TaskEventChannel {
    tx: broadcast::Sender<Value>,
    log: Arc<Mutex<Vec<Value>>>,
}

impl TaskEventChannel {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Append the event to the log and broadcast it. The lock is held across
    /// both operations so a concurrent `subscribe_with_history` cannot observe
    /// an event in only one of the two places.
    pub fn send(&self, event: Value) {
        let mut log = match self.log.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        log.push(event.clone());
        let _ = self.tx.send(event);
    }

    /// Returns the historical event log (cloned) plus a fresh receiver for
    /// future events. The log mutex is held across `subscribe()` so any
    /// concurrent send blocks until the snapshot+receiver pair is consistent.
    pub fn subscribe_with_history(&self) -> (Vec<Value>, broadcast::Receiver<Value>) {
        let log = match self.log.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let rx = self.tx.subscribe();
        let history = log.clone();
        (history, rx)
    }

    /// Number of currently-attached live receivers. Pinned by S034 tests.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Task manager
// ──────────────────────────────────────────────────────────────────────────────

/// Internal task record.
#[derive(Debug)]
struct TaskRecord {
    handle: TaskHandle,
    /// Pending input response, if any.
    #[allow(dead_code)] // HITL continuation is deferred; field reserved for future wiring
    pending_input: Option<String>,
    /// Pending approval response, if any.
    #[allow(dead_code)] // HITL continuation is deferred; field reserved for future wiring
    pending_approval: Option<(String, bool, Option<String>)>,
    /// Per-task event channel (broadcast + history log) for live + replay.
    event_tx: TaskEventChannel,
    /// Cancellation token for cooperative agent loop cancellation.
    cancellation_token: Option<CancellationToken>,
    /// Wall-clock time when the task first entered `Running`. Used for
    /// `simulacra.server.task_duration` histogram and `active_tasks` gauge tracking.
    running_since: Option<Instant>,
    /// Open `simulacra_server_task` span for this task's lifecycle.
    /// Created when the task first enters `Running`, dropped (exported) on terminal state.
    lifecycle_span: Option<tracing::Span>,
    /// Clock-time at which this task entered a terminal state. Used by the
    /// grace-period cleanup loop to evict records (and their event logs) once
    /// the configured grace window has elapsed. `None` while the task is
    /// non-terminal — non-terminal tasks are never auto-evicted.
    terminated_at: Option<Instant>,
}

/// Shared inner state of [`TaskManager`].
///
/// Kept in an `Arc` so that:
///   * `TaskManager` is cheaply cloneable (multiple handlers share state).
///   * The background cleanup loop holds a `Weak` reference and exits cleanly
///     when the last `TaskManager` clone is dropped.
#[derive(Debug)]
struct TaskManagerInner {
    tasks: Mutex<HashMap<String, TaskRecord>>,
    grace_period: Duration,
    clock: Arc<dyn Clock>,
}

/// Manages task lifecycle: create, cancel, pause, resume, input, approval.
///
/// In the full implementation, `TaskManager` communicates with live agent loops
/// via channels. This implementation maintains state and validates transitions.
///
/// ## Grace-period cleanup
///
/// Terminal tasks (`Completed`, `Failed`, `Killed`, `Cancelled`) are retained
/// in memory for a configurable grace period (default
/// [`DEFAULT_GRACE_PERIOD`]) so that late SSE subscribers can replay the full
/// per-task event log. After the grace period elapses, the record and its
/// event log are evicted from memory.
///
/// Non-terminal tasks (`Pending`, `Running`, `Streaming`, `WaitingInput`,
/// `WaitingApproval`, `Paused`) are NEVER evicted automatically.
///
/// Cleanup is driven by a background task spawned at construction (when a
/// tokio runtime is available) plus an explicit [`Self::cleanup_expired_now`]
/// hook used by tests.
#[derive(Debug, Clone)]
pub struct TaskManager {
    inner: Arc<TaskManagerInner>,
    /// Aborts the background cleanup loop when the last `TaskManager` clone is
    /// dropped. The handle itself is `Arc`'d so `Clone` is cheap; the abort
    /// fires from the inner `Drop`.
    _cleanup_guard: Arc<CleanupGuard>,
}

/// RAII guard that aborts the background cleanup task when dropped.
#[derive(Debug)]
struct CleanupGuard {
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.join.lock()
            && let Some(j) = g.take()
        {
            j.abort();
        }
    }
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::with_grace_period(DEFAULT_GRACE_PERIOD)
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a custom grace period and the system clock.
    pub fn with_grace_period(grace_period: Duration) -> Self {
        Self::build(grace_period, Arc::new(SystemClock))
    }

    /// Construct with the default grace period and a custom clock (tests).
    pub fn with_clock<C: Clock + 'static>(clock: C) -> Self {
        Self::build(DEFAULT_GRACE_PERIOD, Arc::new(clock))
    }

    /// Construct with a custom grace period and clock (tests).
    pub fn with_grace_period_and_clock<C: Clock + 'static>(
        grace_period: Duration,
        clock: C,
    ) -> Self {
        Self::build(grace_period, Arc::new(clock))
    }

    fn build(grace_period: Duration, clock: Arc<dyn Clock>) -> Self {
        let inner = Arc::new(TaskManagerInner {
            tasks: Mutex::new(HashMap::new()),
            grace_period,
            clock,
        });

        // Spawn the background cleanup loop only if a tokio runtime is
        // available. Many sync tests construct `TaskManager::new()` outside
        // any runtime context — those rely on lazy/explicit cleanup hooks
        // and don't need (and can't have) a background loop.
        let join = if tokio::runtime::Handle::try_current().is_ok() {
            let weak = Arc::downgrade(&inner);
            // Tick at grace_period / 4, clamped to a sensible range so a 1ms
            // grace doesn't busy-spin and a 7-day grace still notices Drop in
            // a reasonable window.
            let tick = grace_period
                .checked_div(4)
                .unwrap_or(Duration::from_secs(60))
                .clamp(Duration::from_millis(50), Duration::from_secs(60));
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(tick);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // First tick fires immediately; skip it so we don't run cleanup
                // before any tasks have been created.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let Some(strong) = weak.upgrade() else {
                        // TaskManager has been dropped — exit the loop.
                        break;
                    };
                    cleanup_expired(&strong);
                    drop(strong);
                }
            }))
        } else {
            None
        };

        TaskManager {
            inner,
            _cleanup_guard: Arc::new(CleanupGuard {
                join: Mutex::new(join),
            }),
        }
    }

    /// Manually trigger cleanup of expired terminal records. Primarily used by
    /// tests that drive a `TestClock`; production code relies on the background
    /// loop spawned in [`Self::build`].
    pub fn cleanup_expired_now(&self) {
        cleanup_expired(&self.inner);
    }

    /// Create a new task and transition it to `Running`.
    ///
    /// In the full implementation this spawns the agent loop. The stub transitions
    /// immediately to `Running` to model the expected behavior.
    pub fn create_task(
        &self,
        tenant: &TenantConfig,
        task: impl Into<String>,
        agent_type: Option<String>,
        metadata: Value,
        connection_id: Option<String>,
    ) -> Result<TaskHandle, TaskManagerError> {
        let task_id = Uuid::new_v4().to_string();
        let description = task.into();
        let resolved_agent_type = agent_type.unwrap_or_else(|| tenant.agent_type.clone());

        // Build metadata including budget_pool from tenant config.
        let mut full_metadata = metadata.clone();
        if let Some(obj) = full_metadata.as_object_mut() {
            obj.insert(
                "budget_pool".to_string(),
                serde_json::to_value(&tenant.budget_pool).unwrap_or_default(),
            );
        }

        // Per-task event channel: broadcast (64 event buffer) + history log
        // for late SSE subscribers.
        let event_tx = TaskEventChannel::new(64);

        // Start in Pending, then immediately transition to Running.
        let handle = TaskHandle {
            task_id: task_id.clone(),
            state: TaskState::Pending,
            metadata: full_metadata,
            description: description.clone(),
            agent_type: resolved_agent_type.clone(),
            tenant: tenant.namespace.clone(),
            seq: 0,
            connection_id: connection_id.clone(),
        };

        info!(
            task_id = %task_id,
            tenant = %tenant.namespace,
            agent_type = %resolved_agent_type,
            "task created"
        );

        let mut record = TaskRecord {
            handle: handle.clone(),
            pending_input: None,
            pending_approval: None,
            event_tx: event_tx.clone(),
            cancellation_token: None,
            running_since: None,
            lifecycle_span: None,
            terminated_at: None,
        };

        // Immediately transition to Running.
        record.handle.state = TaskState::Running;
        // Track for active_tasks gauge and task_duration histogram.
        record.running_since = Some(Instant::now());
        ServerMeters::get().add_active_tasks(&record.handle.tenant, 1);
        // Open the simulacra_server_task lifecycle span.
        record.lifecycle_span = Some(tracing::info_span!(
            "simulacra_server_task",
            "simulacra.server.task_id" = record.handle.task_id.as_str(),
            "simulacra.server.agent_type" = record.handle.agent_type.as_str(),
            "simulacra.server.tenant" = record.handle.tenant.as_str(),
        ));
        record.handle.seq = 1;
        event_tx.send(serde_json::json!({
            "event": "task.state_changed",
            "task_id": task_id,
            "from": "pending",
            "to": "running",
            "seq": 1,
        }));

        let running_handle = record.handle.clone();

        self.inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?
            .insert(task_id, record);

        Ok(running_handle)
    }

    /// Create a new task in `Pending` state without auto-transitioning to `Running`.
    ///
    /// Used by the worker pool model: tasks queue as `Pending` and transition to
    /// `Running` when a worker picks them up via `start_task`.
    pub fn create_pending_task(
        &self,
        tenant: &TenantConfig,
        task: impl Into<String>,
        agent_type: Option<String>,
        metadata: Value,
        connection_id: Option<String>,
    ) -> Result<TaskHandle, TaskManagerError> {
        let task_id = Uuid::new_v4().to_string();
        let description = task.into();
        let resolved_agent_type = agent_type.unwrap_or_else(|| tenant.agent_type.clone());

        // Build metadata including budget_pool from tenant config.
        let mut full_metadata = metadata.clone();
        if let Some(obj) = full_metadata.as_object_mut() {
            obj.insert(
                "budget_pool".to_string(),
                serde_json::to_value(&tenant.budget_pool).unwrap_or_default(),
            );
        }

        // Per-task event channel: broadcast (64 event buffer) + history log
        // for late SSE subscribers.
        let event_tx = TaskEventChannel::new(64);

        let handle = TaskHandle {
            task_id: task_id.clone(),
            state: TaskState::Pending,
            metadata: full_metadata,
            description: description.clone(),
            agent_type: resolved_agent_type.clone(),
            tenant: tenant.namespace.clone(),
            seq: 0,
            connection_id: connection_id.clone(),
        };

        info!(
            task_id = %task_id,
            tenant = %tenant.namespace,
            agent_type = %resolved_agent_type,
            "task created (pending)"
        );

        let record = TaskRecord {
            handle: handle.clone(),
            pending_input: None,
            pending_approval: None,
            event_tx,
            cancellation_token: None,
            running_since: None,
            lifecycle_span: None,
            terminated_at: None,
        };

        self.inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?
            .insert(task_id, record);

        Ok(handle)
    }

    /// Transition a task from `Pending` to `Running`.
    ///
    /// Called by a worker thread when it picks up the work item.
    /// Emits a `task.state_changed` event on the per-task broadcast channel.
    pub fn start_task(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        self.transition(task_id, |state| match state {
            TaskState::Pending => Ok(TaskState::Running),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::Running,
            }),
        })
    }

    /// Cancel a task. Signals the cancellation token (if stored) and transitions
    /// to `Cancelled`.
    ///
    /// Returns `TaskManagerError::TerminalState` if the task is already terminal.
    pub fn cancel_task(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        // Signal the cancellation token before transitioning state.
        {
            let tasks = self
                .inner
                .tasks
                .lock()
                .map_err(|_| TaskManagerError::LockPoisoned)?;
            if let Some(record) = tasks.get(task_id)
                && let Some(token) = &record.cancellation_token
            {
                token.cancel();
            }
        }
        self.transition(task_id, |state| {
            if state.is_terminal() {
                Err(TaskManagerError::TerminalState {
                    task_id: task_id.to_string(),
                    state: state.clone(),
                })
            } else {
                Ok(TaskState::Cancelled)
            }
        })
    }

    /// Pause a running task. Transitions to `Paused`.
    pub fn pause_task(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        self.transition(task_id, |state| match state {
            TaskState::Running | TaskState::Streaming => Ok(TaskState::Paused),
            _ if state.is_terminal() => Err(TaskManagerError::TerminalState {
                task_id: task_id.to_string(),
                state: state.clone(),
            }),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::Paused,
            }),
        })
    }

    /// Resume a paused task. Transitions to `Running`.
    pub fn resume_task(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        self.transition(task_id, |state| match state {
            TaskState::Paused => Ok(TaskState::Running),
            _ if state.is_terminal() => Err(TaskManagerError::TerminalState {
                task_id: task_id.to_string(),
                state: state.clone(),
            }),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::Running,
            }),
        })
    }

    /// Provide input for a `waiting_input` task.
    ///
    /// **Not yet implemented:** resuming an agent with human input requires
    /// the engine to pause the agent loop on the `input.required` event and
    /// hand the response back to the waiting turn. That mechanism does not
    /// exist yet; rather than silently flip the task back to `Running` with
    /// no worker (and lose the input), this returns
    /// [`TaskManagerError::NotImplemented`]. Terminal/invalid-state errors
    /// are still preferred over NotImplemented so clients see the expected
    /// failure shape.
    ///
    /// TODO(S031-HITL): wire real pause/resume through the engine.
    pub fn provide_input(
        &self,
        task_id: &str,
        _content: &str,
    ) -> Result<TaskState, TaskManagerError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        let record = tasks
            .get(task_id)
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })?;
        match &record.handle.state {
            state if state.is_terminal() => Err(TaskManagerError::TerminalState {
                task_id: task_id.to_string(),
                state: state.clone(),
            }),
            TaskState::WaitingInput => Err(TaskManagerError::NotImplemented {
                op: "provide_input",
            }),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::Running,
            }),
        }
    }

    /// Respond to a tool approval request.
    ///
    /// **Not yet implemented:** like [`provide_input`], resuming an agent
    /// after an approval requires engine-level pause/resume. Returns
    /// [`TaskManagerError::NotImplemented`] when the task is actually in
    /// `WaitingApproval`; terminal/invalid-state errors still take
    /// precedence so clients see familiar error shapes. A deny response
    /// additionally returns [`TaskManagerError::ApprovalDenied`] after the
    /// NotImplemented check so denial is still observable.
    ///
    /// TODO(S031-HITL): wire real pause/resume through the engine.
    pub fn respond_approval(
        &self,
        task_id: &str,
        tool_call_id: &str,
        _approved: bool,
        reason: Option<&str>,
    ) -> Result<TaskState, TaskManagerError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        let record = tasks
            .get(task_id)
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })?;
        let state = record.handle.state.clone();
        drop(tasks);
        match state {
            s if s.is_terminal() => Err(TaskManagerError::TerminalState {
                task_id: task_id.to_string(),
                state: s,
            }),
            TaskState::WaitingApproval => {
                if !_approved {
                    return Err(TaskManagerError::ApprovalDenied {
                        tool_call_id: tool_call_id.to_string(),
                        reason: reason.unwrap_or("denied by user").to_string(),
                    });
                }
                Err(TaskManagerError::NotImplemented {
                    op: "respond_approval",
                })
            }
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other,
                to: TaskState::Running,
            }),
        }
    }

    /// Get a task handle by ID.
    pub fn get_task(&self, task_id: &str) -> Result<TaskHandle, TaskManagerError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        tasks
            .get(task_id)
            .map(|r| r.handle.clone())
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })
    }

    /// Subscribe to per-task events. Returns a snapshot of all events emitted
    /// so far (history) plus a receiver for future events. The two together
    /// give a late subscriber a complete view of the task's event stream.
    pub fn subscribe_task(
        &self,
        task_id: &str,
    ) -> Result<(Vec<Value>, broadcast::Receiver<Value>), TaskManagerError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        tasks
            .get(task_id)
            .map(|r| r.event_tx.subscribe_with_history())
            .ok_or_else(|| TaskManagerError::SubscribeFailed {
                task_id: task_id.to_string(),
            })
    }

    /// Transition a task using a validator function. Used internally.
    fn transition(
        &self,
        task_id: &str,
        validator: impl Fn(&TaskState) -> Result<TaskState, TaskManagerError>,
    ) -> Result<TaskState, TaskManagerError> {
        self.transition_with(task_id, |record| {
            let next = validator(&record.handle.state)?;
            record.handle.state = next.clone();
            Ok(next)
        })
    }

    fn transition_with<F>(&self, task_id: &str, f: F) -> Result<TaskState, TaskManagerError>
    where
        F: FnOnce(&mut TaskRecord) -> Result<TaskState, TaskManagerError>,
    {
        self.transition_with_reason(task_id, None, f)
    }

    fn transition_with_reason<F>(
        &self,
        task_id: &str,
        reason: Option<String>,
        f: F,
    ) -> Result<TaskState, TaskManagerError>
    where
        F: FnOnce(&mut TaskRecord) -> Result<TaskState, TaskManagerError>,
    {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        let record = tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })?;
        let prev_state = record.handle.state.clone();
        let result = f(record);
        if let Ok(ref next_state) = result {
            // Increment the monotonic sequence number on every successful transition.
            record.handle.seq += 1;
            // Emit state_changed event on the per-task channel.
            let seq = record.handle.seq;
            let mut event = serde_json::json!({
                "event": "task.state_changed",
                "task_id": task_id,
                "from": prev_state.to_string(),
                "to": next_state.to_string(),
                "seq": seq,
            });
            if let Some(ref reason) = reason {
                event["reason"] = serde_json::Value::from(reason.clone());
            }
            record.event_tx.send(event);

            // ── Observability ────────────────────────────────────────────────
            let meters = ServerMeters::get();
            let tenant = record.handle.tenant.clone();

            // Entering Running for the first time: increment gauge, start timing, open span.
            if *next_state == TaskState::Running && record.running_since.is_none() {
                record.running_since = Some(Instant::now());
                meters.add_active_tasks(&tenant, 1);
                // Open the simulacra_server_task lifecycle span; it is closed (exported) when
                // the task reaches a terminal state below.
                record.lifecycle_span = Some(tracing::info_span!(
                    "simulacra_server_task",
                    "simulacra.server.task_id" = record.handle.task_id.as_str(),
                    "simulacra.server.agent_type" = record.handle.agent_type.as_str(),
                    "simulacra.server.tenant" = record.handle.tenant.as_str(),
                ));
            }

            // Entering a terminal state: decrement gauge, record duration, close lifecycle span,
            // and stamp `terminated_at` so the grace-period cleanup loop can evict the record
            // once its window elapses.
            if next_state.is_terminal() {
                if let Some(since) = record.running_since {
                    let elapsed = since.elapsed().as_secs_f64();
                    meters.add_active_tasks(&tenant, -1);
                    meters.task_duration.record(
                        elapsed,
                        &[
                            KeyValue::new("tenant", tenant),
                            KeyValue::new("agent_type", record.handle.agent_type.clone()),
                            KeyValue::new("terminal_state", next_state.to_string()),
                        ],
                    );
                    // Drop the lifecycle span — this closes and exports it with the full duration.
                    drop(record.lifecycle_span.take());
                }
                // Stamp termination time using the manager's clock (real or test).
                if record.terminated_at.is_none() {
                    record.terminated_at = Some(self.inner.clock.now());
                }
            }
        }
        result
    }

    /// Mark a task as terminal (for internal use by agent loop).
    ///
    /// The `reason` is included in the `task.state_changed` event JSON.
    /// `WaitingApproval` is also accepted as a non-terminal transition.
    pub fn complete_task(
        &self,
        task_id: &str,
        terminal_state: TaskState,
        reason: Option<String>,
    ) -> Result<(), TaskManagerError> {
        // WaitingApproval is not terminal but is a valid target from complete_task.
        if !terminal_state.is_terminal() && terminal_state != TaskState::WaitingApproval {
            return Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: TaskState::Running,
                to: terminal_state,
            });
        }
        self.transition_with_reason(task_id, reason, |record| {
            if record.handle.state.is_terminal() {
                return Err(TaskManagerError::TerminalState {
                    task_id: task_id.to_string(),
                    state: record.handle.state.clone(),
                });
            }
            record.handle.state = terminal_state.clone();
            Ok(terminal_state.clone())
        })?;
        Ok(())
    }

    /// Transition a task to `waiting_input` state.
    pub fn request_input(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        self.transition(task_id, |state| match state {
            TaskState::Running | TaskState::Streaming => Ok(TaskState::WaitingInput),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::WaitingInput,
            }),
        })
    }

    /// Transition a task to `waiting_approval` state.
    pub fn request_approval(&self, task_id: &str) -> Result<TaskState, TaskManagerError> {
        self.transition(task_id, |state| match state {
            TaskState::Running | TaskState::Streaming => Ok(TaskState::WaitingApproval),
            other => Err(TaskManagerError::InvalidTransition {
                task_id: task_id.to_string(),
                from: other.clone(),
                to: TaskState::WaitingApproval,
            }),
        })
    }

    /// Push an event onto a task's broadcast channel.
    ///
    /// Increments the task's monotonic seq counter. Returns the assigned seq.
    pub fn emit_event(&self, task_id: &str, mut event: Value) -> Result<u64, TaskManagerError> {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        let record = tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })?;
        record.handle.seq += 1;
        let seq = record.handle.seq;

        // Emit events_emitted counter before injecting seq (preserves original event_type).
        let event_type = event
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        ServerMeters::get().events_emitted.add(
            1,
            &[
                KeyValue::new("event_type", event_type),
                KeyValue::new("tenant", record.handle.tenant.clone()),
            ],
        );

        event["seq"] = serde_json::Value::from(seq);
        record.event_tx.send(event);
        Ok(seq)
    }

    /// Get the event channel for a task. Used by SimulacraEngine to construct
    /// EngineActivitySink without ongoing lock acquisition. The returned
    /// `TaskEventChannel` is cheaply cloneable (Arc'd internally) and routes
    /// every send through the history log.
    pub fn get_event_sender(&self, task_id: &str) -> Result<TaskEventChannel, TaskManagerError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        tasks
            .get(task_id)
            .map(|r| r.event_tx.clone())
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })
    }

    /// Store a cancellation token for a task (called by SimulacraEngine after spawn).
    pub fn set_cancellation_token(
        &self,
        task_id: &str,
        token: CancellationToken,
    ) -> Result<(), TaskManagerError> {
        let mut tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)?;
        let record = tasks
            .get_mut(task_id)
            .ok_or_else(|| TaskManagerError::NotFound {
                task_id: task_id.to_string(),
            })?;
        record.cancellation_token = Some(token);
        Ok(())
    }

    /// List all active (non-terminal) task IDs.
    pub fn active_task_ids(&self) -> Vec<String> {
        let tasks = self.inner.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .values()
            .filter(|r| !r.handle.state.is_terminal())
            .map(|r| r.handle.task_id.clone())
            .collect()
    }

    /// Cancel all tasks belonging to a specific WebSocket connection.
    /// Returns the IDs of cancelled tasks.
    pub fn cancel_connection_tasks(&self, conn_id: &str) -> Vec<String> {
        let task_ids: Vec<String> = {
            let tasks = self.inner.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks
                .values()
                .filter(|r| {
                    !r.handle.state.is_terminal()
                        && r.handle.connection_id.as_deref() == Some(conn_id)
                })
                .map(|r| r.handle.task_id.clone())
                .collect()
        };
        for id in &task_ids {
            let _ = self.cancel_task(id);
        }
        task_ids
    }

    /// Cancel all active tasks on this server (legacy — prefer `cancel_connection_tasks`).
    pub fn cancel_all_active(&self) -> Vec<String> {
        let task_ids: Vec<String> = self.active_task_ids();
        for id in &task_ids {
            let _ = self.cancel_task(id);
        }
        task_ids
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Grace-period cleanup
// ──────────────────────────────────────────────────────────────────────────────

/// Evict every terminal task whose `terminated_at` is older than
/// `grace_period`. Non-terminal tasks are left untouched regardless of how
/// long they have existed (a 30-day `Running` task is still live and must
/// continue to receive events).
///
/// Lock discipline: acquires `inner.tasks` exactly once and releases it before
/// returning. No await points are crossed while holding the lock — callers can
/// be sync or async without risk of deadlock.
fn cleanup_expired(inner: &TaskManagerInner) {
    let now = inner.clock.now();
    let grace = inner.grace_period;
    let mut guard = match inner.tasks.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            warn!("task_manager.cleanup: lock poisoned, recovering");
            poisoned.into_inner()
        }
    };
    let to_remove: Vec<String> = guard
        .iter()
        .filter_map(|(id, rec)| {
            // Defensive: only consider records that are BOTH in a terminal
            // state AND have a `terminated_at` stamp. A non-terminal task —
            // including Pending/Running/Streaming/WaitingInput/WaitingApproval/Paused —
            // is never evicted.
            let terminated_at = rec.terminated_at?;
            if !rec.handle.state.is_terminal() {
                return None;
            }
            if now.saturating_duration_since(terminated_at) >= grace {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    for id in &to_remove {
        guard.remove(id);
    }
    if !to_remove.is_empty() {
        info!(
            evicted = to_remove.len(),
            "task_manager.cleanup: evicted expired terminal tasks"
        );
    }
}
