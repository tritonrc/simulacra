//! The atomic calls of `NotifyingFsLayer` publish the path the layers below
//! act on, not the spelling the caller chose: a watcher on `/artifacts` hears
//! a write that landed there even when it was spelled `/scratch/../artifacts/a`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use simulacra_types::{FsMutation, Precondition, TenantId, VfsEvent, VfsWatcher, VirtualFs};
use simulacra_vfs::{MemoryFs, NotifyingFsLayer};
use tokio::time::timeout;

fn tenant() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

fn layer() -> (Arc<MemoryFs>, NotifyingFsLayer) {
    let inner = Arc::new(MemoryFs::new());
    let layer = NotifyingFsLayer::for_tenant(tenant(), inner.clone() as Arc<dyn VirtualFs>);
    (inner, layer)
}

async fn drain(watcher: &mut VfsWatcher) -> Vec<VfsEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_millis(50), watcher.recv()).await {
        events.push(event);
    }
    events
}

fn paths(events: &[VfsEvent]) -> Vec<PathBuf> {
    events
        .iter()
        .filter_map(|event| match event {
            VfsEvent::Written { path, .. } | VfsEvent::Removed { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn write_if_on_a_non_canonical_path_publishes_the_normalized_path() {
    let (inner, layer) = layer();
    let mut watcher = layer.subscribe("/artifacts");

    layer
        .write_if(
            "/scratch/../artifacts/a",
            b"hello",
            Precondition::Unconditional,
        )
        .unwrap();

    assert_eq!(
        inner.read("/artifacts/a").unwrap(),
        b"hello",
        "positive control: the write landed at the normalized path"
    );
    let events = drain(&mut watcher).await;
    assert_eq!(
        paths(&events),
        vec![PathBuf::from("/artifacts/a")],
        "{events:?}"
    );
}

#[tokio::test]
async fn rename_on_non_canonical_paths_publishes_the_normalized_paths() {
    let (inner, layer) = layer();
    inner.write("/artifacts/old", b"hello").unwrap();
    let mut watcher = layer.subscribe("/artifacts");

    layer
        .rename("/scratch/../artifacts/old", "/x/../artifacts/new")
        .unwrap();

    assert_eq!(inner.read("/artifacts/new").unwrap(), b"hello");
    let events = drain(&mut watcher).await;
    assert_eq!(
        paths(&events),
        vec![
            PathBuf::from("/artifacts/old"),
            PathBuf::from("/artifacts/new")
        ],
        "{events:?}"
    );
}

#[tokio::test]
async fn apply_batch_on_non_canonical_paths_publishes_the_normalized_paths() {
    let (inner, layer) = layer();
    inner.write("/artifacts/gone", b"x").unwrap();
    inner.write("/artifacts/old", b"hello").unwrap();
    let mut watcher = layer.subscribe("/artifacts");

    layer
        .apply_batch(&[
            FsMutation::Write {
                path: "/scratch/../artifacts/a".into(),
                data: b"12345".to_vec(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Remove {
                path: "//artifacts/./gone".into(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Rename {
                from: "/artifacts/old".into(),
                to: "/y/../artifacts/new".into(),
                precondition: Precondition::Unconditional,
            },
        ])
        .unwrap();

    assert_eq!(inner.read("/artifacts/a").unwrap(), b"12345");
    let events = drain(&mut watcher).await;
    assert_eq!(
        paths(&events),
        vec![
            PathBuf::from("/artifacts/a"),
            PathBuf::from("/artifacts/gone"),
            PathBuf::from("/artifacts/old"),
            PathBuf::from("/artifacts/new"),
        ],
        "{events:?}"
    );
}
