//! An atomic call that removes or moves away an ancestor of the memory
//! prefix would take the memory subtree with it, so it is refused before
//! anything is forwarded. Calls that merely sit beside the prefix still pass.

use std::sync::Arc;

use simulacra_memory::{MemoryStore, SqliteMemoryStore};
use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};
use simulacra_vfs::{MemoryFs, MemoryStoreFs};

use super::{memory_capability, memory_path, tenant};

struct Fixture {
    _dir: tempfile::TempDir,
    inner: Arc<dyn VirtualFs>,
    fs: MemoryStoreFs<Arc<dyn VirtualFs>>,
}

/// An inner filesystem holding `/var/scratch`, and a durable note at
/// `/var/memory/self/note`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    store
        .put(&tenant(), &memory_path("/var/memory/self/note"), b"durable")
        .unwrap();
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    inner.write("/var/scratch", b"scratch").unwrap();
    let capability = memory_capability(&["/var/memory/self", "/mnt"], &["/var/memory/self"]);
    let fs = MemoryStoreFs::new(Arc::clone(&inner), tenant(), store, capability);
    Fixture {
        _dir: dir,
        inner,
        fs,
    }
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
        fx.inner.read("/var/scratch").unwrap(),
        b"scratch",
        "{what}: the inner tree is untouched"
    );
    assert!(!fx.inner.exists("/moved"), "{what}: nothing was moved");
    assert_eq!(
        fx.fs.read("/var/memory/self/note").unwrap(),
        b"durable",
        "{what}: the durable note is still where it was"
    );
}

#[test]
fn renaming_an_ancestor_of_the_memory_prefix_is_refused() {
    let fx = fixture();
    refused(fx.fs.rename("/var", "/moved"), "rename /var");
    assert_nothing_moved(&fx, "rename /var");
}

#[test]
fn renaming_an_ancestor_spelled_non_canonically_is_refused() {
    let fx = fixture();
    refused(fx.fs.rename("/x/../var", "/moved"), "rename /x/../var");
    refused(fx.fs.rename("//var/", "/moved"), "rename //var/");
    assert_nothing_moved(&fx, "non-canonical rename");
}

#[test]
fn renaming_the_root_is_refused() {
    let fx = fixture();
    refused(fx.fs.rename("/", "/moved"), "rename /");
    assert_nothing_moved(&fx, "rename /");
}

#[test]
fn a_batch_removing_an_ancestor_of_the_memory_prefix_is_refused_whole() {
    let fx = fixture();
    for ancestor in ["/var", "/", "/x/../var"] {
        let batch = [
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
        refused(fx.fs.apply_batch(&batch), &format!("remove {ancestor}"));
        assert!(!fx.inner.exists("/first"), "{ancestor}: nothing applied");
        assert_nothing_moved(&fx, ancestor);
    }
}

#[test]
fn a_batch_renaming_an_ancestor_of_the_memory_prefix_is_refused_whole() {
    let fx = fixture();
    for ancestor in ["/var", "/"] {
        let batch = [FsMutation::Rename {
            from: ancestor.into(),
            to: "/moved".into(),
            precondition: Precondition::Unconditional,
        }];
        refused(fx.fs.apply_batch(&batch), &format!("rename {ancestor}"));
        assert_nothing_moved(&fx, ancestor);
    }
}

#[test]
fn positive_control_calls_beside_the_memory_prefix_are_still_forwarded() {
    let fx = fixture();
    fx.fs.rename("/var/scratch", "/var/scratch2").unwrap();
    assert!(fx.inner.exists("/var/scratch2") && !fx.inner.exists("/var/scratch"));
    fx.fs
        .apply_batch(&[FsMutation::Remove {
            path: "/var/scratch2".into(),
            precondition: Precondition::Unconditional,
        }])
        .unwrap();
    assert!(!fx.inner.exists("/var/scratch2"));
    assert_eq!(fx.fs.read("/var/memory/self/note").unwrap(), b"durable");
}
