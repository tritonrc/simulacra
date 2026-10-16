//! `AgentCellFsProxy`'s `write_if` and `apply_batch` forwards when the backend
//! answers `OutcomeUnknown`: the commit may still land, so the reservation
//! stays charged, and the error reaches the caller typed.

use crate::fs_proxy::AgentCellFsProxy;
use crate::test_support::CapturingJournal;
use simulacra_types::{
    AgentId, CapabilityToken, FsMetadata, FsMutation, JournalStorage, PathPattern, Precondition,
    ResourceBudget, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::sync::{Arc, Mutex};

/// A `MemoryFs` whose `write_if` and `apply_batch` answer `OutcomeUnknown`.
struct UnknownCommitFs(MemoryFs);

impl VirtualFs for UnknownCommitFs {
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
    fn write_if(&self, _path: &str, _data: &[u8], _pre: Precondition) -> Result<(), VfsError> {
        Err(VfsError::OutcomeUnknown("response lost".into()))
    }
    fn apply_batch(&self, _mutations: &[FsMutation]) -> Result<(), VfsError> {
        Err(VfsError::OutcomeUnknown("response lost".into()))
    }
}

fn proxy(max: u64, used: u64) -> (AgentCellFsProxy, Arc<Mutex<ResourceBudget>>) {
    let mut budget = ResourceBudget::new(0, 0, rust_decimal::Decimal::ZERO, 0);
    budget.max_vfs_bytes = max;
    budget.used_vfs_bytes = used;
    let budget = Arc::new(Mutex::new(budget));
    let journal: Arc<dyn JournalStorage> = Arc::new(CapturingJournal::default());
    let proxy = AgentCellFsProxy {
        vfs: Arc::new(UnknownCommitFs(MemoryFs::new())),
        capability: CapabilityToken {
            paths_read: vec![PathPattern("/ok/**".into())],
            paths_write: vec![PathPattern("/ok/**".into())],
            ..Default::default()
        },
        budget: Arc::clone(&budget),
        journal,
        agent_id: AgentId("proxy-unknown-test".into()),
    };
    (proxy, budget)
}

fn used(budget: &Arc<Mutex<ResourceBudget>>) -> u64 {
    budget.lock().unwrap().used_vfs_bytes
}

fn write_item(path: &str, data: &[u8]) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition: Precondition::Unconditional,
    }
}

#[test]
fn write_if_with_an_unknown_outcome_keeps_its_reservation_and_the_typed_error() {
    let (proxy, budget) = proxy(100, 3);

    let result = VirtualFs::write_if(&proxy, "/ok/a", b"1234", Precondition::Unconditional);

    assert!(
        matches!(&result, Err(VfsError::OutcomeUnknown(m)) if m == "response lost"),
        "{result:?}"
    );
    assert_eq!(used(&budget), 7, "3 + 4: the write may have landed");
}

#[test]
fn apply_batch_with_an_unknown_outcome_keeps_its_reservation_and_the_typed_error() {
    let (proxy, budget) = proxy(100, 0);

    let result = VirtualFs::apply_batch(
        &proxy,
        &[write_item("/ok/a", b"123"), write_item("/ok/b", b"4567")],
    );

    assert!(
        matches!(&result, Err(VfsError::OutcomeUnknown(m)) if m == "response lost"),
        "{result:?}"
    );
    assert_eq!(used(&budget), 7, "3 + 4: the batch may have landed");
}

#[test]
fn repeated_unknown_write_ifs_cannot_write_past_the_budget() {
    let (proxy, budget) = proxy(8, 0);

    for _ in 0..2 {
        let result = VirtualFs::write_if(&proxy, "/ok/a", b"1234", Precondition::Unconditional);
        assert!(matches!(result, Err(VfsError::OutcomeUnknown(_))));
    }
    let third = VirtualFs::write_if(&proxy, "/ok/a", b"1234", Precondition::Unconditional);

    assert!(
        matches!(&third, Err(e) if !matches!(e, VfsError::OutcomeUnknown(_))),
        "the third attempt must be refused by the budget before the backend is called: {third:?}"
    );
    assert_eq!(used(&budget), 8);
}

#[test]
fn positive_control_a_definite_failure_still_releases() {
    // A plain conflict answered by a backend that rejected the whole thing.
    struct ConflictFs(UnknownCommitFs);
    impl VirtualFs for ConflictFs {
        fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
            self.0.read(p)
        }
        fn write(&self, p: &str, d: &[u8]) -> Result<(), VfsError> {
            self.0.write(p, d)
        }
        fn exists(&self, p: &str) -> bool {
            self.0.exists(p)
        }
        fn list_dir(&self, p: &str) -> Result<Vec<String>, VfsError> {
            self.0.list_dir(p)
        }
        fn mkdir(&self, p: &str) -> Result<(), VfsError> {
            self.0.mkdir(p)
        }
        fn remove(&self, p: &str) -> Result<(), VfsError> {
            self.0.remove(p)
        }
        fn metadata(&self, p: &str) -> Result<FsMetadata, VfsError> {
            self.0.metadata(p)
        }
        fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
            self.0.snapshot()
        }
        fn restore(&self, s: &VfsSnapshot) -> Result<(), VfsError> {
            self.0.restore(s)
        }
        fn write_if(&self, p: &str, _d: &[u8], _pre: Precondition) -> Result<(), VfsError> {
            Err(VfsError::Conflict(p.to_string()))
        }
    }
    let (mut proxy, budget) = proxy(100, 3);
    proxy.vfs = Arc::new(ConflictFs(UnknownCommitFs(MemoryFs::new())));

    let result = VirtualFs::write_if(&proxy, "/ok/a", b"1234", Precondition::Unconditional);

    assert!(matches!(result, Err(VfsError::Conflict(_))));
    assert_eq!(used(&budget), 3, "a definite failure changed nothing");
}
