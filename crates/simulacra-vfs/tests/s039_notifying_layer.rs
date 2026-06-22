use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use simulacra_types::{FsMetadata, TenantId, VfsError, VfsEvent, VfsSnapshot, VirtualFs};
use simulacra_vfs::{MemoryFs, NotifyingFsLayer};
use tokio::time::{sleep, timeout};

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

#[tokio::test]
async fn notifying_layer_drops_slow_consumer_with_skipped_sentinel() {
    // Capacity 2 + 5 writes without consuming → exactly 3 events drop.
    // Tokio's broadcast keeps the LAST `capacity` items, so survivors are
    // /foo/3.txt and /foo/4.txt — pinned in order.
    let layer = notifying_with_capacity(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>, 2);
    let mut watcher = layer.subscribe("/");

    for idx in 0..5 {
        layer
            .write(&format!("/foo/{idx}.txt"), idx.to_string().as_bytes())
            .unwrap();
    }

    // First event surfaces a Skipped sentinel.
    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    let skipped_count = match first {
        Ok(Some(VfsEvent::Skipped { count })) => count,
        other => panic!("expected Skipped sentinel as first event, got {other:?}"),
    };
    assert_eq!(
        skipped_count, 3,
        "5 writes - capacity 2 = 3 dropped events, got {skipped_count}"
    );

    // Drain the surviving Written events: must be exactly /foo/3.txt then
    // /foo/4.txt, in order.
    let mut survived: Vec<PathBuf> = Vec::new();
    for _ in 0..5 {
        match timeout(Duration::from_millis(25), watcher.recv()).await {
            Ok(Some(VfsEvent::Written { path, .. })) => survived.push(path),
            Ok(Some(VfsEvent::Skipped { .. })) => {
                panic!("only one Skipped sentinel expected before survivors")
            }
            Ok(Some(VfsEvent::Removed { .. })) => panic!("no Removed events expected"),
            Ok(None) => panic!("broadcast closed unexpectedly"),
            Err(_) => break,
        }
    }
    assert_eq!(
        survived,
        vec![PathBuf::from("/foo/3.txt"), PathBuf::from("/foo/4.txt"),],
        "expected the LAST {cap} writes to survive in order, got {survived:?}",
        cap = 2
    );
}

#[tokio::test]
async fn notifying_layer_writers_do_not_block_when_ring_is_full() {
    let layer = notifying_with_capacity(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>, 1);
    let _watcher = layer.subscribe("/");
    let start = Instant::now();

    for idx in 0..1000 {
        layer
            .write(&format!("/foo/{idx}.txt"), idx.to_string().as_bytes())
            .unwrap();
    }

    let elapsed = start.elapsed();
    sleep(Duration::from_millis(50)).await;

    assert!(
        elapsed < Duration::from_secs(1),
        "1000 writes should complete in < 1s; took {elapsed:?}"
    );
}

#[tokio::test]
async fn unmatched_prefix_returns_no_events_while_other_watchers_stay_alive() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut unmatched = layer.subscribe("/nope");
    let mut matched = layer.subscribe("/foo");

    layer.write("/foo/bar.txt", b"ok").unwrap();

    let unmatched_result = timeout(Duration::from_millis(50), unmatched.recv()).await;
    let matched_result = timeout(Duration::from_millis(50), matched.recv()).await;

    assert!(
        unmatched_result.is_err(),
        "unmatched-prefix watcher leaked: {unmatched_result:?}"
    );
    assert!(matches!(matched_result, Ok(Some(VfsEvent::Written { .. }))));
}

#[tokio::test]
async fn capacity_at_ring_limit_does_not_emit_skipped_sentinel() {
    // With capacity exactly equal to the number of unconsumed writes, every
    // event must be deliverable; no Skipped sentinel should appear.
    let layer = notifying_with_capacity(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>, 4);
    let mut watcher = layer.subscribe("/");

    for idx in 0..4 {
        layer.write(&format!("/foo/{idx}.txt"), b"x").unwrap();
    }

    for _ in 0..4 {
        let received = timeout(Duration::from_millis(50), watcher.recv()).await;
        match received {
            Ok(Some(VfsEvent::Written { .. })) => {}
            Ok(Some(VfsEvent::Skipped { .. })) => {
                panic!("Skipped sentinel emitted when capacity exactly accommodated all writes")
            }
            other => panic!("unexpected recv outcome: {other:?}"),
        }
    }
}

#[tokio::test]
async fn watcher_subscribed_after_writes_only_sees_post_subscription_events() {
    // Broadcast semantics: a watcher that subscribes AFTER prior writes have
    // landed must not receive those prior writes.
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);

    layer.write("/foo/before.txt", b"early").unwrap();
    let mut late_watcher = layer.subscribe("/");

    let pre_subscribe = timeout(Duration::from_millis(50), late_watcher.recv()).await;
    assert!(
        pre_subscribe.is_err(),
        "late watcher saw pre-subscription event: {pre_subscribe:?}"
    );

    layer.write("/foo/after.txt", b"late").unwrap();
    let post_subscribe = timeout(Duration::from_millis(50), late_watcher.recv()).await;
    assert!(matches!(
        post_subscribe,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/after.txt")
    ));
}

#[tokio::test]
async fn one_writer_two_watchers_each_watcher_sees_every_event() {
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut a = layer.subscribe("/");
    let mut b = layer.subscribe("/");

    layer.write("/foo/1.txt", b"x").unwrap();
    layer.write("/foo/2.txt", b"yy").unwrap();

    let a1 = timeout(Duration::from_millis(50), a.recv()).await;
    let a2 = timeout(Duration::from_millis(50), a.recv()).await;
    let b1 = timeout(Duration::from_millis(50), b.recv()).await;
    let b2 = timeout(Duration::from_millis(50), b.recv()).await;

    assert!(matches!(
        a1,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/1.txt")
    ));
    assert!(matches!(
        a2,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/2.txt")
    ));
    assert!(matches!(
        b1,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/1.txt")
    ));
    assert!(matches!(
        b2,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/2.txt")
    ));
}

#[tokio::test]
async fn two_writers_one_watcher_total_event_count_matches_total_writes() {
    // Two concurrent writer tasks issue 5 writes each. A single watcher must
    // observe exactly 10 events (no event loss when below capacity).
    let layer = Arc::new(notifying_with_capacity(
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        64,
    ));
    let mut watcher = layer.subscribe("/");

    let layer_a = Arc::clone(&layer);
    let task_a = tokio::spawn(async move {
        for idx in 0..5 {
            layer_a.write(&format!("/a/{idx}.txt"), b"a").unwrap();
        }
    });
    let layer_b = Arc::clone(&layer);
    let task_b = tokio::spawn(async move {
        for idx in 0..5 {
            layer_b.write(&format!("/b/{idx}.txt"), b"b").unwrap();
        }
    });

    task_a.await.unwrap();
    task_b.await.unwrap();

    let mut count = 0usize;
    while let Ok(Some(_event)) = timeout(Duration::from_millis(50), watcher.recv()).await {
        count += 1;
        if count >= 10 {
            break;
        }
    }
    assert_eq!(
        count, 10,
        "expected 10 events from two writers issuing 5 writes each, got {count}"
    );
}

#[tokio::test]
async fn watcher_path_equal_to_prefix_receives_the_event() {
    // Edge case: subscribe to "/foo" and write to exactly "/foo" (no trailing
    // slash, no child segment). The prefix filter must accept this — a path
    // that equals the prefix is "under" the prefix.
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/foo");

    layer.write("/foo", b"root").unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        received,
        Ok(Some(VfsEvent::Written { path, len: 4, .. })) if path == std::path::Path::new("/foo")
    ));
}

#[tokio::test]
async fn watcher_on_foo_does_not_receive_writes_to_foobar() {
    // Segment-aware prefix matching: subscribe("/foo") matches /foo and
    // /foo/<anything>, but NOT /foobar (which is a sibling that happens to
    // share a byte prefix). Pre-fix behavior would leak the event.
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/foo");

    layer.write("/foo/bar.txt", b"under-foo").unwrap();
    layer.write("/foobar/leak.txt", b"sibling").unwrap();

    // The /foo/bar.txt event surfaces.
    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        first,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/bar.txt")
    ));

    // The /foobar/leak.txt event must NOT surface — `/foobar` is not under
    // `/foo`. recv must time out.
    let second = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(
        second.is_err(),
        "watcher on /foo leaked /foobar event: {second:?}"
    );
}

#[tokio::test]
async fn watcher_on_foo_with_trailing_slash_normalizes_to_foo() {
    // Trailing-slash form of the prefix is normalized to the same matching
    // semantics as the no-slash form: /foo and /foo/* match; /foobar does not.
    let layer = notifying(Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>);
    let mut watcher = layer.subscribe("/foo/");

    layer.write("/foo/bar.txt", b"x").unwrap();
    layer.write("/foobar/leak.txt", b"y").unwrap();

    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(matches!(
        first,
        Ok(Some(VfsEvent::Written { path, .. })) if path == std::path::Path::new("/foo/bar.txt")
    ));
    let second = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(
        second.is_err(),
        "trailing-slash prefix leaked sibling event: {second:?}"
    );
}

#[tokio::test]
async fn for_tenant_with_capacity_constructs_layer_at_explicit_capacity() {
    // Pin the post-refactor surface: capacity is set ONCE at construction via
    // `for_tenant_with_capacity`. There is no chainable builder anymore.
    let layer = NotifyingFsLayer::for_tenant_with_capacity(
        tenant_id(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        2,
    );
    let mut watcher = layer.subscribe("/");

    // Three writes against capacity-2: one event drops, surfaces as Skipped.
    for idx in 0..3 {
        layer.write(&format!("/foo/{idx}.txt"), b"x").unwrap();
    }

    let first = timeout(Duration::from_millis(50), watcher.recv()).await;
    assert!(
        matches!(first, Ok(Some(VfsEvent::Skipped { count: 1 }))),
        "expected Skipped(1) when 3 writes hit a capacity-2 ring, got {first:?}"
    );
}
