//! S037 §20 Retention — per-subtree TTL sweeps.
//!
//! The [`RetentionReaper`] is a process-wide service that periodically sweeps
//! expired memory content. For each registered tenant it runs its own worker
//! task so a slow-or-stuck sweep on one tenant never blocks another.
//!
//! Sweeps are paginated: `batch_size` entries deleted per iteration, releasing
//! the per-tenant write lock between batches.
//!
//! Each **per-subtree** sweep emits a `memory_reaper_sweep` span (per S037 §18)
//! with `tenant`, `subtree`, `deleted_count`, `duration_ms` attributes so
//! compliance/DLP operators can audit what was removed and why.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use simulacra_types::{MemoryPath, TenantId};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{Instrument, Span, debug, field, info_span, warn};

use crate::error::MemoryError;
use crate::index::VectorIndex;
use crate::store::MemoryStore;

/// Maximum time `shutdown` waits for any single tenant worker to finish its
/// current sweep before moving on.
const SHUTDOWN_WORKER_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for [`RetentionReaper`].
#[derive(Debug, Clone)]
pub struct RetentionReaperConfig {
    /// How often each tenant worker scans its subtrees. Default 1h.
    pub interval: Duration,
    /// Maximum entries deleted per batch before yielding the per-tenant
    /// write lock. Default 256.
    pub batch_size: u64,
    /// Retention subtrees: each `(prefix, ttl)` pair means "delete entries
    /// under `prefix` whose mtime is older than `ttl`".
    pub subtrees: Vec<RetentionSubtree>,
}

impl Default for RetentionReaperConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            batch_size: 256,
            subtrees: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetentionSubtree {
    pub prefix: MemoryPath,
    pub ttl: Duration,
}

/// Stats returned from a single tenant sweep. Returned by `sweep_now` and
/// aggregated inside the spawned worker loop.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReaperStats {
    pub paths_deleted: u64,
    pub batches: u64,
    pub subtrees_scanned: u64,
    /// Paths where `MemoryStore::delete` succeeded but `VectorIndex::delete_path`
    /// failed — these rows are gone from the store but stale chunks may linger
    /// in the index. Operators should alert on a non-zero value and run a
    /// consistency check. Distinct from `paths_deleted` which counts only
    /// fully-removed entries.
    pub index_failures: u64,
}

/// Per-tenant worker record. Shared between `register_tenant` and `shutdown`.
struct TenantWorker {
    /// Oneshot used to signal the worker loop to exit.
    shutdown_tx: oneshot::Sender<()>,
    /// Join handle for the spawned worker task.
    handle: JoinHandle<()>,
}

/// Process-wide retention reaper. Per-tenant workers are spawned on
/// [`RetentionReaper::register_tenant`] and torn down on
/// [`RetentionReaper::shutdown`].
pub struct RetentionReaper {
    config: Arc<RetentionReaperConfig>,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    workers: Arc<Mutex<HashMap<TenantId, TenantWorker>>>,
}

impl RetentionReaper {
    /// Construct a reaper. Tenants must be registered via
    /// [`register_tenant`](Self::register_tenant) before periodic sweeps begin.
    pub fn new(
        config: RetentionReaperConfig,
        store: Arc<dyn MemoryStore>,
        index: Arc<dyn VectorIndex>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            store,
            index,
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a tenant with the reaper. Idempotent — a second call for
    /// the same tenant is a no-op. Spawns a dedicated tokio task that ticks
    /// every `config.interval` and runs a sweep.
    pub fn register_tenant(&self, tenant: TenantId) {
        // The workers map is protected by a non-async std::sync::Mutex.
        // We only ever hold it across trivial insert/drain operations —
        // the worker loop itself runs outside the lock.
        let mut guard = self
            .workers
            .lock()
            .expect("retention reaper workers mutex poisoned");
        if guard.contains_key(&tenant) {
            return;
        }
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let worker_config = Arc::clone(&self.config);
        let worker_store = Arc::clone(&self.store);
        let worker_index = Arc::clone(&self.index);
        let worker_tenant = tenant.clone();
        let handle = tokio::spawn(async move {
            run_worker(
                worker_tenant,
                worker_config,
                worker_store,
                worker_index,
                shutdown_rx,
            )
            .await;
        });
        guard.insert(
            tenant,
            TenantWorker {
                shutdown_tx,
                handle,
            },
        );
    }

    /// Force an immediate sweep for a tenant, bypassing the interval timer.
    /// Returns when the sweep completes. Used primarily by tests — production
    /// callers rely on the interval loop.
    pub async fn sweep_now(&self, tenant: &TenantId) -> Result<ReaperStats, MemoryError> {
        sweep_tenant(tenant, &self.config, &self.store, &self.index).await
    }

    /// Drain all tenant workers and shut down. Signals every worker's
    /// shutdown oneshot in parallel and awaits their join handles with a
    /// per-worker timeout.
    pub async fn shutdown(self) -> Result<(), MemoryError> {
        // Drain the worker map without holding the lock across awaits.
        let drained: Vec<(TenantId, TenantWorker)> = {
            let mut guard = self
                .workers
                .lock()
                .expect("retention reaper workers mutex poisoned");
            guard.drain().collect()
        };

        // Signal all workers first so they can wind down in parallel.
        let mut pending: Vec<(TenantId, JoinHandle<()>)> = Vec::with_capacity(drained.len());
        for (tenant, worker) in drained {
            let TenantWorker {
                shutdown_tx,
                handle,
            } = worker;
            let _ = shutdown_tx.send(());
            pending.push((tenant, handle));
        }

        // Await every worker with a bounded timeout. Collect all timeouts
        // rather than early-returning — leaving live workers running past
        // `shutdown` would leak `Arc<dyn MemoryStore>` clones and keep the
        // runtime busy. On timeout we abort via `AbortHandle` (which does
        // NOT consume the JoinHandle, unlike `tokio::time::timeout(handle)`)
        // so no worker outlives us.
        let mut timed_out = 0u64;
        for (tenant, mut handle) in pending {
            let abort = handle.abort_handle();
            match tokio::time::timeout(SHUTDOWN_WORKER_TIMEOUT, &mut handle).await {
                Ok(Ok(())) => {
                    debug!(tenant = %tenant, "retention reaper worker drained");
                }
                Ok(Err(join_error)) if join_error.is_cancelled() => {
                    debug!(tenant = %tenant, "retention reaper worker cancelled during shutdown");
                }
                Ok(Err(join_error)) => {
                    warn!(tenant = %tenant, error = %join_error, "retention reaper worker failed during shutdown");
                }
                Err(_elapsed) => {
                    warn!(tenant = %tenant, "retention reaper worker did not drain within timeout — aborting");
                    abort.abort();
                    // Await the abort to complete so the task is truly gone
                    // before we return. Grace period is short — the task is
                    // already in the abort queue.
                    let _ = tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                    timed_out += 1;
                }
            }
        }

        if timed_out > 0 {
            return Err(MemoryError::ShutdownTimeout);
        }
        Ok(())
    }
}

/// Entry point for the per-tenant worker task. Ticks at `config.interval`
/// and runs a sweep each tick. On shutdown, runs one final drain sweep so
/// pending expired entries are reaped before the task exits.
async fn run_worker(
    tenant: TenantId,
    config: Arc<RetentionReaperConfig>,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut interval = tokio::time::interval(config.interval);
    // Missed ticks don't compound — retention is an eventually-consistent
    // background process. One tick per interval is enough.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate initial tick so the first sweep honors the
    // configured interval (otherwise `register_tenant` triggers a sweep
    // eagerly and the interval assertion becomes trivial).
    interval.tick().await;
    let mut should_shutdown = false;

    while !should_shutdown {
        // Wait for either a tick or shutdown. If shutdown wins, we still
        // run one last drain sweep below so the caller's
        // "shutdown drains pending sweeps" contract holds.
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                should_shutdown = true;
            }
            _ = interval.tick() => {}
        }

        // Run the sweep OUTSIDE of `select!` so a shutdown signal that
        // arrives mid-sweep cannot cancel the in-flight deletions.
        match sweep_tenant(&tenant, &config, &store, &index).await {
            Ok(stats) => {
                if stats.paths_deleted > 0 {
                    debug!(
                        tenant = %tenant,
                        paths_deleted = stats.paths_deleted,
                        batches = stats.batches,
                        "retention reaper sweep"
                    );
                }
            }
            Err(e) => {
                warn!(tenant = %tenant, error = %e, "retention reaper sweep failed");
            }
        }
    }
}

/// Perform a single sweep across every configured subtree for `tenant`.
///
/// Deletes expired entries in batches of `config.batch_size`, yielding to
/// the scheduler between batches so the per-tenant write lock is released.
///
/// Emits one `memory_reaper_sweep` span per subtree (per S037 §18) with
/// `tenant`, `subtree`, `deleted_count`, `duration_ms` attributes.
async fn sweep_tenant(
    tenant: &TenantId,
    config: &RetentionReaperConfig,
    store: &Arc<dyn MemoryStore>,
    index: &Arc<dyn VectorIndex>,
) -> Result<ReaperStats, MemoryError> {
    let batch_size = config.batch_size.max(1) as usize;
    let mut stats = ReaperStats::default();

    for subtree in &config.subtrees {
        let subtree_stats = sweep_subtree(tenant, subtree, batch_size, store, index).await?;
        stats.subtrees_scanned += 1;
        stats.paths_deleted += subtree_stats.paths_deleted;
        stats.batches += subtree_stats.batches;
        stats.index_failures += subtree_stats.index_failures;
    }

    Ok(stats)
}

/// Per-subtree stats used for span instrumentation.
struct SubtreeStats {
    paths_deleted: u64,
    batches: u64,
    index_failures: u64,
}

async fn sweep_subtree(
    tenant: &TenantId,
    subtree: &RetentionSubtree,
    batch_size: usize,
    store: &Arc<dyn MemoryStore>,
    index: &Arc<dyn VectorIndex>,
) -> Result<SubtreeStats, MemoryError> {
    // Per S037 §18: span name `memory_reaper_sweep` with tenant, subtree,
    // deleted_count, duration_ms.
    let span = info_span!(
        "memory_reaper_sweep",
        tenant = tenant.as_str(),
        subtree = subtree.prefix.as_str(),
        deleted_count = field::Empty,
        duration_ms = field::Empty,
    );
    let start = std::time::Instant::now();
    let result = sweep_subtree_inner(tenant, subtree, batch_size, store, index)
        .instrument(span.clone())
        .await;
    record_subtree_stats(&span, &result, start);
    result
}

fn record_subtree_stats(
    span: &Span,
    result: &Result<SubtreeStats, MemoryError>,
    start: std::time::Instant,
) {
    let duration_ms = start.elapsed().as_millis() as u64;
    span.record("duration_ms", duration_ms);
    if let Ok(stats) = result {
        span.record("deleted_count", stats.paths_deleted);
    }
}

async fn sweep_subtree_inner(
    tenant: &TenantId,
    subtree: &RetentionSubtree,
    batch_size: usize,
    store: &Arc<dyn MemoryStore>,
    index: &Arc<dyn VectorIndex>,
) -> Result<SubtreeStats, MemoryError> {
    let mut stats = SubtreeStats {
        paths_deleted: 0,
        batches: 0,
        index_failures: 0,
    };
    let now = SystemTime::now();
    let entries = store.list_prefix(tenant, &subtree.prefix)?;
    let expired: Vec<(MemoryPath, _)> = entries
        .into_iter()
        .filter_map(|entry| {
            let elapsed = now.duration_since(entry.mtime).unwrap_or_default();
            if elapsed > subtree.ttl {
                Some((entry.path, entry.version))
            } else {
                None
            }
        })
        .collect();

    for chunk in expired.chunks(batch_size) {
        for (path, _prior_version) in chunk {
            match store.delete(tenant, path) {
                Ok(tombstone_version) => {
                    // Synchronously clear the vector index so downstream
                    // queries never see expired content even if the store's
                    // subscription is lagging. If index deletion fails,
                    // record in `index_failures` — this is a real consistency
                    // gap operators must alert on (store gone, index stale).
                    match index.delete_path(tenant, path, tombstone_version) {
                        Ok(()) => {
                            stats.paths_deleted += 1;
                        }
                        Err(e) => {
                            stats.index_failures += 1;
                            warn!(
                                tenant = %tenant,
                                path = %path,
                                error = %e,
                                "retention reaper: store delete OK but index clear failed — stale chunks may linger"
                            );
                        }
                    }
                }
                Err(MemoryError::NotFound(_)) => {
                    // Raced with another deleter — fine, nothing to do.
                }
                Err(e) => return Err(e),
            }
        }
        stats.batches += 1;
        // Yield so the per-tenant write lock in SqliteMemoryStore is
        // released between batches. Concurrent writers and other
        // tenants get a chance to make progress.
        tokio::task::yield_now().await;
    }

    Ok(stats)
}
