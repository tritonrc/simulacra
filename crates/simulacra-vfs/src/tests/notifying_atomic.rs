use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use simulacra_types::{
    FsMutation, Precondition, TenantId, VfsError, VfsEvent, VfsWatcher, VirtualFs,
};
use tokio::time::timeout;

use super::bounded::TruncatingFs;
use crate::{MemoryFs, NotifyingFsLayer, OverlayFs, ReadOnlyPathGuard};

fn tenant() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

fn layer_over(inner: Arc<dyn VirtualFs>) -> NotifyingFsLayer {
    NotifyingFsLayer::for_tenant(tenant(), inner)
}

fn written(path: &str, len: u64) -> VfsEvent {
    VfsEvent::Written {
        tenant: tenant(),
        path: PathBuf::from(path),
        len,
    }
}

fn removed(path: &str) -> VfsEvent {
    VfsEvent::Removed {
        tenant: tenant(),
        path: PathBuf::from(path),
    }
}

/// Everything the watcher receives until it has been quiet for 50ms.
async fn drain(watcher: &mut VfsWatcher) -> Vec<VfsEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_millis(50), watcher.recv()).await {
        events.push(event);
    }
    events
}

fn seeded() -> (Arc<MemoryFs>, NotifyingFsLayer) {
    let inner = Arc::new(MemoryFs::new());
    let layer = layer_over(inner.clone() as Arc<dyn VirtualFs>);
    (inner, layer)
}

#[tokio::test]
async fn forwarded_rename_publishes_removed_from_then_written_to() {
    let (inner, layer) = seeded();
    inner.write("/a", b"hello").unwrap();
    let mut watcher = layer.subscribe("/");

    layer.rename("/a", "/dir/b").unwrap();

    assert_eq!(inner.read("/dir/b").unwrap(), b"hello");
    assert!(!inner.exists("/a"));
    let events = drain(&mut watcher).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], removed("/a"));
    assert!(
        matches!(&events[1], VfsEvent::Written { tenant: t, path, .. }
            if *t == tenant() && path == std::path::Path::new("/dir/b")),
        "{events:?}"
    );
}

#[tokio::test]
async fn rename_conflict_surfaces_unchanged_and_publishes_nothing() {
    let (inner, layer) = seeded();
    inner.write("/a", b"1").unwrap();
    inner.write("/b", b"2").unwrap();
    let mut watcher = layer.subscribe("/");

    let err = layer.rename("/a", "/b").unwrap_err();

    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert_eq!(inner.read("/a").unwrap(), b"1");
    assert_eq!(inner.read("/b").unwrap(), b"2");
    assert!(drain(&mut watcher).await.is_empty());
}

#[tokio::test]
async fn rename_unsupported_below_publishes_nothing() {
    let inner: Arc<dyn VirtualFs> = Arc::new(OverlayFs::new(
        Box::new(MemoryFs::new()),
        Box::new(MemoryFs::new()),
    ));
    inner.write("/a", b"1").unwrap();
    let layer = layer_over(Arc::clone(&inner));
    let mut watcher = layer.subscribe("/");

    let err = layer.rename("/a", "/b").unwrap_err();

    assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");
    assert!(!inner.exists("/b"));
    assert!(drain(&mut watcher).await.is_empty());
}

#[tokio::test]
async fn forwarded_batch_publishes_one_event_per_affected_path() {
    let (inner, layer) = seeded();
    inner.write("/gone", b"g").unwrap();
    inner.write("/old", b"o").unwrap();
    let mut watcher = layer.subscribe("/");
    let batch = [
        FsMutation::Write {
            path: "/w".into(),
            data: b"abc".to_vec(),
            precondition: Precondition::MustBeAbsent,
        },
        FsMutation::Remove {
            path: "/gone".into(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Rename {
            from: "/old".into(),
            to: "/new".into(),
            precondition: Precondition::Unconditional,
        },
    ];

    layer.apply_batch(&batch).unwrap();

    assert_eq!(inner.read("/w").unwrap(), b"abc");
    assert_eq!(inner.read("/new").unwrap(), b"o");
    let events = drain(&mut watcher).await;
    assert_eq!(events.len(), 4, "one event per affected path: {events:?}");
    assert!(events.contains(&written("/w", 3)), "{events:?}");
    assert!(events.contains(&removed("/gone")), "{events:?}");
    assert!(events.contains(&removed("/old")), "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e,
            VfsEvent::Written { path, .. } if path == std::path::Path::new("/new"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn failed_batch_publishes_nothing_and_applies_nothing() {
    let (inner, layer) = seeded();
    inner.write("/taken", b"t").unwrap();
    let mut watcher = layer.subscribe("/");
    let batch = [
        FsMutation::Write {
            path: "/w".into(),
            data: vec![1],
            precondition: Precondition::Unconditional,
        },
        FsMutation::Write {
            path: "/taken".into(),
            data: vec![2],
            precondition: Precondition::MustBeAbsent,
        },
    ];

    let err = layer.apply_batch(&batch).unwrap_err();

    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert!(!inner.exists("/w"));
    assert_eq!(inner.read("/taken").unwrap(), b"t");
    assert!(drain(&mut watcher).await.is_empty());
}

#[tokio::test]
async fn forwarded_write_if_publishes_on_success_and_not_on_conflict() {
    let (inner, layer) = seeded();
    let mut watcher = layer.subscribe("/");

    layer
        .write_if("/a", b"hello", Precondition::MustBeAbsent)
        .unwrap();
    let err = layer
        .write_if("/a", b"again", Precondition::MustBeAbsent)
        .unwrap_err();

    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert_eq!(inner.read("/a").unwrap(), b"hello");
    assert_eq!(drain(&mut watcher).await, vec![written("/a", 5)]);
}

#[test]
fn notifying_layer_forwards_list_dir_bounded() {
    let inner = TruncatingFs::new();
    inner.write("/a", b"1").unwrap();
    let layer = layer_over(Arc::new(inner));
    let listing = layer.list_dir_bounded("/").unwrap();
    assert_eq!(listing.entries, vec!["a".to_string()]);
    assert!(listing.truncated);
}

#[tokio::test]
async fn watcher_subscribed_through_a_read_only_guard_receives_write_events() {
    let layer: Arc<dyn VirtualFs> = Arc::new(layer_over(Arc::new(MemoryFs::new())));
    let guard = ReadOnlyPathGuard::new(layer, "/skills");
    let mut watcher = guard.subscribe("/");

    guard.write("/a.txt", b"hello").unwrap();

    assert_eq!(drain(&mut watcher).await, vec![written("/a.txt", 5)]);
}

#[tokio::test]
async fn guard_over_notifying_layer_delivers_rename_and_batch_events() {
    let inner = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let layer: Arc<dyn VirtualFs> = Arc::new(layer_over(inner as Arc<dyn VirtualFs>));
    let guard = ReadOnlyPathGuard::new(layer, "/skills");
    let mut watcher = guard.subscribe("/");

    guard.rename("/a", "/b").unwrap();
    guard
        .apply_batch(&[FsMutation::Remove {
            path: "/b".into(),
            precondition: Precondition::Unconditional,
        }])
        .unwrap();

    let events = drain(&mut watcher).await;
    assert_eq!(events.first(), Some(&removed("/a")), "{events:?}");
    assert_eq!(events.last(), Some(&removed("/b")), "{events:?}");
    assert_eq!(events.len(), 3, "{events:?}");
}

#[tokio::test]
async fn guard_refusal_publishes_no_event() {
    let layer: Arc<dyn VirtualFs> = Arc::new(layer_over(Arc::new(MemoryFs::new())));
    layer.write("/skills/x.md", b"s").unwrap();
    let guard = ReadOnlyPathGuard::new(Arc::clone(&layer), "/skills");
    let mut watcher = guard.subscribe("/");

    assert!(matches!(
        guard.rename("/skills/x.md", "/y"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        guard.remove("/skills/x.md"),
        Err(VfsError::PermissionDenied(_))
    ));

    assert!(drain(&mut watcher).await.is_empty());
}
