//! Fan-out of `MemoryStore` events as `VfsEvent`s for `MemoryStoreFs`
//! subscribers.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use simulacra_memory::MemoryEvent;
use simulacra_types::{TenantId, VfsEvent};
use tokio::sync::broadcast;

/// `simulacra.vfs.events` — counter incremented on every published `VfsEvent`,
/// keyed by `kind ∈ {written, removed, skipped}` and `layer` (this adapter
/// always tags `layer = "memory_store_fs"`).
fn vfs_events_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("simulacra-vfs")
            .u64_counter("simulacra.vfs.events")
            .with_description("VfsEvent publications, keyed by kind and layer")
            .build()
    })
}

fn record_event(kind: &'static str) {
    vfs_events_counter().add(
        1,
        &[
            KeyValue::new("kind", kind),
            KeyValue::new("layer", "memory_store_fs"),
        ],
    );
}

/// Translate one `MemoryEvent` into a `VfsEvent` and publish it on the
/// per-layer broadcast, filtering by the layer's bound tenant. The path-
/// prefix filter is intentionally NOT applied here — it lives on
/// `VfsWatcher::recv` so the watcher abstraction is uniform across all
/// `VirtualFs` impls.
pub(super) fn forward_memory_event(
    sender: &broadcast::Sender<VfsEvent>,
    bound_tenant: &TenantId,
    event: MemoryEvent,
) {
    match event {
        MemoryEvent::Put {
            tenant,
            path,
            bytes_len,
            ..
        } => {
            if &tenant != bound_tenant {
                return;
            }
            record_event("written");
            let _ = sender.send(VfsEvent::Written {
                tenant,
                path: std::path::PathBuf::from(path.as_str()),
                len: bytes_len,
            });
        }
        MemoryEvent::Delete { tenant, path, .. } => {
            if &tenant != bound_tenant {
                return;
            }
            record_event("removed");
            let _ = sender.send(VfsEvent::Removed {
                tenant,
                path: std::path::PathBuf::from(path.as_str()),
            });
        }
    }
}
