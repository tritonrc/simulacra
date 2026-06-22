use std::sync::Arc;
use std::time::Duration;

use simulacra_memory::{MemoryStore, SqliteMemoryStore};
use simulacra_types::{MemoryCapability, MemoryPath, TenantId, VfsEvent, VirtualFs};
use simulacra_vfs::{MemoryFs, MemoryStoreFs, NotifyingFsLayer};
use tokio::time::timeout;

fn tenant_id(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

fn capability() -> MemoryCapability {
    MemoryCapability {
        enabled: true,
        search_scopes: vec![memory_path("/var/memory")],
        write_scopes: vec![memory_path("/var/memory")],
    }
}

#[tokio::test]
async fn same_path_writes_are_observed_in_order_modulo_skipped() {
    // Distinct payload lengths so a buggy reordering impl fails on len, not
    // just position.
    let layer = NotifyingFsLayer::for_tenant(
        tenant_id("tenant-a"),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    );
    let mut watcher = layer.subscribe("/");

    layer.write("/foo/bar.txt", b"A").unwrap();
    layer.write("/foo/bar.txt", b"BB").unwrap();
    layer.write("/foo/bar.txt", b"CCC").unwrap();

    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    let second = timeout(Duration::from_millis(50), watcher.recv()).await;
    let third = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(first, Ok(Some(VfsEvent::Written { len: 1, .. }))));
    assert!(matches!(second, Ok(Some(VfsEvent::Written { len: 2, .. }))));
    assert!(matches!(third, Ok(Some(VfsEvent::Written { len: 3, .. }))));
}

#[tokio::test]
async fn each_layer_in_a_stack_emits_one_event_per_write_to_its_own_subscriber() {
    // Per-subscriber emission (resolved decision #3): a write through a
    // `NotifyingFsLayer(NotifyingFsLayer(MemoryFs))` stack produces ONE event
    // per layer's broadcast — layers do NOT coordinate. A subscriber attached
    // to the outer layer sees exactly one event; a subscriber attached to the
    // inner layer also sees exactly one event. They are independent
    // broadcasts. The "stack-wide event count" is not a thing.
    let inner = Arc::new(NotifyingFsLayer::for_tenant(
        tenant_id("tenant-a"),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    )) as Arc<dyn VirtualFs>;
    let outer = NotifyingFsLayer::for_tenant(tenant_id("tenant-a"), Arc::clone(&inner));

    let mut outer_watcher = outer.subscribe("/");
    let mut inner_watcher = inner.subscribe("/");

    outer.write("/foo/bar.txt", b"hello").unwrap();

    let outer_event = timeout(Duration::from_millis(50), outer_watcher.recv()).await;
    let inner_event = timeout(Duration::from_millis(50), inner_watcher.recv()).await;

    // Both layers emit independently — each watcher receives its own event.
    assert!(matches!(
        outer_event,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/bar.txt")
    ));
    assert!(matches!(
        inner_event,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/bar.txt")
    ));

    // Each watcher receives exactly one event for one write — no duplicates
    // from the other layer's broadcast.
    let outer_again = timeout(Duration::from_millis(50), outer_watcher.recv()).await;
    let inner_again = timeout(Duration::from_millis(50), inner_watcher.recv()).await;
    assert!(
        outer_again.is_err(),
        "outer watcher leaked extra event: {outer_again:?}"
    );
    assert!(
        inner_again.is_err(),
        "inner watcher leaked extra event: {inner_again:?}"
    );
}

#[tokio::test]
async fn tenant_isolation_holds_across_memory_store_and_notifying_stack() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let tenant_a = Arc::new(NotifyingFsLayer::for_tenant(
        tenant_id("tenant-a"),
        Arc::new(MemoryStoreFs::new(
            MemoryFs::new(),
            tenant_id("tenant-a"),
            Arc::clone(&store),
            capability(),
        )) as Arc<dyn VirtualFs>,
    )) as Arc<dyn VirtualFs>;
    let tenant_b = Arc::new(NotifyingFsLayer::for_tenant(
        tenant_id("tenant-b"),
        Arc::new(MemoryStoreFs::new(
            MemoryFs::new(),
            tenant_id("tenant-b"),
            Arc::clone(&store),
            capability(),
        )) as Arc<dyn VirtualFs>,
    )) as Arc<dyn VirtualFs>;
    let mut watcher_a = tenant_a.subscribe("/var/memory");
    let mut watcher_b = tenant_b.subscribe("/var/memory");

    tenant_a.write("/var/memory/a.md", b"a").unwrap();
    tenant_b.write("/var/memory/b.md", b"b").unwrap();

    let event_a = timeout(Duration::from_millis(50), watcher_a.recv()).await;
    let event_b = timeout(Duration::from_millis(50), watcher_b.recv()).await;

    assert!(matches!(
        event_a,
        Ok(Some(VfsEvent::Written { tenant, path, .. }))
            if tenant == tenant_id("tenant-a") && path == std::path::Path::new("/var/memory/a.md")
    ));
    assert!(matches!(
        event_b,
        Ok(Some(VfsEvent::Written { tenant, path, .. }))
            if tenant == tenant_id("tenant-b") && path == std::path::Path::new("/var/memory/b.md")
    ));

    // Non-leakage: after each watcher has received its own tenant's event,
    // a follow-up recv with a small timeout must time out (no cross-tenant
    // event ever arrives).
    let leak_a = timeout(Duration::from_millis(50), watcher_a.recv()).await;
    let leak_b = timeout(Duration::from_millis(50), watcher_b.recv()).await;
    assert!(
        leak_a.is_err(),
        "tenant-a watcher received cross-tenant event: {leak_a:?}"
    );
    assert!(
        leak_b.is_err(),
        "tenant-b watcher received cross-tenant event: {leak_b:?}"
    );
}

#[tokio::test]
async fn watcher_returns_none_when_the_last_vfs_owner_is_dropped() {
    let mut watcher = {
        let vfs: Arc<dyn VirtualFs> = Arc::new(NotifyingFsLayer::for_tenant(
            tenant_id("tenant-a"),
            Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        ));
        let watcher = vfs.subscribe("/");
        drop(vfs);
        watcher
    };

    let received = timeout(Duration::from_millis(100), watcher.recv()).await;

    assert!(matches!(received, Ok(None)));
}
