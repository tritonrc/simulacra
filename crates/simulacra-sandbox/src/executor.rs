//! Bounded executor for script runtimes (JS, Python, WASM).
//!
//! Prevents script execution from exhausting tokio's blocking thread pool
//! by limiting concurrency with a semaphore. Scripts that exceed the
//! concurrency limit wait for a permit (backpressure).

use std::sync::Arc;

use opentelemetry::metrics::Histogram;
use tokio::sync::Semaphore;

/// Lazily-initialized OTel meters for the script executor.
struct ExecutorMeters {
    queue_wait: Histogram<f64>,
}

impl ExecutorMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<ExecutorMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-sandbox");
            ExecutorMeters {
                queue_wait: meter
                    .f64_histogram("simulacra.script.queue_wait_ms")
                    .with_unit("ms")
                    .with_description("Time scripts wait for a concurrency permit before execution")
                    .build(),
            }
        })
    }
}

/// Bounded executor for script runtimes.
///
/// Uses a [`Semaphore`] to limit the number of concurrent script executions,
/// preventing script workloads from saturating tokio's blocking thread pool.
///
/// Two execution modes:
/// - [`execute`](Self::execute): Acquires a permit, then runs the closure on
///   `tokio::task::spawn_blocking`. Use for `Send` runtimes (Python, WASM).
/// - [`acquire_permit`](Self::acquire_permit): Returns a permit guard for
///   runtimes that must run on the current thread (`!Send` JS runtime).
///   The caller holds the permit while executing inline.
#[derive(Clone)]
pub struct ScriptExecutor {
    semaphore: Arc<Semaphore>,
}

impl ScriptExecutor {
    /// Create a new executor with the given concurrency limit.
    ///
    /// `max_concurrent` controls how many scripts can run simultaneously
    /// across all runtimes (JS, Python, WASM). Default recommendation: 4.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Execute a blocking closure on tokio's blocking thread pool,
    /// bounded by this executor's concurrency limit.
    ///
    /// Waits for a permit before spawning. The permit is held for the
    /// duration of the closure.
    pub async fn execute<F, T>(&self, f: F) -> Result<T, ScriptExecutorError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let start = std::time::Instant::now();
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| ScriptExecutorError::SemaphoreClosed)?;

        let wait_ms = start.elapsed().as_secs_f64() * 1000.0;
        let meters = ExecutorMeters::get();
        meters.queue_wait.record(wait_ms, &[]);

        if wait_ms > 1.0 {
            tracing::info!(
                simulacra.script.queue_wait_ms = wait_ms,
                "script waited for executor permit"
            );
        }

        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit; // hold permit until closure completes
            f()
        })
        .await
        .map_err(|e| ScriptExecutorError::JoinError(e.to_string()))?;

        Ok(result)
    }

    /// Acquire a concurrency permit without spawning a blocking task.
    ///
    /// Use this for `!Send` runtimes (like QuickJS) that must execute on
    /// the current thread. The returned guard holds the permit; drop it
    /// when execution is complete.
    pub async fn acquire_permit(&self) -> Result<ScriptPermit<'_>, ScriptExecutorError> {
        let start = std::time::Instant::now();
        let permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| ScriptExecutorError::SemaphoreClosed)?;

        let wait_ms = start.elapsed().as_secs_f64() * 1000.0;
        let meters = ExecutorMeters::get();
        meters.queue_wait.record(wait_ms, &[]);

        if wait_ms > 1.0 {
            tracing::info!(
                simulacra.script.queue_wait_ms = wait_ms,
                "script waited for executor permit"
            );
        }

        Ok(ScriptPermit { _permit: permit })
    }

    /// Try to acquire a concurrency permit synchronously.
    ///
    /// This is used by synchronous entry points such as `AgentCell::execute_js`
    /// where awaiting for backpressure is not possible.
    pub fn try_acquire_permit(&self) -> Result<ScriptPermit<'_>, ScriptExecutorError> {
        let permit = self
            .semaphore
            .try_acquire()
            .map_err(|_| ScriptExecutorError::PermitUnavailable)?;
        Ok(ScriptPermit { _permit: permit })
    }

    /// Return the number of available permits.
    #[cfg(test)]
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// RAII guard holding a script execution permit.
///
/// The permit is released when this guard is dropped.
pub struct ScriptPermit<'a> {
    _permit: tokio::sync::SemaphorePermit<'a>,
}

/// Errors from the script executor.
#[derive(Debug, thiserror::Error)]
pub enum ScriptExecutorError {
    #[error("script executor semaphore closed")]
    SemaphoreClosed,
    #[error("script executor permit unavailable")]
    PermitUnavailable,
    #[error("spawn_blocking failed: {0}")]
    JoinError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_runs_closure_on_blocking_pool() {
        let executor = ScriptExecutor::new(4);
        let result = executor.execute(|| 2 + 2).await.unwrap();
        assert_eq!(result, 4);
    }

    #[tokio::test]
    async fn execute_returns_closure_result() {
        let executor = ScriptExecutor::new(4);
        let result = executor.execute(|| "hello".to_string()).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn acquire_permit_limits_concurrency() {
        let executor = ScriptExecutor::new(2);
        assert_eq!(executor.available_permits(), 2);

        let _p1 = executor.acquire_permit().await.unwrap();
        assert_eq!(executor.available_permits(), 1);

        let _p2 = executor.acquire_permit().await.unwrap();
        assert_eq!(executor.available_permits(), 0);

        // Dropping a permit frees it
        drop(_p1);
        assert_eq!(executor.available_permits(), 1);
    }

    #[tokio::test]
    async fn concurrent_scripts_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let executor = Arc::new(ScriptExecutor::new(2));
        let running = Arc::new(AtomicUsize::new(0));
        let max_running = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..6 {
            let exec = Arc::clone(&executor);
            let r = Arc::clone(&running);
            let m = Arc::clone(&max_running);
            handles.push(tokio::spawn(async move {
                exec.execute(move || {
                    let current = r.fetch_add(1, Ordering::SeqCst) + 1;
                    // Update max
                    m.fetch_max(current, Ordering::SeqCst);
                    // Simulate work
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    r.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // max_running should never exceed the concurrency limit of 2
        assert!(max_running.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn scripts_dont_block_async_runtime() {
        let executor = Arc::new(ScriptExecutor::new(1));

        // Start a long-running script
        let exec = Arc::clone(&executor);
        let script_handle = tokio::spawn(async move {
            exec.execute(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                42
            })
            .await
            .unwrap()
        });

        // Give the script a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Async work should complete while script runs
        let async_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), async { 1 + 1 }).await;
        assert!(async_result.is_ok(), "async work should not be blocked");

        let script_result = script_handle.await.unwrap();
        assert_eq!(script_result, 42);
    }
}
