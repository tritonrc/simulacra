//! An atomic call that removes or moves away `/proc`, an ancestor of the
//! mailbox, would take the mailbox subtree with it, so `MailboxFs` refuses it
//! before forwarding, whatever sits above it in the stack.

use std::sync::Arc;

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};
use simulacra_vfs::MemoryFs;

use super::{RecordingArtifactStore, mailbox_fs};

struct Fixture {
    inner: Arc<dyn VirtualFs>,
    store: Arc<RecordingArtifactStore>,
    fs: simulacra_vfs::MailboxFs<Arc<dyn VirtualFs>>,
}

/// An inner filesystem holding `/proc/scratch`, and a stored artifact
/// `report.md` readable at `/proc/mailbox/report.md`.
fn fixture() -> Fixture {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/proc/scratch", b"scratch").unwrap();
    let store = Arc::new(RecordingArtifactStore::default());
    store.seed("tenant-a", "task-123", "report.md", b"durable");
    let fs = mailbox_fs(Arc::clone(&inner), store.clone());
    Fixture { inner, store, fs }
}

fn refused<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(
            result,
            Err(VfsError::Unsupported(_)) | Err(VfsError::PermissionDenied(_))
        ),
        "{what} must be refused before it is forwarded: {result:?}"
    );
}

fn assert_nothing_moved(fx: &Fixture, what: &str) {
    assert_eq!(
        fx.inner.read("/proc/scratch").unwrap(),
        b"scratch",
        "{what}: the inner tree is untouched"
    );
    assert!(!fx.inner.exists("/moved"), "{what}: nothing was moved");
    assert_eq!(
        fx.fs.read("/proc/mailbox/report.md").unwrap(),
        b"durable",
        "{what}: the artifact is still readable"
    );
    assert!(
        fx.store
            .stored("tenant-a", "task-123", "report.md")
            .is_some()
    );
}

#[test]
fn renaming_an_ancestor_of_the_mailbox_is_refused() {
    let fx = fixture();
    refused(fx.fs.rename("/proc", "/moved"), "rename /proc");
    refused(fx.fs.rename("/x/../proc", "/moved"), "rename /x/../proc");
    refused(fx.fs.rename("/", "/moved"), "rename /");
    assert_nothing_moved(&fx, "rename");
}

#[test]
fn a_batch_removing_or_renaming_an_ancestor_of_the_mailbox_is_refused_whole() {
    let fx = fixture();
    for ancestor in ["/proc", "/", "/x/../proc"] {
        let remove = [
            FsMutation::Write {
                path: "/first".into(),
                data: b"1".to_vec(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Remove {
                path: ancestor.into(),
                precondition: Precondition::Unconditional,
            },
        ];
        refused(fx.fs.apply_batch(&remove), &format!("remove {ancestor}"));
        assert!(!fx.inner.exists("/first"), "{ancestor}: nothing applied");
        let rename = [FsMutation::Rename {
            from: ancestor.into(),
            to: "/moved".into(),
            precondition: Precondition::Unconditional,
        }];
        refused(fx.fs.apply_batch(&rename), &format!("rename {ancestor}"));
        assert_nothing_moved(&fx, ancestor);
    }
}

#[test]
fn positive_control_calls_beside_the_mailbox_are_still_forwarded() {
    let fx = fixture();
    fx.fs.rename("/proc/scratch", "/proc/scratch2").unwrap();
    assert!(fx.inner.exists("/proc/scratch2") && !fx.inner.exists("/proc/scratch"));
    fx.fs
        .apply_batch(&[FsMutation::Remove {
            path: "/proc/scratch2".into(),
            precondition: Precondition::Unconditional,
        }])
        .unwrap();
    assert!(!fx.inner.exists("/proc/scratch2"));
    assert_eq!(fx.fs.read("/proc/mailbox/report.md").unwrap(), b"durable");
}
