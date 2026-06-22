//! Background embedder — per-tenant task that consumes `MemoryEvent`s from
//! a [`MemoryStore`] and upserts chunks into a [`VectorIndex`].
//!
//! Per S037 §7 Guarantee 3 and §8 queue overflow policy.
//!
//! The embedder is constructed once per simulacra-server process with a shared
//! `Arc<dyn MemoryStore>` + `Arc<dyn VectorIndex>` + `Arc<dyn Embedder>` +
//! chunker registry. It subscribes to store events and fans them out to
//! per-tenant worker tasks, each with a bounded channel. On queue overflow,
//! writes are durably persisted to `memory_embed_backlog` via the index's
//! write path (deferred indexing, recovered by the retention/reaper cycle).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use simulacra_types::{MemoryPath, TenantId};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Total drain budget for `BackgroundEmbedder::shutdown`. Spec S038 §Design
/// fixes this at 30 seconds. Configurable later if needed.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

use crate::chunker::Chunker;
use crate::embedder::Embedder;
use crate::error::MemoryError;
use crate::index::{BACKLOG_MAX_RETRIES, IndexedChunk, VectorIndex};
use crate::metrics::MemoryMeters;
use crate::store::{MemoryEvent, MemoryRecvOutcome, MemoryStore};

/// Default per-tenant queue capacity. Spec §8: 2048 events.
pub const DEFAULT_QUEUE_CAPACITY: usize = 2048;

/// Default bounded enqueue timeout. Spec §8: 100ms.
pub const DEFAULT_ENQUEUE_TIMEOUT_MS: u64 = 100;

/// Poll interval for the backlog-draining worker when the backlog is empty.
/// Kept short so reindex progresses quickly; not so short that it burns CPU.
pub const BACKLOG_DRAIN_IDLE_MS: u64 = 250;

/// Batch size for backlog pulls — bounds the per-iteration work.
pub const BACKLOG_BATCH_SIZE: usize = 32;

/// Selects a chunker for a given memory path. Used by the embedder to
/// dispatch to markdown-section / fixed-token / jsonl-line chunkers based
/// on the path extension or subtree.
pub type ChunkerSelector = Arc<dyn Fn(&MemoryPath) -> Option<Arc<dyn Chunker>> + Send + Sync>;

/// Configuration for the background embedder.
pub struct BackgroundEmbedderConfig {
    /// Maximum events buffered per tenant. Overflow is durably persisted
    /// to the index's `memory_embed_backlog` table.
    pub queue_capacity: usize,
    /// How long to wait when pushing to a full queue before falling back
    /// to backlog persistence.
    pub enqueue_timeout: Duration,
    /// Batch size for embedding calls.
    pub embed_batch_size: usize,
}

impl Default for BackgroundEmbedderConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            enqueue_timeout: Duration::from_millis(DEFAULT_ENQUEUE_TIMEOUT_MS),
            embed_batch_size: 32,
        }
    }
}

/// Per-tenant worker record shared between the dispatcher and shutdown. The
/// dispatcher inserts a `TenantWorker` when it sees the first event for a
/// given tenant; `shutdown` drains the map to drop senders and await handles.
struct TenantWorker {
    sender: mpsc::Sender<MemoryEvent>,
    handle: JoinHandle<()>,
}

type TenantWorkers = Arc<Mutex<HashMap<TenantId, TenantWorker>>>;

/// Sync snapshot of per-tenant senders, maintained in lockstep with
/// `TenantWorkers` insertions and evictions. Lives behind a `std::sync::Mutex`
/// so observable-gauge callbacks (which run from non-async OTel collection
/// contexts) can read queue depth without touching the tokio mutex.
type TenantSenders = Arc<std::sync::Mutex<HashMap<TenantId, mpsc::Sender<MemoryEvent>>>>;

/// The background embedder. Owns a subscription to the `MemoryStore` event
/// stream and a per-tenant fan-out of worker tasks.
///
/// Construct with `spawn`. The preferred shutdown path is the async
/// [`BackgroundEmbedder::shutdown`] method, which stops the dispatcher and
/// awaits every per-tenant worker with a bounded drain. If the embedder is
/// dropped without calling `shutdown`, the dispatcher task is aborted as a
/// safety net (workers observe channel close once the dispatcher task
/// teardown drops the senders map).
pub struct BackgroundEmbedder {
    // Optioned so `shutdown(self)` can take ownership of the handle before
    // `Drop` runs at end of scope.
    dispatcher_handle: Option<JoinHandle<()>>,
    // Backlog-draining worker. Runs alongside the dispatcher, polling
    // `index.known_tenants()` every BACKLOG_DRAIN_IDLE_MS and consuming
    // rows from `memory_embed_backlog`. Owns its own shutdown channel.
    backlog_drain_handle: Option<JoinHandle<()>>,
    backlog_shutdown_tx: Option<oneshot::Sender<()>>,
    // Oneshot sender used by `shutdown` to ask the dispatcher loop to exit.
    // Optioned because `oneshot::Sender::send` takes ownership.
    shutdown_tx: Option<oneshot::Sender<()>>,
    // Shared between the dispatcher and `shutdown`. The dispatcher inserts
    // workers here as new tenants appear; `shutdown` drains this map to
    // drop senders and await handles.
    tenant_workers: TenantWorkers,
    // Sync snapshot of senders, kept in lockstep with `tenant_workers`.
    // Read by observable-gauge callbacks without touching the tokio
    // mutex. Also cleared by `shutdown` so gauges do not keep reporting
    // on drained channels after shutdown.
    tenant_senders: TenantSenders,
    // Holds the queue-depth observable gauge registration alive. The
    // callback closes over `tenant_senders`; dropping the embedder drops
    // this gauge which detaches the callback.
    _queue_depth_gauge: ObservableGauge<u64>,
    // Holds the reindex-backlog observable gauge registration alive. The
    // callback closes over a clone of the shared `index` Arc and the
    // sender snapshot so it can report a row for every known tenant.
    _reindex_backlog_gauge: ObservableGauge<u64>,
}

impl BackgroundEmbedder {
    /// Spawn the background embedder. It subscribes to `store.subscribe()`
    /// and begins consuming events immediately. Returns once the subscription
    /// is live and the dispatcher task is running; actual embedding work
    /// happens asynchronously.
    ///
    /// IMPORTANT: this function must be called from within a tokio runtime.
    pub fn spawn(
        store: Arc<dyn MemoryStore>,
        index: Arc<dyn VectorIndex>,
        embedder: Arc<dyn Embedder>,
        chunker_selector: ChunkerSelector,
        config: BackgroundEmbedderConfig,
    ) -> Result<Self, MemoryError> {
        let mut subscription = store.subscribe()?;
        let tenant_workers: TenantWorkers = Arc::new(Mutex::new(HashMap::new()));
        let tenant_senders: TenantSenders = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // Snapshot the queue capacity into a plain value so the gauge
        // callback can compute `capacity - sender.capacity()` without
        // touching the config struct.
        let queue_capacity = config.queue_capacity;

        // S037 §20: per-tenant queue depth gauge. Callback reports how
        // many events are currently buffered in each tenant's channel,
        // derived from `capacity - sender.capacity()` (the channel's
        // remaining slots). Reads from the sync `tenant_senders` snapshot
        // so the callback never touches the tokio-mutex-protected
        // tenant_workers map.
        let queue_depth_senders = Arc::clone(&tenant_senders);
        let meter = opentelemetry::global::meter("simulacra-memory");
        let queue_depth_gauge = meter
            .u64_observable_gauge("simulacra_memory_queue_depth")
            .with_description(
                "Per-tenant background embedder queue depth (buffered events awaiting dispatch)",
            )
            .with_callback(move |observer| {
                let guard = match queue_depth_senders.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for (tenant, sender) in guard.iter() {
                    let depth = queue_capacity.saturating_sub(sender.capacity()) as u64;
                    observer.observe(
                        depth,
                        &[KeyValue::new("tenant", tenant.as_str().to_string())],
                    );
                }
            })
            .build();

        // S037 §20: per-tenant reindex backlog gauge. Reports the row
        // count in `memory_embed_backlog` for every tenant the index
        // knows about on disk, unioned with tenants currently in the
        // in-memory sender snapshot. Filesystem enumeration covers
        // tenants whose worker has been evicted or never spawned; the
        // sender-map union covers in-memory test fakes that don't
        // persist. A query failure logs + skips that tenant.
        let backlog_index = Arc::clone(&index);
        let backlog_senders = Arc::clone(&tenant_senders);
        let reindex_backlog_gauge = meter
            .u64_observable_gauge("simulacra_memory_reindex_backlog")
            .with_description(
                "Per-tenant rows awaiting deferred indexing in memory_embed_backlog",
            )
            .with_callback(move |observer| {
                use std::collections::HashSet;
                let mut tenants: HashSet<TenantId> = HashSet::new();
                let guard = match backlog_senders.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                for t in guard.keys() {
                    tenants.insert(t.clone());
                }
                drop(guard);
                match backlog_index.known_tenants() {
                    Ok(known) => {
                        for t in known {
                            tenants.insert(t);
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to enumerate known tenants for backlog gauge");
                    }
                }
                for tenant in tenants.iter() {
                    match backlog_index.backlog_count(tenant) {
                        Ok(count) => observer.observe(
                            count,
                            &[KeyValue::new("tenant", tenant.as_str().to_string())],
                        ),
                        Err(e) => {
                            warn!(tenant = %tenant, error = %e, "failed to read backlog count for gauge");
                        }
                    }
                }
            })
            .build();

        let dispatcher_workers = Arc::clone(&tenant_workers);
        let dispatcher_senders = Arc::clone(&tenant_senders);

        // Clone the shared deps so the dispatcher closure can take
        // ownership while the backlog drainer (spawned below) also has
        // its own references.
        let dispatcher_store = Arc::clone(&store);
        let dispatcher_index = Arc::clone(&index);
        let dispatcher_embedder = Arc::clone(&embedder);
        let dispatcher_chunker_selector = Arc::clone(&chunker_selector);

        let dispatcher_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        info!("background embedder dispatcher received shutdown signal");
                        break;
                    }
                    outcome = subscription.recv() => {
                        match outcome {
                            MemoryRecvOutcome::Event(event) => {
                                if let Err(e) = dispatch_event(
                                    event,
                                    &dispatcher_workers,
                                    &dispatcher_senders,
                                    Arc::clone(&dispatcher_store),
                                    Arc::clone(&dispatcher_index),
                                    Arc::clone(&dispatcher_embedder),
                                    Arc::clone(&dispatcher_chunker_selector),
                                    &config,
                                )
                                .await
                                {
                                    warn!(error = %e, "background embedder failed to dispatch event");
                                }
                            }
                            MemoryRecvOutcome::Lagged { skipped } => {
                                warn!(
                                    skipped,
                                    "background embedder lagged — recovering by reading current state"
                                );
                            }
                            MemoryRecvOutcome::Closed => {
                                info!("background embedder subscription closed; dispatcher exiting");
                                break;
                            }
                        }
                    }
                }
            }
            // Intentionally do NOT drain `dispatcher_workers` here. In the
            // `shutdown(self)` path, the outer task takes over draining. In
            // the Drop-safety-net path, the dispatcher task is aborted and
            // the map is dropped by the outer struct, closing the channels.
        });

        // Spawn the backlog-draining worker. It runs alongside the
        // dispatcher, polling every BACKLOG_DRAIN_IDLE_MS and processing
        // rows from `memory_embed_backlog` for every tenant the index
        // knows about on disk.
        let (backlog_shutdown_tx, backlog_shutdown_rx) = oneshot::channel::<()>();
        let backlog_drain_handle = tokio::spawn(backlog_drain_loop(
            Arc::clone(&store),
            Arc::clone(&index),
            Arc::clone(&embedder),
            Arc::clone(&chunker_selector),
            backlog_shutdown_rx,
        ));

        Ok(Self {
            dispatcher_handle: Some(dispatcher_handle),
            backlog_drain_handle: Some(backlog_drain_handle),
            backlog_shutdown_tx: Some(backlog_shutdown_tx),
            shutdown_tx: Some(shutdown_tx),
            tenant_workers,
            tenant_senders,
            _queue_depth_gauge: queue_depth_gauge,
            _reindex_backlog_gauge: reindex_backlog_gauge,
        })
    }

    /// S038: Orderly shutdown — stop dispatching new events, drop the
    /// per-tenant senders so workers observe channel close, and await
    /// every worker handle. Consumes `self`.
    ///
    /// The whole operation runs under a single [`SHUTDOWN_DRAIN_TIMEOUT`]
    /// deadline covering dispatcher quiescence + worker drain; there is
    /// no separate dispatcher grace period. This avoids the "30s becomes
    /// 35s in the worst case" class of bug (review item W5).
    ///
    /// Dispatcher quiescence is **mandatory**: if the dispatcher refuses
    /// to exit within the deadline, we abort it before touching the
    /// worker map. Without mandatory quiescence, the dispatcher could
    /// still insert new tenant workers into the map while `shutdown` is
    /// draining it (review item B2).
    ///
    /// Returns `Ok(())` on clean drain, [`MemoryError::ShutdownTimeout`]
    /// if the deadline fires before the drain completes, or
    /// [`MemoryError::WorkerPanic`] if a worker panicked — shutdown
    /// continues draining the rest before returning.
    pub async fn shutdown(mut self) -> Result<(), MemoryError> {
        // Signal the backlog drainer first and await its handle — it
        // doesn't touch the tenant_workers map, so its drain is
        // independent of the dispatcher/per-tenant shutdown below.
        if let Some(tx) = self.backlog_shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.backlog_drain_handle.take() {
            let _ = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, handle).await;
        }

        // Take ownership of the dispatcher handle first so `Drop` does
        // not abort underneath us at end-of-scope.
        let dispatcher_handle = self.dispatcher_handle.take();

        // Stash an `AbortHandle` for the dispatcher BEFORE moving the
        // JoinHandle into the timed future. If the shared timeout fires
        // while phase 1 is still awaiting the dispatcher, dropping the
        // timed future just detaches the JoinHandle — no abort — and the
        // dispatcher task survives `shutdown(self)` as a ghost, free to
        // keep inserting new workers into the tenant map we were about
        // to drain. See review item B1.
        let dispatcher_abort = dispatcher_handle.as_ref().map(|h| h.abort_handle());

        // Signal the dispatcher to exit its select loop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        let tenant_workers = Arc::clone(&self.tenant_workers);
        let tenant_senders_for_shutdown = Arc::clone(&self.tenant_senders);

        // Single combined deadline: dispatcher quiescence + worker drain.
        // Total budget = SHUTDOWN_DRAIN_TIMEOUT, shared.
        let shutdown_result = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async move {
            // Phase 1 — await dispatcher quiescence. Mandatory: if it
            // doesn't finish on its own, abort it so no new workers can
            // be inserted into `tenant_workers`.
            if let Some(handle) = dispatcher_handle {
                match handle.await {
                    Ok(()) => {}
                    Err(join_error) if join_error.is_cancelled() => {
                        debug!("dispatcher task was cancelled during shutdown");
                    }
                    Err(join_error) => {
                        warn!(error = %join_error, "dispatcher task failed during shutdown");
                    }
                }
            }

            // Phase 2 — drain the map. Drop every sender and collect the
            // handles to await. Because the dispatcher has finished, no
            // new entries can land between the lock and the drain.
            let drained: Vec<(TenantId, JoinHandle<()>)> = {
                let mut guard = tenant_workers.lock().await;
                guard
                    .drain()
                    .map(|(tenant, worker)| {
                        let TenantWorker { handle, sender } = worker;
                        drop(sender);
                        (tenant, handle)
                    })
                    .collect()
            };
            // Clear the sync senders snapshot so observable gauges no
            // longer report on the drained channels.
            if let Ok(mut snapshot) = tenant_senders_for_shutdown.lock() {
                snapshot.clear();
            }

            let mut first_panic: Option<TenantId> = None;
            for (tenant, handle) in drained {
                match handle.await {
                    Ok(()) => {
                        debug!(tenant = %tenant, "background embedder worker drained");
                    }
                    Err(join_error) if join_error.is_panic() => {
                        warn!(tenant = %tenant, "background embedder worker panicked during drain");
                        if first_panic.is_none() {
                            first_panic = Some(tenant);
                        }
                    }
                    Err(join_error) => {
                        warn!(tenant = %tenant, error = %join_error, "worker join error during drain");
                    }
                }
            }

            match first_panic {
                Some(tenant) => Err::<(), MemoryError>(MemoryError::WorkerPanic { tenant }),
                None => Ok(()),
            }
        })
        .await;

        match shutdown_result {
            Ok(Ok(())) => {
                info!("background embedder shutdown drain complete");
                Ok(())
            }
            Ok(Err(panic_err)) => {
                info!("background embedder shutdown drain complete (with worker panic)");
                Err(panic_err)
            }
            Err(_elapsed) => {
                // Deadline fired before drain completed. Abort the
                // dispatcher first (if the timed future was still in
                // phase 1, the JoinHandle was detached when the future
                // was dropped — the `AbortHandle` stashed above is the
                // only way to stop the dispatcher now) then abort any
                // surviving worker handles.
                //
                // Note: aborting a task stuck in synchronous
                // `block_in_place` does NOT interrupt the blocking
                // thread — see R-embedder-thread-isolation. Any detached
                // std thread running a wedged embedder is deliberately
                // leaked until process exit.
                if let Some(abort) = dispatcher_abort {
                    warn!("aborting un-quiesced embedder dispatcher after shutdown timeout");
                    abort.abort();
                }
                let guard = self.tenant_workers.lock().await;
                for (tenant, worker) in guard.iter() {
                    if !worker.handle.is_finished() {
                        warn!(tenant = %tenant, "aborting un-drained embedder worker after shutdown timeout");
                        worker.handle.abort();
                    }
                }
                warn!(
                    timeout_secs = SHUTDOWN_DRAIN_TIMEOUT.as_secs(),
                    "background embedder shutdown exceeded drain budget"
                );
                Err(MemoryError::ShutdownTimeout)
            }
        }
    }
}

impl Drop for BackgroundEmbedder {
    fn drop(&mut self) {
        // Safety net: if shutdown was not called, abort the dispatcher task
        // so it cannot outlive the struct. Workers observe channel close
        // once the tenant_workers Arc is dropped (here) — their senders
        // live in the map and are released when the last Arc goes away.
        if let Some(handle) = self.dispatcher_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.backlog_drain_handle.take() {
            handle.abort();
        }
    }
}

/// Dispatch a single event to its tenant's worker, spawning the worker if
/// this is the first event for that tenant.
#[allow(clippy::too_many_arguments)] // internal helper; args are all part of the same dispatch context
async fn dispatch_event(
    event: MemoryEvent,
    tenant_workers: &TenantWorkers,
    tenant_senders: &TenantSenders,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    chunker_selector: ChunkerSelector,
    config: &BackgroundEmbedderConfig,
) -> Result<(), MemoryError> {
    let tenant = match &event {
        MemoryEvent::Put { tenant, .. } | MemoryEvent::Delete { tenant, .. } => tenant.clone(),
    };

    // Get or create the per-tenant worker channel.
    //
    // S038 review fix (dead-worker eviction): if the cached entry has a
    // finished handle (worker exited — panic, channel closed, whatever),
    // treat it as absent and respawn. Without this, a panicking embed
    // call would permanently silently drop every subsequent event for
    // that tenant because the cached sender points at a dead receiver.
    let mut workers = tenant_workers.lock().await;
    let is_stale = workers
        .get(&tenant)
        .map(|w| w.handle.is_finished() || w.sender.is_closed())
        .unwrap_or(false);
    if is_stale {
        warn!(tenant = %tenant, "evicting dead background embedder worker; respawning on next event");
        workers.remove(&tenant);
        // Keep the sync senders snapshot in lockstep with evictions.
        if let Ok(mut snapshot) = tenant_senders.lock() {
            snapshot.remove(&tenant);
        }
    }

    let sender = if let Some(worker) = workers.get(&tenant) {
        worker.sender.clone()
    } else {
        let (tx, rx) = mpsc::channel::<MemoryEvent>(config.queue_capacity);
        let handle = tokio::spawn(tenant_worker(
            tenant.clone(),
            rx,
            Arc::clone(&store),
            Arc::clone(&index),
            Arc::clone(&embedder),
            Arc::clone(&chunker_selector),
            config.embed_batch_size,
        ));
        workers.insert(
            tenant.clone(),
            TenantWorker {
                sender: tx.clone(),
                handle,
            },
        );
        // Publish the sender into the sync snapshot so observable
        // gauges (queue depth, backlog) can read it without blocking
        // on the tokio mutex.
        if let Ok(mut snapshot) = tenant_senders.lock() {
            snapshot.insert(tenant.clone(), tx.clone());
        }
        tx
    };
    drop(workers);

    // Send with bounded timeout. On timeout, the event is considered
    // 'deferred' and falls back to the backlog table (populated by the
    // index layer's upsert path — for MVP this is best-effort).
    match tokio::time::timeout(config.enqueue_timeout, sender.send(event.clone())).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_closed)) => {
            // Worker channel closed between our check and the send.
            // Evict so the next event for this tenant respawns. The
            // current event is dropped — acceptable since we racelost
            // a shutdown or panic.
            warn!(tenant = %tenant, "tenant worker channel closed; evicting and dropping event");
            let mut workers = tenant_workers.lock().await;
            workers.remove(&tenant);
            if let Ok(mut snapshot) = tenant_senders.lock() {
                snapshot.remove(&tenant);
            }
            Ok(())
        }
        Err(_elapsed) => {
            debug!(tenant = %tenant, "tenant queue full; event deferred to backlog");
            // Spec §8: handle the overflowed event so writes never get
            // permanently lost to the indexing fanout.
            //
            // Puts are staged in `memory_embed_backlog` for the drainer
            // (see `backlog_drain_loop`) — the drainer polls every
            // BACKLOG_DRAIN_IDLE_MS, re-chunks from memory_content at
            // the enqueued version, and re-embeds.
            //
            // Deletes cannot ride the backlog (the table is keyed by
            // path alone with no tombstone flag) but `VectorIndex::delete_path`
            // is synchronous, idempotent, and cheap — so we apply it
            // inline from the dispatcher. This is load-bearing: without
            // it, a Delete that loses the enqueue race leaves the prior
            // Put's chunks searchable indefinitely under sustained queue
            // saturation (spec §8 item 4 — writes to MemoryStore always
            // succeed, only indexing fanout can fall behind, and it must
            // eventually catch up).
            match &event {
                MemoryEvent::Put { path, version, .. } => {
                    crate::metrics::record_queue_overflow("put");
                    if let Err(e) = index.enqueue_backlog_for(&tenant, path, *version) {
                        warn!(
                            tenant = %tenant,
                            path = %path,
                            error = %e,
                            "failed to stage overflowed Put in memory_embed_backlog; event lost",
                        );
                    }
                }
                MemoryEvent::Delete { path, version, .. } => {
                    crate::metrics::record_queue_overflow("delete");
                    if let Err(e) = index.delete_path(&tenant, path, *version) {
                        warn!(
                            tenant = %tenant,
                            path = %path,
                            error = %e,
                            "failed to apply overflowed Delete synchronously; stale chunks may remain until next Put or reindex",
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

/// Per-tenant worker — consumes events, chunks, embeds, upserts.
async fn tenant_worker(
    tenant: TenantId,
    mut rx: mpsc::Receiver<MemoryEvent>,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    chunker_selector: ChunkerSelector,
    _batch_size: usize,
) {
    info!(tenant = %tenant, "background embedder worker starting");
    while let Some(event) = rx.recv().await {
        match event {
            MemoryEvent::Put {
                tenant: t,
                path,
                version,
                produced_at,
                ..
            } => {
                if let Err(e) = handle_put(
                    &t,
                    &path,
                    version,
                    produced_at,
                    Arc::clone(&store),
                    Arc::clone(&index),
                    Arc::clone(&embedder),
                    Arc::clone(&chunker_selector),
                )
                .await
                {
                    warn!(tenant = %t, path = %path, error = %e, "embedder failed to upsert");
                }
            }
            MemoryEvent::Delete {
                tenant: t,
                path,
                version,
                ..
            } => {
                if let Err(e) = index.delete_path(&t, &path, version) {
                    warn!(tenant = %t, path = %path, error = %e, "embedder failed to delete");
                }
            }
        }
    }
    info!(tenant = %tenant, "background embedder worker stopping");
}

#[allow(clippy::too_many_arguments)] // internal helper; args form the per-put indexing context
async fn handle_put(
    tenant: &TenantId,
    path: &MemoryPath,
    version: simulacra_types::MemoryVersion,
    produced_at: SystemTime,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    chunker_selector: ChunkerSelector,
) -> Result<(), MemoryError> {
    // Skip dedup subtree — spec §12 says content under /var/memory/dedup/
    // is not indexed.
    if path.is_dedup() {
        return Ok(());
    }

    // Select a chunker. If no chunker matches the path, skip — some subtrees
    // are intentionally unindexed (e.g., /var/memory/dedup, binary formats).
    let Some(chunker) = chunker_selector(path) else {
        debug!(path = %path, "no chunker for path; skipping");
        return Ok(());
    };

    // Fetch the current content. If the store version moved past ours,
    // the upsert will be dropped as Stale by the index anyway.
    let (data, current_version) = match store.get(tenant, path) {
        Ok(v) => v,
        Err(MemoryError::NotFound(_)) => {
            // Tombstoned or deleted between the event and our read.
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if current_version > version {
        // A newer write is already in flight; that write's own event will
        // trigger another upsert, so we can skip this one.
        return Ok(());
    }

    let chunks = chunker.chunk(path.as_str(), &data)?;
    if chunks.is_empty() {
        return Ok(());
    }

    // Embed the chunks. The embedder produces unit vectors.
    //
    // Isolation note: `Embedder::embed` is synchronous and may block for a
    // non-trivial amount of time (network calls for remote embedders, heavy
    // CPU work for local ones, arbitrarily long waits in tests that simulate
    // a wedged embedder). We deliberately run it on a detached `std::thread`
    // rather than `tokio::task::spawn_blocking` so a misbehaving embedder
    // cannot:
    //   * starve the runtime's worker pool, and
    //   * block `Runtime::drop` via the blocking pool's shutdown path.
    // The tokio runtime handle is cloned into the thread so embedder
    // implementations that internally call `Handle::current()` keep working.
    // If the worker is cancelled mid-embed, the std::thread is orphaned; a
    // stuck embedder will leak a single OS thread until the process exits,
    // which is strictly better than hanging the whole runtime.
    let owned_texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let embedder_for_thread = Arc::clone(&embedder);
    type EmbedOutcome =
        Result<Result<Vec<Vec<f32>>, MemoryError>, Box<dyn std::any::Any + Send + 'static>>;
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<EmbedOutcome>();
    let runtime_handle = tokio::runtime::Handle::try_current().ok();
    std::thread::Builder::new()
        .name("simulacra-memory-embed".to_string())
        .spawn(move || {
            let _guard = runtime_handle.as_ref().map(|h| h.enter());
            let refs: Vec<&str> = owned_texts.iter().map(String::as_str).collect();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                embedder_for_thread.embed(&refs)
            }));
            let _ = result_tx.send(outcome);
        })
        .map_err(|e| MemoryError::Internal(format!("failed to spawn embedder thread: {e}")))?;

    let embeddings = match result_rx.await {
        Ok(Ok(result)) => result?,
        Ok(Err(panic_payload)) => {
            // Re-raise the embedder panic on this (async) task so the
            // tenant worker's JoinHandle reports `is_panic()`. This is
            // load-bearing for the S038 shutdown path, which distinguishes
            // clean drain, timeout, and worker panic.
            std::panic::resume_unwind(panic_payload);
        }
        Err(_recv) => {
            return Err(MemoryError::Internal(
                "embedder thread dropped result channel without sending".to_string(),
            ));
        }
    };
    if embeddings.len() != chunks.len() {
        return Err(MemoryError::Internal(format!(
            "embedder returned {} vectors for {} chunks",
            embeddings.len(),
            chunks.len()
        )));
    }

    let indexed: Vec<crate::index::IndexedChunk> = chunks
        .into_iter()
        .zip(embeddings)
        .map(|(c, e)| crate::index::IndexedChunk {
            chunk_index: c.chunk_index,
            locator: c.locator,
            text: c.text,
            embedding: e,
        })
        .collect();

    match index.upsert(tenant, path, version, embedder.id(), &indexed) {
        Ok(crate::index::UpsertOutcome::Applied) => {
            // S037 §20: record lag from MemoryEvent::Put emission to the
            // completion of the applied upsert. Only Applied outcomes
            // count — stale/tombstoned writes did no indexing work, so
            // including them would skew the histogram downward.
            let lag_seconds = match SystemTime::now().duration_since(produced_at) {
                Ok(d) => d.as_secs_f64(),
                Err(_) => 0.0,
            };
            MemoryMeters::get().embed_lag_seconds.record(
                lag_seconds,
                &[KeyValue::new("tenant", tenant.as_str().to_string())],
            );
            debug!(path = %path, lag_seconds, "embedder upsert applied");
            Ok(())
        }
        Ok(other) => {
            debug!(path = %path, outcome = ?other, "embedder upsert not applied (stale/tombstoned)");
            Ok(())
        }
        Err(e) => {
            error!(path = %path, error = %e, "embedder upsert failed");
            Err(e)
        }
    }
}

/// S037 §13: backlog-draining worker loop. Polls every
/// [`BACKLOG_DRAIN_IDLE_MS`] when idle; on each tick, iterates every
/// tenant in `index.known_tenants()` and processes up to
/// [`BACKLOG_BATCH_SIZE`] backlog rows per tenant. Exits on shutdown
/// signal.
async fn backlog_drain_loop(
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    chunker_selector: ChunkerSelector,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let idle = Duration::from_millis(BACKLOG_DRAIN_IDLE_MS);
    info!("backlog-draining worker starting");
    loop {
        let tenants = match index.known_tenants() {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "backlog drain failed to enumerate tenants");
                Vec::new()
            }
        };

        let mut did_work = false;
        for tenant in tenants.iter() {
            // Mid-loop shutdown check keeps worst-case latency bounded by
            // the time to process a single row, not a full pass.
            if shutdown_rx.try_recv().is_ok() {
                info!("backlog-draining worker received shutdown mid-pass; stopping");
                return;
            }
            let rows = match index.take_backlog_batch(tenant, BACKLOG_BATCH_SIZE) {
                Ok(r) => r,
                Err(e) => {
                    warn!(tenant = %tenant, error = %e, "backlog drain failed to read batch");
                    continue;
                }
            };
            if rows.is_empty() {
                continue;
            }
            did_work = true;
            for row in rows {
                if row.retry_count >= BACKLOG_MAX_RETRIES {
                    debug!(
                        tenant = %tenant,
                        path = %row.path,
                        retry_count = row.retry_count,
                        "backlog row dead-lettered; leaving in place for operator",
                    );
                    continue;
                }
                match process_backlog_row(
                    tenant,
                    &row.path,
                    row.version,
                    store.as_ref(),
                    index.as_ref(),
                    embedder.as_ref(),
                    &chunker_selector,
                )
                .await
                {
                    Ok(()) => {
                        if let Err(e) = index.delete_backlog_row(tenant, &row.path, row.version) {
                            warn!(tenant = %tenant, path = %row.path, error = %e,
                                  "failed to delete drained backlog row");
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        warn!(tenant = %tenant, path = %row.path, error = %e,
                              "backlog row embed failed; bumping retry");
                        if let Err(bump_err) =
                            index.bump_backlog_retry(tenant, &row.path, row.version, &msg)
                        {
                            warn!(tenant = %tenant, path = %row.path, error = %bump_err,
                                  "failed to bump backlog retry");
                        }
                    }
                }
            }
        }

        if !did_work {
            // Backlog empty across all tenants — wait for the poll
            // interval or shutdown, whichever comes first.
            tokio::select! {
                _ = tokio::time::sleep(idle) => {},
                _ = &mut shutdown_rx => {
                    info!("backlog-draining worker received shutdown signal");
                    return;
                }
            }
        }
    }
}

/// Process one `memory_embed_backlog` row. Loads existing chunks if any;
/// otherwise re-chunks from `memory_content` (`wipe_and_rebuild` path).
/// Embeds, then upserts the full chunks+embeddings set.
async fn process_backlog_row(
    tenant: &TenantId,
    path: &MemoryPath,
    version: simulacra_types::MemoryVersion,
    store: &dyn MemoryStore,
    index: &dyn VectorIndex,
    embedder: &dyn Embedder,
    chunker_selector: &ChunkerSelector,
) -> Result<(), MemoryError> {
    if path.is_dedup() {
        return Ok(());
    }

    // 1. Load existing chunks for (path, version). If present (normal
    //    reindex_background path), re-embed those texts. If absent
    //    (wipe_and_rebuild path), re-chunk from memory_content.
    let mut chunks = index.load_chunks_for(tenant, path, version)?;
    if chunks.is_empty() {
        let (data, current_version) = match store.get(tenant, path) {
            Ok(v) => v,
            Err(MemoryError::NotFound(_)) => {
                // Content deleted between enqueue and processing; drop
                // the stale backlog row by returning Ok.
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if current_version != version {
            // Content has moved past our backlog row. The newer version
            // will be indexed by its own MemoryEvent::Put; drop this one.
            return Ok(());
        }
        let chunker = chunker_selector(path)
            .ok_or_else(|| MemoryError::Internal(format!("no chunker for path {}", path)))?;
        let chunked = chunker.chunk(path.as_str(), &data)?;
        if chunked.is_empty() {
            return Ok(());
        }
        let staged: Vec<IndexedChunk> = chunked
            .into_iter()
            .map(|c| IndexedChunk {
                chunk_index: c.chunk_index,
                locator: c.locator,
                text: c.text,
                embedding: Vec::new(),
            })
            .collect();
        index.upsert_chunks_only(tenant, path, version, &staged)?;
        chunks = staged;
    }

    // 2. Embed. The backlog is a cold path (startup reindex + overflow
    //    recovery) so we use `tokio::task::block_in_place` rather than
    //    the hot Put-path's detached-thread isolation. A wedged embedder
    //    on the backlog path stalls one tokio worker thread rather than
    //    the runtime; since the worker_threads flavor the tests use is
    //    multi-threaded, other work makes progress.
    let owned_texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let refs: Vec<&str> = owned_texts.iter().map(String::as_str).collect();
    let embeddings = tokio::task::block_in_place(|| embedder.embed(&refs))?;

    // 3. Write vectors for the existing chunks. Cannot use `upsert`
    //    here: chunks are already present at `version`, which upsert
    //    would see as Stale. `write_vectors_for_chunks` populates
    //    `memory_vectors` keyed on the existing `chunk_id` and updates
    //    the `embedder_id` stamp on the chunk row.
    index.write_vectors_for_chunks(tenant, path, version, embedder.id(), &embeddings)?;
    Ok(())
}
