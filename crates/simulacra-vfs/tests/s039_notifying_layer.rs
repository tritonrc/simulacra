use std::sync::Arc;
use std::time::Duration;

use simulacra_types::{FsMetadata, TenantId, VfsError, VfsEvent, VfsSnapshot, VirtualFs};
use simulacra_vfs::{MemoryFs, NotifyingFsLayer};
use tokio::time::timeout;

fn tenant_id() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

/// A `VirtualFs` whose every operation is configured to fail. Used to assert
/// that no event is published when the underlying VFS rejects the call.
struct FailingFs;

impl VirtualFs for FailingFs {
    fn read(&self, _path: &str) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn write(&self, _path: &str, _data: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn mkdir(&self, _path: &str) -> Result<(), VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn remove(&self, _path: &str) -> Result<(), VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn metadata(&self, _path: &str) -> Result<FsMetadata, VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        Err(VfsError::Io("failing fs".to_string()))
    }
}

fn notifying(inner: Arc<dyn VirtualFs>) -> NotifyingFsLayer {
    NotifyingFsLayer::for_tenant(tenant_id(), inner)
}

fn notifying_with_capacity(inner: Arc<dyn VirtualFs>, cap: usize) -> NotifyingFsLayer {
    NotifyingFsLayer::for_tenant_with_capacity(tenant_id(), inner, cap)
}

#[tokio::test]
async fn for_tenant_constructor_supports_default_and_custom_capacity() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let _custom = notifying_with_capacity(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>, 4);

    let mut watcher = layer.subscribe("/");
    // No writes yet — recv should pend (Err(Elapsed)) rather than yield None
    // (the broadcast still has a live sender attached to the layer).
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(received.is_err(), "expected recv to pend, got {received:?}");
}

#[tokio::test]
async fn successful_write_publishes_written_event_with_byte_len() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/");

    layer.write("/foo/bar.txt", b"hello").unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        received,
        Ok(Some(VfsEvent::Written {
            tenant,
            path,
            len: 5
        })) if tenant == tenant_id() && path == std::path::Path::new("/foo/bar.txt")
    ));
}

#[tokio::test]
async fn empty_payload_write_publishes_written_event_with_len_zero() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/");

    layer.write("/foo/empty.txt", b"").unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        received,
        Ok(Some(VfsEvent::Written {
            tenant,
            path,
            len: 0
        })) if tenant == tenant_id() && path == std::path::Path::new("/foo/empty.txt")
    ));
}

#[tokio::test]
async fn successful_remove_publishes_removed_event_after_written() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/");

    layer.write("/foo/bar.txt", b"hello").unwrap();
    layer.remove("/foo/bar.txt").unwrap();

    // First event must be Written — pinning the drain order avoids a buggy
    // impl that drops Written and only emits Removed from passing.
    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    let second = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(
        first,
        Ok(Some(VfsEvent::Written { tenant, path, len: 5 }))
            if tenant == tenant_id() && path == std::path::Path::new("/foo/bar.txt")
    ));
    assert!(matches!(
        second,
        Ok(Some(VfsEvent::Removed { tenant, path }))
            if tenant == tenant_id() && path == std::path::Path::new("/foo/bar.txt")
    ));
}

#[tokio::test]
async fn failing_write_publishes_no_event() {
    let layer = notifying(Arc::new(FailingFs) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/");

    assert!(layer.write("/foo/bar.txt", b"hello").is_err());
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(received.is_err(), "expected pending recv, got {received:?}");
}

#[tokio::test]
async fn failing_remove_publishes_no_event() {
    // Mirror of the failing-write test: a remove that fails at the inner VFS
    // must not produce a Removed event.
    let layer = notifying(Arc::new(FailingFs) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/");

    assert!(layer.remove("/foo/bar.txt").is_err());
    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(received.is_err(), "expected pending recv, got {received:?}");
}

#[tokio::test]
async fn notifying_layer_filters_events_by_prefix() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/foo");

    layer.write("/foo/bar.txt", b"foo").unwrap();
    layer.write("/baz/qux.txt", b"baz").unwrap();

    // First recv: must surface the matching /foo/bar.txt write.
    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        first,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/bar.txt")
    ));

    // Second recv: the /baz/qux.txt write must be silently dropped by the
    // prefix filter; no second event ever arrives.
    let second = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(
        second.is_err(),
        "non-matching prefix event leaked: {second:?}"
    );
}

#[tokio::test]
async fn dropping_one_watcher_mid_stream_does_not_break_other_subscribers() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher_a = layer.subscribe("/");
    let mut watcher_b = layer.subscribe("/");

    // Event 1 — both watchers see it.
    layer.write("/foo/one.txt", b"1").unwrap();
    let a1 = timeout(Duration::from_millis(50), watcher_a.recv()).await;
    let b1 = timeout(Duration::from_millis(50), watcher_b.recv()).await;
    assert!(matches!(
        a1,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/one.txt")
    ));
    assert!(matches!(
        b1,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/one.txt")
    ));

    // Drop watcher A mid-stream.
    drop(watcher_a);

    // Event 2 — surviving watcher B still receives it.
    layer.write("/foo/two.txt", b"22").unwrap();
    let b2 = timeout(Duration::from_millis(50), watcher_b.recv()).await;
    assert!(matches!(
        b2,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/two.txt")
    ));
}
