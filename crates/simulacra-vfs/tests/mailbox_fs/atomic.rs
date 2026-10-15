use std::sync::Arc;
use std::time::Duration;

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, TenantId, VfsError, VfsEvent, VfsSnapshot,
    VfsWatcher, VirtualFs,
};
use simulacra_vfs::{MailboxFs, MemoryFs, NotifyingFsLayer};

use super::{RecordingArtifactStore, composed_procfs, mailbox_fs};

/// A `MemoryFs` whose bounded listings always report truncation.
struct TruncatingFs(MemoryFs);

impl VirtualFs for TruncatingFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.0.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.0.write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        self.0.exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.0.list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.0.mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.0.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.0.metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.0.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.0.restore(snapshot)
    }
    fn write_if(&self, path: &str, data: &[u8], pre: Precondition) -> Result<(), VfsError> {
        self.0.write_if(path, data, pre)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.0.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.0.apply_batch(mutations)
    }
    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        Ok(DirListing {
            entries: self.0.list_dir(path)?,
            truncated: true,
        })
    }
}

fn write_item(path: &str, data: &[u8], precondition: Precondition) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition,
    }
}

#[test]
fn mailbox_answers_unsupported_for_a_rename_into_the_mailbox_and_changes_nothing() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(Arc::clone(&inner), store.clone());
    assert!(matches!(
        fs.rename("/a", "/proc/mailbox/out.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(inner.exists("/a"));
    assert!(!inner.exists("/proc/mailbox/out.md"));
    assert!(store.stored("tenant-a", "task-123", "out.md").is_none());
}

#[test]
fn mailbox_forwards_a_rename_outside_the_mailbox() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let fs = mailbox_fs(
        Arc::clone(&inner),
        Arc::new(RecordingArtifactStore::default()),
    );
    fs.rename("/a", "/b").unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");
    assert!(!inner.exists("/a"));
}

#[test]
fn mailbox_answers_unsupported_for_a_rename_out_of_the_mailbox() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(Arc::clone(&inner), store.clone());
    fs.write("/proc/mailbox/report.md", b"body").unwrap();
    assert!(matches!(
        fs.rename("/proc/mailbox/report.md", "/copy.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(!inner.exists("/copy.md"));
    assert_eq!(
        store.stored("tenant-a", "task-123", "report.md").unwrap(),
        b"body"
    );
    assert_eq!(fs.read("/proc/mailbox/report.md").unwrap(), b"body");
}

#[test]
fn mailbox_answers_unsupported_for_write_if_into_the_mailbox_and_changes_nothing() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(Arc::clone(&inner), store.clone());
    for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
        let err = fs
            .write_if("/proc/mailbox/x.md", b"1", precondition)
            .unwrap_err();
        assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");
    }
    assert!(store.stored("tenant-a", "task-123", "x.md").is_none());
    assert!(!inner.exists("/proc/mailbox/x.md"));
}

#[test]
fn mailbox_answers_unsupported_for_a_batch_whose_last_item_alone_touches_the_mailbox() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = mailbox_fs(Arc::clone(&inner), store.clone());
    let batch = [
        write_item("/one", &[1], Precondition::Unconditional),
        FsMutation::Rename {
            from: "/a".into(),
            to: "/b".into(),
            precondition: Precondition::Unconditional,
        },
        write_item("/proc/mailbox/x.md", &[2], Precondition::Unconditional),
    ];
    assert!(matches!(
        fs.apply_batch(&batch),
        Err(VfsError::Unsupported(_))
    ));
    assert!(!inner.exists("/one") && !inner.exists("/b"));
    assert_eq!(inner.read("/a").unwrap(), b"1");
    assert!(store.stored("tenant-a", "task-123", "x.md").is_none());
}

#[test]
fn mailbox_forwards_write_if_and_batches_with_their_atomicity() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let fs = mailbox_fs(
        Arc::clone(&inner),
        Arc::new(RecordingArtifactStore::default()),
    );
    fs.write("/taken", b"t").unwrap();

    let err = fs
        .write_if("/taken", b"x", Precondition::MustBeAbsent)
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    fs.write_if("/fresh", b"f", Precondition::MustBeAbsent)
        .unwrap();
    assert_eq!(inner.read("/fresh").unwrap(), b"f");

    let failing = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/taken", &[2], Precondition::MustBeAbsent),
    ];
    assert!(matches!(
        fs.apply_batch(&failing),
        Err(VfsError::Conflict(_))
    ));
    assert!(!inner.exists("/ok"));
    assert_eq!(inner.read("/taken").unwrap(), b"t");

    fs.apply_batch(&[write_item("/ok", &[1], Precondition::MustBeAbsent)])
        .unwrap();
    assert_eq!(inner.read("/ok").unwrap(), vec![1]);
}

#[test]
fn mailbox_lists_its_own_directory_untruncated_and_forwards_other_bounds() {
    let inner: Arc<dyn VirtualFs> = Arc::new(TruncatingFs(MemoryFs::new()));
    let fs = mailbox_fs(
        Arc::clone(&inner),
        Arc::new(RecordingArtifactStore::default()),
    );
    fs.write("/proc/mailbox/one.md", b"1").unwrap();
    fs.write("/proc/mailbox/two.md", b"2").unwrap();
    let mailbox = fs.list_dir_bounded("/proc/mailbox").unwrap();
    assert_eq!(mailbox.entries, fs.list_dir("/proc/mailbox").unwrap());
    assert!(!mailbox.entries.is_empty());
    assert!(
        !mailbox.truncated,
        "the mailbox listing comes from the store"
    );

    fs.write("/work/a", b"1").unwrap();
    let work = fs.list_dir_bounded("/work").unwrap();
    assert_eq!(work.entries, vec!["a".to_string()]);
    assert!(work.truncated);
}

#[tokio::test]
async fn mailbox_forwards_subscribe_to_its_inner_filesystem() {
    let tenant = TenantId::parse("tenant-a").unwrap();
    let layer = NotifyingFsLayer::for_tenant(
        tenant.clone(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    );
    let fs = MailboxFs::new(
        layer,
        "task-123".to_string(),
        "tenant-a".to_string(),
        Arc::new(RecordingArtifactStore::default()),
    );
    let mut watcher: VfsWatcher = fs.subscribe("/");

    fs.write("/a.txt", b"hello").unwrap();

    let event = tokio::time::timeout(Duration::from_millis(100), watcher.recv())
        .await
        .expect("a write through MailboxFs must reach a subscriber");
    assert!(
        matches!(&event, Some(VfsEvent::Written { tenant: t, path, len: 5 })
            if *t == tenant && path == std::path::Path::new("/a.txt")),
        "{event:?}"
    );
}

#[test]
fn composed_stack_reaches_the_mailbox_through_procfs_and_gets_unsupported_not_denied() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let store = Arc::new(RecordingArtifactStore::default());
    let fs = composed_procfs(Arc::clone(&inner), store.clone());

    assert!(matches!(
        fs.rename("/a", "/proc/mailbox/out.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        fs.write_if("/proc/mailbox/x.md", b"1", Precondition::MustBeAbsent),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        fs.apply_batch(&[write_item(
            "/proc/mailbox/y.md",
            &[1],
            Precondition::Unconditional
        )]),
        Err(VfsError::Unsupported(_))
    ));
    // The rest of /proc is read-only and says so.
    assert!(matches!(
        fs.rename("/a", "/proc/agent/id"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        fs.rename("/a", "/svc/toy-saas/x"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert_eq!(inner.read("/a").unwrap(), b"1");
    assert!(store.stored("tenant-a", "task-123", "out.md").is_none());
}

#[test]
fn composed_stack_forwards_atomic_operations_outside_every_owned_prefix() {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/a", b"1").unwrap();
    let fs = composed_procfs(
        Arc::clone(&inner),
        Arc::new(RecordingArtifactStore::default()),
    );

    fs.rename("/a", "/b").unwrap();
    fs.write_if("/c", b"3", Precondition::MustBeAbsent).unwrap();
    assert!(matches!(
        fs.write_if("/c", b"4", Precondition::MustBeAbsent),
        Err(VfsError::Conflict(_))
    ));
    let failing = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/c", &[2], Precondition::MustBeAbsent),
    ];
    assert!(matches!(
        fs.apply_batch(&failing),
        Err(VfsError::Conflict(_))
    ));

    assert_eq!(inner.read("/b").unwrap(), b"1");
    assert_eq!(inner.read("/c").unwrap(), b"3");
    assert!(!inner.exists("/ok") && !inner.exists("/a"));
}
