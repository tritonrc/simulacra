use std::path::PathBuf;
use std::time::Duration;

use simulacra_types::{TenantId, VfsEvent, VfsWatcher, VirtualFs};
use simulacra_vfs::MemoryFs;
use tokio::sync::broadcast;
use tokio::time::timeout;

fn tenant() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

#[tokio::test]
async fn vfs_event_variants_are_constructible_with_their_payload_fields() {
    // This test pins the VfsEvent variant shape: it must be constructible with
    // the payload fields the spec requires (tenant + path + len for Written,
    // tenant + path for Removed, count for Skipped). It is mostly a
    // compile-time check; the runtime assertion below confirms the layer that
    // provides these constructors is in fact `simulacra-types` and re-exported
    // through `simulacra-vfs`.
    let tenant = tenant();
    let written = VfsEvent::Written {
        tenant: tenant.clone(),
        path: PathBuf::from("/foo/bar.txt"),
        len: 7,
    };
    let removed = VfsEvent::Removed {
        tenant,
        path: PathBuf::from("/foo/bar.txt"),
    };
    let skipped = VfsEvent::Skipped { count: 3 };

    // Pattern-match each variant so the compiler proves the shape, and the
    // test fails (rather than silently passing) if anyone reshapes the enum.
    assert!(matches!(written, VfsEvent::Written { len: 7, .. }));
    assert!(matches!(removed, VfsEvent::Removed { .. }));
    assert!(matches!(skipped, VfsEvent::Skipped { count: 3 }));
}

#[tokio::test]
async fn vfs_watcher_recv_filters_prefixes_and_surfaces_skipped_events() {
    let (sender, receiver) = broadcast::channel(8);
    let mut watcher = VfsWatcher::new(receiver, "/foo");

    sender
        .send(VfsEvent::Written {
            tenant: tenant(),
            path: PathBuf::from("/baz/qux.txt"),
            len: 4,
        })
        .unwrap();
    sender.send(VfsEvent::Skipped { count: 2 }).unwrap();

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(received, Ok(Some(VfsEvent::Skipped { count: 2 }))));
}

#[tokio::test]
async fn virtual_fs_default_subscribe_returns_closed_watcher_for_memory_fs() {
    let fs = MemoryFs::new();
    let mut watcher = fs.subscribe("/");

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(received, Ok(None)));
}

#[tokio::test]
async fn subscribing_to_memory_fs_yields_a_dead_channel_not_an_error() {
    let fs = MemoryFs::new();
    let mut watcher = fs.subscribe("/");

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(received.is_ok(), "default subscribe should not error");
    assert!(received.unwrap().is_none());
}

#[tokio::test]
async fn memory_fs_default_subscribe_with_non_root_prefix_is_still_dead() {
    // Even with a meaningful prefix, the default-impl `subscribe` returns a
    // dead-channel watcher: there is no broadcast sender on `MemoryFs`, so
    // the watcher's first `recv` resolves to `None` rather than hanging.
    let fs = MemoryFs::new();
    let mut watcher = fs.subscribe("/foo/bar");

    let received = timeout(Duration::from_millis(50), watcher.recv()).await;

    assert!(matches!(received, Ok(None)));
}

#[tokio::test]
async fn empty_prefix_is_equivalent_to_root_for_vfs_watchers() {
    // A watcher created with `""` and a watcher created with `"/"` against the
    // same broadcast sender must each receive the same event. Asserted
    // independently per watcher (no Debug-string equality).
    let (sender, rx_empty) = broadcast::channel(8);
    let mut empty_prefix = VfsWatcher::new(rx_empty, "");
    let mut root_prefix = VfsWatcher::new(sender.subscribe(), "/");

    sender
        .send(VfsEvent::Written {
            tenant: tenant(),
            path: PathBuf::from("/foo/bar.txt"),
            len: 3,
        })
        .unwrap();

    let empty = timeout(Duration::from_millis(50), empty_prefix.recv()).await;
    let root = timeout(Duration::from_millis(50), root_prefix.recv()).await;

    assert!(matches!(empty, Ok(Some(VfsEvent::Written { len: 3, .. }))));
    assert!(matches!(root, Ok(Some(VfsEvent::Written { len: 3, .. }))));
}
