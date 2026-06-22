//! Agent worker pool — bounded thread pool for agent execution (S035).
//!
//! Replaces `spawn_blocking` with a fixed pool of N persistent worker threads.
//! Workers loop: receive task -> build `current_thread` runtime -> run agent ->
//! drop all resources -> receive next. Bounded, observable, recoverable.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel as channel;
use tracing::{error, info};

use crate::engine::EngineError;

/// A boxed closure that runs on a worker thread.
pub type WorkItem = Box<dyn FnOnce() + Send + 'static>;

/// Configuration for the agent worker pool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerPoolConfig {
    /// Number of worker threads. Default: min(num_cpus, 8).
    #[serde(default = "default_worker_count")]
    pub count: usize,
    /// Bounded queue capacity. Default: 1000.
    /// 503 only when this many tasks are already queued.
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            count: default_worker_count(),
            queue_capacity: default_queue_capacity(),
        }
    }
}

fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}

fn default_queue_capacity() -> usize {
    1000
}

/// A bounded thread pool for agent execution.
///
/// Workers block on a crossbeam bounded channel. Each work item gets its own
/// `current_thread` tokio runtime, ensuring clean resource isolation between
/// consecutive agents on the same worker.
pub struct AgentWorkerPool {
    sender: Option<channel::Sender<WorkItem>>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    receiver: channel::Receiver<WorkItem>,
    config: WorkerPoolConfig,
    shutdown: AtomicBool,
}

impl std::fmt::Debug for AgentWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentWorkerPool")
            .field("count", &self.config.count)
            .field("queue_capacity", &self.config.queue_capacity)
            .field("shutdown", &self.shutdown.load(Ordering::Relaxed))
            .finish()
    }
}

impl AgentWorkerPool {
    /// Create a new worker pool and spawn `config.count` worker threads.
    pub fn new(config: WorkerPoolConfig) -> Self {
        let (sender, receiver) = channel::bounded(config.queue_capacity);

        info!(
            worker_count = config.count,
            queue_capacity = config.queue_capacity,
            "agent worker pool starting"
        );

        let mut workers = Vec::with_capacity(config.count);
        for id in 0..config.count {
            workers.push(spawn_worker(id, receiver.clone()));
        }

        Self {
            sender: Some(sender),
            workers: Mutex::new(workers),
            receiver,
            config,
            shutdown: AtomicBool::new(false),
        }
    }

    /// Submit a work item to the pool.
    ///
    /// Returns `PoolShutdown` if the pool is shutting down.
    /// Returns `PoolExhausted` if the queue is full (at `queue_capacity`).
    pub fn submit(&self, work: WorkItem) -> Result<(), EngineError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(EngineError::PoolShutdown);
        }
        self.check_and_respawn();
        let sender = self.sender.as_ref().ok_or(EngineError::PoolShutdown)?;
        sender
            .try_send(work)
            .map_err(|_| EngineError::PoolExhausted)
    }

    /// Check for crashed workers and respawn replacements.
    ///
    /// Called on each `submit()` to maintain the worker count.
    fn check_and_respawn(&self) {
        let mut workers = self.workers.lock().unwrap();
        for i in 0..workers.len() {
            if workers[i].is_finished() {
                error!(worker_id = i, "worker thread panicked — respawning");
                workers[i] = spawn_worker(i, self.receiver.clone());
            }
        }
    }

    /// Returns the current worker count from config.
    pub fn worker_count(&self) -> usize {
        self.config.count
    }

    /// Returns the queue capacity.
    pub fn queue_capacity(&self) -> usize {
        self.config.queue_capacity
    }

    /// Returns true if the shutdown flag has been set.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

impl Drop for AgentWorkerPool {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Drop the sender — workers will drain remaining items then exit.
        self.sender.take();
        // Join all worker threads.
        let workers = self.workers.get_mut().unwrap();
        for handle in workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Spawn a single worker thread that loops over the receiver.
fn spawn_worker(id: usize, receiver: channel::Receiver<WorkItem>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("simulacra-agent-worker-{id}"))
        .spawn(move || {
            while let Ok(work) = receiver.recv() {
                // Execute the work item. If it panics, std::thread catches it
                // and this worker's JoinHandle will report is_finished + panic.
                // We use catch_unwind to keep the worker alive.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                if let Err(panic_info) = result {
                    error!(
                        worker_id = id,
                        "work item panicked: {:?}",
                        panic_info
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic_info.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic")
                    );
                }
            }
            // Sender dropped + queue empty -> exit.
        })
        .expect("failed to spawn worker thread")
}
