//! OTel meter instruments for the memory subsystem.
//!
//! Per S037 §20 Observability, the memory subsystem exports four metrics:
//!
//! - `simulacra_memory_embed_lag_seconds` (histogram, seconds) — wall time between
//!   `MemoryEvent::Put.produced_at` and the completion of the corresponding
//!   `VectorIndex::upsert`. Bucket bounds cover the spec's p50 < 2s / p99 <
//!   30s target.
//! - `simulacra_memory_queue_depth` (observable gauge, events) — per-tenant
//!   queue fill count for the BackgroundEmbedder's mpsc channels.
//! - `simulacra_memory_reindex_backlog` (observable gauge, rows) — per-tenant
//!   row count in `memory_embed_backlog`, the durable deferred-indexing
//!   table populated by the queue-overflow path.
//! - `simulacra_memory_embedder_load_failures_total` (counter, failures) —
//!   incremented whenever the configured embedder cannot be loaded at
//!   startup. A permanently non-zero value is an alertable SLO breach:
//!   the memory subsystem degrades to "no indexing" until resolved.
//! - `simulacra_memory_overflow_total` (counter, events) — incremented by
//!   the BackgroundEmbedder's `dispatch_event` whenever the per-tenant
//!   queue is saturated and an event falls through to the overflow
//!   fallback. The `kind` attribute is low-cardinality (`"put"` or
//!   `"delete"`) so operators can distinguish a write-heavy tenant
//!   (Puts backing up to the backlog) from a tenant whose deletes are
//!   racing with indexing. A rising value is the earliest signal that
//!   embedder capacity needs to scale.
//!
//! The histogram and the embedder-load counter are singletons acquired from
//! the global meter on first use, mirroring the `HookMeters` / `ServerMeters`
//! pattern. The two observable gauges are owned by each [`BackgroundEmbedder`]
//! because their callbacks close over the embedder's per-tenant worker map
//! and the backing index.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

/// Histogram bucket bounds for `simulacra_memory_embed_lag_seconds`, in seconds.
/// Chosen to bracket the spec's p50 < 2s / p99 < 30s performance target while
/// leaving headroom above and below for out-of-SLO observations.
pub(crate) const EMBED_LAG_BUCKETS_SECONDS: &[f64] =
    &[0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0];

/// Lazily-initialised OTel instruments shared across the memory subsystem.
pub(crate) struct MemoryMeters {
    /// Wall-time lag from `MemoryEvent::Put.produced_at` to successful upsert.
    pub(crate) embed_lag_seconds: Histogram<f64>,
    /// Permanent embedder load failures at process startup.
    pub(crate) embedder_load_failures_total: Counter<u64>,
    /// Per-tenant-queue overflow events, keyed on event kind.
    pub(crate) overflow_total: Counter<u64>,
}

impl MemoryMeters {
    /// Return the process-wide singleton, initialising it on first call.
    /// Acquired from the global meter provider that is in place at first use.
    pub(crate) fn get() -> &'static Self {
        static METERS: OnceLock<MemoryMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-memory");
            MemoryMeters {
                embed_lag_seconds: meter
                    .f64_histogram("simulacra_memory_embed_lag_seconds")
                    .with_description(
                        "Background embedder lag from MemoryEvent::Put emission to VectorIndex::upsert completion, in seconds",
                    )
                    .with_unit("s")
                    .with_boundaries(EMBED_LAG_BUCKETS_SECONDS.to_vec())
                    .build(),
                embedder_load_failures_total: meter
                    .u64_counter("simulacra_memory_embedder_load_failures_total")
                    .with_description(
                        "Count of failed attempts to load the configured embedder at startup. A non-zero steady-state value indicates the memory subsystem is running without indexing.",
                    )
                    .with_unit("{failure}")
                    .build(),
                overflow_total: meter
                    .u64_counter("simulacra_memory_overflow_total")
                    .with_description(
                        "Count of background-embedder events that fell through the per-tenant queue overflow path, keyed by `kind` (put|delete).",
                    )
                    .with_unit("{event}")
                    .build(),
            }
        })
    }
}

/// Record a permanent embedder load failure. Intended to be called from
/// startup code paths that surface a user-visible error AND need the
/// failure to show up on alerting dashboards.
///
/// `reason` is a short, low-cardinality category (e.g. `"model_not_found"`,
/// `"dim_mismatch"`, `"io"`); free-form error messages should NOT be passed
/// here to avoid cardinality blow-up in the metrics backend.
pub fn record_embedder_load_failure(reason: &'static str) {
    MemoryMeters::get()
        .embedder_load_failures_total
        .add(1, &[KeyValue::new("reason", reason)]);
}

/// Record a queue-overflow fallback. Called from `BackgroundEmbedder`'s
/// `dispatch_event` when the per-tenant channel is saturated and an
/// event falls through to the durable-backlog-or-sync-delete path.
///
/// `kind` is a low-cardinality category (`"put"` or `"delete"`); callers
/// must not pass a tenant or path here to avoid series-count blow-up.
pub(crate) fn record_queue_overflow(kind: &'static str) {
    MemoryMeters::get()
        .overflow_total
        .add(1, &[KeyValue::new("kind", kind)]);
}
