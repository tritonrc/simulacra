# S035 — Agent Worker Pool

**Status:** Active
**Crates:** `simulacra-server` (primary), `simulacra-config` (WorkerPoolConfig)

## Dependencies

- **S034** — SimulacraEngine (spawn_task, agent construction sequence, EngineActivitySink)
- **S031** — API server (TaskManager, task lifecycle, pending/running states)
- **S033** — Integration fabric (IntegrationRegistry, tenant-scoped integration grants)

## Scope

A bounded thread pool for agent execution that replaces `spawn_blocking`. Fixes the unbounded OS thread consumption problem and two security issues from the S034 review.

**In scope:**
- `AgentWorkerPool` struct: fixed N worker threads, each building a `current_thread` tokio runtime per work item
- Bounded task queue (`crossbeam::channel::bounded`) with generous default capacity
- Tasks queue in `pending` state; transition to `running` when a worker picks them up
- 503 only on true sustained overload (queue full at configured max, default 1000)
- `SimulacraEngine::spawn_task` submits to the pool instead of `spawn_blocking`
- `WorkerPoolConfig` in `ServerConfig` (count, queue_capacity)
- Graceful shutdown with explicit drain-then-close semantics
- Worker panic recovery (respawn replacement worker)
- Pool-level OTel metrics (active workers, queue depth, queue wait time)
- **Fix: tenant-scoped integration grants** — `tenant_integrations` uses tenant config, not `reg.names()`
- **Fix: task_status ownership check** — use `resolve_and_check_ownership` like other endpoints
- **Fix: `integrations` field on `simulacra-server::TenantConfig`**

**Out of scope:**
- Dynamic pool resizing at runtime
- Priority queues / task scheduling policies
- Per-tenant worker quotas (future enhancement)
- Work-stealing between workers

## Context

`SimulacraEngine::spawn_task` currently uses `tokio::task::spawn_blocking` with a `current_thread` runtime per agent. This was necessary because QuickJS (rquickjs) is `!Send` — the `SendableJsRuntime` uses `unsafe impl Send` with a `Mutex`, but the runtime cannot safely migrate between tokio worker threads across await points.

The problem: each agent permanently occupies one thread from tokio's blocking pool (default 512) for its entire lifetime — potentially minutes or hours. There is no backpressure, no configurable limit, and no visibility. With 50 concurrent long-running agents, 50 OS threads are pinned.

The solution: a fixed pool of N persistent worker threads. Agents queue via a bounded channel. Workers loop: receive task → build `current_thread` runtime → run agent → drop runtime and all resources → receive next. Bounded, observable, recoverable.

## Design

### Queue-first model

`spawn_task` always succeeds (unless the queue is at max capacity). The task enters `pending` state immediately. The client gets a `task_id` back and can:
- Poll `GET /api/v1/tasks/{task_id}/status` to watch `pending → running → completed`
- Open an SSE stream at `/api/v1/tasks/{task_id}/events` which streams events once the agent starts
- Cancel with `POST /api/v1/tasks/{task_id}/cancel` if they don't want to wait
- Disconnect the SSE stream if they lose patience (task continues in background)

503 is reserved for genuine sustained overload — the queue at configured max (default 1000). For normal bursts, the queue absorbs the spike. This is the standard job-queue pattern (Sidekiq, Celery, Bull).

### AgentWorkerPool

```rust
pub struct AgentWorkerPool {
    sender: crossbeam::channel::Sender<WorkItem>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    receiver: crossbeam::channel::Receiver<WorkItem>,  // held for respawning
    config: WorkerPoolConfig,
    shutdown: AtomicBool,
}

type WorkItem = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerPoolConfig {
    /// Number of worker threads. Default: min(num_cpus, 8).
    #[serde(default = "default_worker_count")]
    pub count: usize,
    /// Bounded queue capacity. Default: 1000.
    /// 503 only when this many tasks are already queued.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
}

fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}

fn default_queue_capacity() -> usize { 1000 }
```

### Worker thread lifecycle

Each worker thread runs:
```
loop {
    let work_item = receiver.recv();
    if recv fails (sender dropped + queue empty) → exit loop (shutdown)

    active_workers.increment();
    
    // Each work item gets its own current_thread runtime.
    // This ensures clean resource lifecycle: the runtime, VFS, AgentCell,
    // ToolRegistry, and JsRuntime are all created and dropped within one
    // work item. No state leaks between agents on the same worker.
    work_item();
    
    active_workers.decrement();
}
```

### Worker panic recovery

If a worker panics, the pool detects it (via `JoinHandle::is_finished()`) and spawns a replacement:

```rust
impl AgentWorkerPool {
    /// Check for crashed workers and respawn replacements.
    /// Called periodically (e.g., on each submit()) or via a background monitor.
    fn check_and_respawn(&self) {
        let mut workers = self.workers.lock().unwrap();
        for i in 0..workers.len() {
            if workers[i].is_finished() {
                tracing::error!("worker thread {} panicked — respawning", i);
                workers[i] = spawn_worker(i, self.receiver.clone());
            }
        }
    }
}
```

This ensures the pool maintains N workers even after panics.

### Submission

```rust
impl AgentWorkerPool {
    pub fn submit(&self, work: WorkItem) -> Result<(), EngineError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(EngineError::PoolShutdown);
        }
        self.check_and_respawn();
        self.sender.try_send(work).map_err(|_| EngineError::PoolExhausted)
    }
}
```

`try_send` is non-blocking. Returns `PoolExhausted` only when the queue is at `queue_capacity` (default 1000). Normal operation: always succeeds, task queues.

### Completion notification

The work closure sends its result back via `tokio::sync::oneshot`:

```rust
let (tx, rx) = tokio::sync::oneshot::channel();
pool.submit(Box::new(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("agent runtime");
    let result = rt.block_on(async move {
        // Full agent construction + execution
        agent_loop.run(&description).await
    });
    let _ = tx.send(result);
}))?;

// Completion handler on main runtime
tokio::spawn(async move {
    match rx.await {
        Ok(Ok(output)) => { /* map ExitReason → TaskState */ }
        Ok(Err(e)) => { /* Failed */ }
        Err(_) => { /* worker panicked, oneshot dropped → Failed */ }
    }
});
```

### Graceful shutdown

```rust
impl AgentWorkerPool {
    pub fn shutdown(&self) {
        // 1. Set shutdown flag (submit rejects new work)
        self.shutdown.store(true, Ordering::SeqCst);
        
        // 2. Workers drain remaining queued items to completion
        //    (crossbeam recv continues until sender dropped AND queue empty)
        
        // 3. Drop sender — workers will exit after draining
        // (sender is dropped when AgentWorkerPool is dropped)
    }
}

impl Drop for AgentWorkerPool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Sender drops automatically (struct field)
        // Workers drain queue, then exit recv loop, then join
        let workers = self.workers.get_mut().unwrap();
        for handle in workers.drain(..) {
            let _ = handle.join();
        }
    }
}
```

Shutdown semantics:
1. Set `shutdown` flag → `submit()` rejects new work immediately
2. Workers continue draining already-queued items to completion
3. When `AgentWorkerPool` is dropped, sender drops, workers exit after queue is empty
4. `Drop` joins all worker threads

This is **drain** behavior, not cancel. Queued tasks run to completion. Callers who want to cancel pending tasks should call `task.cancel` on each one.

### Configuration

```toml
[server]
host = "0.0.0.0"
port = 8080

[server.workers]
count = 8            # worker threads (default: min(num_cpus, 8))
queue_capacity = 1000  # max queued tasks before 503 (default: 1000)
```

### Pending → Running transition

Today, `create_task` transitions immediately to `Running`. With the worker pool, the lifecycle changes:

1. `spawn_task` creates the task in `Pending` state via `TaskManager`
2. `spawn_task` submits the work item to the pool queue
3. Returns the `TaskHandle` with `state: pending` to the client
4. When a worker picks up the item, it transitions to `Running` and emits `task.state_changed`
5. The client sees `pending → running` either via status polling or SSE stream

This requires `TaskManager::create_task` to be split:
- `create_pending_task(...)` → creates in `Pending`, does NOT auto-transition to `Running`
- The work item calls `task_manager.start_task(task_id)` → transitions `Pending → Running`

### Integration with SimulacraEngine

```rust
pub struct SimulacraEngine {
    config: SimulacraConfig,
    integration_registry: Option<Arc<IntegrationRegistry>>,
    pool: Arc<AgentWorkerPool>,
}

impl SimulacraEngine {
    pub fn new(
        config: SimulacraConfig,
        integration_registry: Option<Arc<IntegrationRegistry>>,
        pool_config: WorkerPoolConfig,
    ) -> Result<Self, EngineError>;

    pub async fn spawn_task(
        &self,
        task_manager: &TaskManager,
        description: &str,
        tenant: &TenantConfig,
        agent_type_override: Option<&str>,
        metadata: Value,
        connection_id: Option<String>,
    ) -> Result<TaskHandle, EngineError> {
        // 1. Resolve agent type
        // 2. Create task in Pending state (NOT Running)
        // 3. Extract broadcast sender
        // 4. Submit work item to pool
        //    Work item: build runtime → construct agent → start_task() → run → complete
        // 5. Return TaskHandle (state: pending)
    }
}
```

### Tenant-scoped integration grants (blocker fix)

In `spawn_task`, change:
```rust
// BEFORE (broken — grants all integrations):
cell.tenant_integrations = reg.names();

// AFTER (tenant-scoped):
cell.tenant_integrations = tenant.integrations.clone();
```

Add `pub integrations: Vec<String>` to `simulacra-server::TenantConfig` (with `#[serde(default)]`).

If `tenant.integrations` is empty and this is a single-tenant deployment (CLI mode), fall back to `reg.names()` for backwards compatibility. The heuristic: if `SimulacraConfig.tenants` is empty (no explicit tenants configured), the engine is in CLI/single-tenant mode and all integrations are granted.

### task_status ownership check (blocker fix)

```rust
// BEFORE (IDOR):
let credentials = extract_credentials(&headers);
state.auth.authenticate(&credentials).await?;
state.task_manager.get_task(&task_id)?;

// AFTER:
let (_, handle) = resolve_and_check_ownership(&state, &headers, &task_id).await?;
```

`resolve_and_check_ownership` already does: authenticate → resolve tenant → get task → verify `task.tenant == resolved_tenant.namespace`. Works for reads and mutations.

## Behavior

### Pool lifecycle
1. `AgentWorkerPool::new(config)` spawns `config.count` OS threads, each running a recv loop.
2. Each worker thread is named `simulacra-agent-worker-{N}` for debugging.
3. Workers block on `crossbeam::channel::Receiver::recv()` waiting for work items.
4. Drop of `AgentWorkerPool` sets shutdown flag, drops sender, joins workers after drain.
5. Workers drain remaining queued items before exiting (drain semantics, not cancel).

### Task submission
6. `submit(work)` checks shutdown flag, then calls `sender.try_send(work)`.
7. If shutdown is set, returns `EngineError::PoolShutdown`.
8. If the channel is full (at `queue_capacity`), returns `EngineError::PoolExhausted`.
9. The HTTP handler maps `PoolExhausted` to HTTP 503 Service Unavailable.
10. If the channel has capacity, the work item is queued and `submit` returns immediately.
11. `submit` calls `check_and_respawn` to recover any panicked workers before sending.

### Pending → Running
12. `spawn_task` creates the task in `Pending` state.
13. The HTTP response returns `state: "pending"` with the task_id.
14. When a worker picks up the work item, it calls `task_manager.start_task(task_id)` which transitions `Pending → Running`.
15. The `pending → running` transition emits a `task.state_changed` event on the broadcast channel.
16. SSE subscribers and status pollers see the transition.

### Agent execution on workers
17. Worker receives a work item and calls it.
18. The work item builds a fresh `current_thread` tokio runtime.
19. The agent's entire lifecycle (VFS, AgentCell, ToolRegistry, AgentLoop) is scoped to the work item.
20. When the work item returns, all agent resources are dropped, including the `current_thread` runtime.
21. The worker loops back to recv for the next item.
22. QuickJS runtime is created and destroyed within one work item — never migrates between threads.
23. The `current_thread` runtime is per-work-item, not per-worker. This guarantees clean isolation between consecutive agents on the same worker.

### Completion and failure handling
24. The work item sends its result via a `oneshot` channel.
25. A tokio task on the main runtime awaits the oneshot and maps ExitReason → TaskState.
26. If the worker panics, the oneshot sender is dropped and the receiver gets `RecvError` → `Failed`.
27. Panic in a worker does NOT crash other workers or the pool.
28. Panicked workers are automatically respawned on the next `submit()` call.

### Backpressure
29. Queue capacity defaults to 1000 (configurable).
30. 503 is returned only when queue is full — genuine sustained overload.
31. Normal burst traffic is absorbed by the queue; tasks sit in `pending` until a worker is free.
32. `simulacra.engine.pool.queue_depth` gauge reflects current queue occupancy.
33. `simulacra.engine.pool.active_workers` gauge reflects currently executing agents.

### Tenant integration scoping
34. `spawn_task` reads `tenant.integrations` for the tenant's integration grants.
35. Only integrations in the tenant's list are wired into `cell.tenant_integrations`.
36. An agent for tenant A cannot inject credentials for tenant B's integrations.
37. In single-tenant mode (no explicit tenants in config), fall back to `reg.names()`.

### task_status ownership
38. `GET /api/v1/tasks/{task_id}/status` calls `resolve_and_check_ownership`.
39. Returns 403 if the authenticated tenant does not own the task.
40. Returns 404 if the task does not exist.

## Assertions

### Pool lifecycle
- [x] `AgentWorkerPool::new` spawns exactly `config.count` OS threads.
- [x] Each worker thread is named `simulacra-agent-worker-{N}`.
- [x] Drop of pool joins all worker threads after draining queue.
- [x] After shutdown flag is set, `submit()` returns `PoolShutdown`.
- [x] Queued tasks drain to completion during shutdown (not cancelled).

### Task submission
- [x] `submit()` succeeds when queue has capacity.
- [x] `submit()` returns `PoolExhausted` when queue is at `queue_capacity`.
- [x] HTTP handler maps `PoolExhausted` to 503.
- [x] Submitted work item executes on a worker thread.
- [x] Default `queue_capacity` is 1000.

### Pending → Running
- [x] `spawn_task` returns `TaskHandle` with `state: pending`.
- [x] Worker pickup transitions task from `pending` to `running`.
- [x] `task.state_changed` event emitted on `pending → running` transition.
- [x] SSE subscriber sees the transition.

### Agent execution
- [x] Agent runs on a `current_thread` tokio runtime within the work item.
- [x] QuickJS runtime works correctly (no statSync errors from thread migration).
- [x] Agent resources are dropped after completion.
- [x] Multiple agents execute concurrently on different workers.
- [x] Worker processes tasks sequentially (one at a time per worker).
- [x] `current_thread` runtime is per-work-item, not reused across agents.

### Worker recovery
- [x] Panicked worker is detected on next `submit()`.
- [x] Replacement worker is spawned automatically.
- [x] Pool maintains `config.count` workers after panic recovery.

### Completion handling
- [x] Successful agent completion maps ExitReason to TaskState via oneshot.
- [x] Agent error maps to TaskState::Failed via oneshot.
- [x] Worker panic maps to TaskState::Failed (oneshot dropped → RecvError).

### Backpressure
- [x] With default config, queue accepts 1000 tasks before 503.
- [x] After a worker completes, queued tasks proceed.
- [x] Long-running agents (30+ minutes) block their worker but don't affect others.

### Tenant integration scoping
- [x] Agent with `tenant.integrations = ["hubspot"]` only gets hubspot credentials.
- [x] Agent with `tenant.integrations = []` gets no credential injection.
- [x] Agent for tenant A cannot inject credentials for tenant B's integrations.
- [x] Single-tenant mode falls back to all integrations.

### task_status ownership
- [x] `GET /status` returns 403 for cross-tenant task access.
- [x] `GET /status` returns 200 with correct state for owned tasks.
- [x] `GET /status` returns 404 for nonexistent tasks.

## Observability (see S010)

- [x] `simulacra.engine.pool.active_workers` gauge tracks currently executing agents.
- [x] `simulacra.engine.pool.queue_depth` gauge tracks queued waiting tasks.
- [x] `simulacra.engine.pool.queue_wait_ms` histogram records time from submit to worker pickup.
- [x] `simulacra.engine.pool.tasks_submitted` counter with `tenant` label.
- [x] `simulacra.engine.pool.tasks_rejected` counter (503 responses).
- [x] `tracing::info!` on pool startup with worker count and queue capacity.
- [x] `tracing::info!` on task pickup by worker with task_id and worker name.
- [x] `tracing::info!` on `pending → running` transition with queue wait time.
- [x] `tracing::warn!` on pool exhaustion (503).
- [x] `tracing::error!` on worker panic with thread name, respawn triggered.
