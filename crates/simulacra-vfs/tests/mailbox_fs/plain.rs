//! MailboxFs answers the plain calls for `/proc/mailbox` by the normalized
//! path, so a non-canonical spelling is routed as the canonical one is.

use std::sync::Arc;

use simulacra_types::{ArtifactStore, VfsError, VirtualFs};
use simulacra_vfs::{MailboxFs, MemoryFs};

use super::{RecordingArtifactStore, mailbox_fs};

const CANONICAL: &str = "/proc/mailbox/reports/a.md";

/// Spellings of the absolute path `canonical` that are not the canonical one.
fn spellings(canonical: &str) -> Vec<String> {
    let (first, tail) = canonical[1..].split_once('/').unwrap();
    vec![
        format!("/x/..{canonical}"),
        format!("/{canonical}"),
        format!("/{first}/./{tail}"),
        format!("{canonical}/../{}", canonical.rsplit('/').next().unwrap()),
    ]
}

fn stored_names(store: &RecordingArtifactStore) -> Vec<String> {
    store
        .list("tenant-a", "task-123")
        .unwrap()
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

fn fixture() -> (
    Arc<MemoryFs>,
    Arc<RecordingArtifactStore>,
    MailboxFs<Arc<dyn VirtualFs>>,
) {
    let inner = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        Arc::clone(&store) as Arc<dyn ArtifactStore>,
    );
    (inner, store, fs)
}

#[test]
fn mailbox_routes_write_on_a_non_canonical_path_to_the_store_like_the_canonical_one() {
    let (inner, store, fs) = fixture();
    fs.write(CANONICAL, b"v").unwrap();
    let canonical_inner = inner.read(CANONICAL).ok();
    let canonical_entries = stored_names(&store);
    assert_eq!(
        store
            .stored("tenant-a", "task-123", "reports/a.md")
            .unwrap(),
        b"v"
    );

    for path in spellings(CANONICAL) {
        let (inner, store, fs) = fixture();
        fs.write(&path, b"v").unwrap();
        assert_eq!(
            store.stored("tenant-a", "task-123", "reports/a.md"),
            Some(b"v".to_vec()),
            "{path} must reach the store at its normalized name"
        );
        assert_eq!(
            stored_names(&store),
            canonical_entries,
            "{path} must store nothing else"
        );
        assert_eq!(inner.read(CANONICAL).ok(), canonical_inner, "{path}");
    }
}

#[test]
fn mailbox_answers_mkdir_on_a_non_canonical_path_without_touching_the_inner_filesystem() {
    let (inner, _store, fs) = fixture();
    fs.mkdir("/proc/mailbox/d").unwrap();
    assert!(!inner.exists("/proc/mailbox/d"), "canonical baseline");
    for path in spellings("/proc/mailbox/d/e") {
        let (inner, _store, fs) = fixture();
        fs.mkdir(&path).unwrap();
        assert!(
            !inner.exists("/proc/mailbox/d/e") && !inner.exists("/proc/mailbox/d"),
            "{path} must not create a directory in the inner filesystem"
        );
    }
}

#[test]
fn mailbox_denies_remove_on_a_non_canonical_path_and_removes_nothing() {
    let (_inner, _store, fs) = fixture();
    assert!(matches!(
        fs.remove(CANONICAL),
        Err(VfsError::PermissionDenied(_))
    ));
    for path in spellings(CANONICAL) {
        let (inner, store, fs) = fixture();
        store.seed("tenant-a", "task-123", "reports/a.md", b"stored");
        inner.write(CANONICAL, b"inner").unwrap();
        let result = fs.remove(&path);
        assert!(
            matches!(result, Err(VfsError::PermissionDenied(_))),
            "{path}: {result:?}"
        );
        assert_eq!(inner.read(CANONICAL).unwrap(), b"inner", "{path}");
        assert_eq!(
            store
                .stored("tenant-a", "task-123", "reports/a.md")
                .unwrap(),
            b"stored",
            "{path}"
        );
    }
}

#[test]
fn mailbox_reads_a_non_canonical_path_as_the_canonical_one() {
    let (_inner, store, fs) = fixture();
    store.seed("tenant-a", "task-123", "reports/a.md", b"stored");
    assert_eq!(fs.read(CANONICAL).unwrap(), b"stored");
    assert!(fs.exists(CANONICAL));
    let canonical_meta = format!("{:?}", fs.metadata(CANONICAL).unwrap());
    assert_eq!(
        fs.list_dir("/proc/mailbox/reports").unwrap(),
        vec!["a.md".to_string()]
    );

    for path in spellings(CANONICAL) {
        assert_eq!(fs.read(&path).unwrap(), b"stored", "read {path}");
        assert!(fs.exists(&path), "exists {path}");
        assert_eq!(
            format!("{:?}", fs.metadata(&path).unwrap()),
            canonical_meta,
            "metadata {path}"
        );
    }
    for path in spellings("/proc/mailbox/reports") {
        assert_eq!(
            fs.list_dir(&path).unwrap(),
            vec!["a.md".to_string()],
            "list_dir {path}"
        );
    }
}
