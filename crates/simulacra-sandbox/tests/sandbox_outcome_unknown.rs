//! A write whose outcome is unknown may still land, so the byte reservation
//! stays charged on every write path, and no compensating rollback runs
//! against a write that may still be pending.
//!
//! Covered here: JS `appendFileSync` (conditional write), the plain
//! `AgentCell::write_file`, and the item-by-item fallback of
//! `apply_vfs_mutations` after the backend answers `Unsupported` to
//! `apply_batch`.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_sandbox::{SandboxError, VfsMutation, VfsWritePrecondition};
use simulacra_types::{FsMutation, Precondition};
use std::sync::atomic::Ordering::SeqCst;

/// A `MemoryFs` with scripted failures. `write_if` always answers
/// `OutcomeUnknown`; a plain `write` of `unknown_write_path` answers
/// `OutcomeUnknown` without applying anything. `apply_batch` is left to the
/// trait default, which is `Unsupported`.
struct UnknownFs {
    inner: Arc<MemoryFs>,
    unknown_write_path: &'static str,
    writes: AtomicUsize,
    removes: AtomicUsize,
}

impl UnknownFs {
    fn new(unknown_write_path: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryFs::new()),
            unknown_write_path,
            writes: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
        })
    }
}

impl VirtualFs for UnknownFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes.fetch_add(1, SeqCst);
        if path == self.unknown_write_path {
            return Err(VfsError::OutcomeUnknown("response lost".to_owned()));
        }
        self.inner.write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.removes.fetch_add(1, SeqCst);
        self.inner.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
    fn write_if(&self, _path: &str, _data: &[u8], _pre: Precondition) -> Result<(), VfsError> {
        Err(VfsError::OutcomeUnknown("response lost".to_owned()))
    }
}

fn cell_over(fs: Arc<UnknownFs>, budget: Arc<Mutex<ResourceBudget>>) -> AgentCell {
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(
        fs,
        capability(&["/**"], &["/**"], false, true),
        budget,
        journal,
        http,
    )
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

#[test]
fn js_append_with_an_unknown_outcome_keeps_its_bytes_charged() {
    let fs = UnknownFs::new("/never");
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cell = cell_over(fs.clone(), Arc::clone(&budget));
    fs.inner.write("/a", b"old").unwrap();

    let result = cell.execute_js("fs.appendFileSync('/a', 'xy')");

    assert!(result.is_err(), "the append's outcome is unknown");
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        2,
        "the append may have landed, so its bytes stay charged"
    );
}

#[test]
fn js_append_with_an_unknown_outcome_repeated_never_gets_free_writes() {
    let fs = UnknownFs::new("/never");
    // Room for exactly two 2-byte appends.
    let budget = budget_with_overrides(0, 0, 4, 0);
    let cell = cell_over(fs.clone(), Arc::clone(&budget));
    fs.inner.write("/a", b"old").unwrap();

    for _ in 0..2 {
        assert!(cell.execute_js("fs.appendFileSync('/a', 'xy')").is_err());
    }
    let third = cell.execute_js("fs.appendFileSync('/a', 'xy')");
    let message = third.unwrap_err().to_string().to_lowercase();
    assert!(
        message.contains("budget"),
        "the third attempt must be refused for budget, got: {message}"
    );
}

#[test]
fn a_plain_write_with_an_unknown_outcome_keeps_its_bytes_charged() {
    let fs = UnknownFs::new("/b");
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cell = cell_over(fs.clone(), Arc::clone(&budget));

    let err = cell.write_file("/b", b"12345").unwrap_err();

    assert!(
        matches!(&err, SandboxError::Vfs(VfsError::OutcomeUnknown(_))),
        "{err}"
    );
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        5,
        "the write may have landed, so its bytes stay charged"
    );
}

fn write_any(path: &str, data: &[u8]) -> VfsMutation {
    VfsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition: VfsWritePrecondition::Any,
    }
}

#[test]
fn the_fallback_keeps_the_reservation_and_does_not_roll_back_an_unknown_outcome() {
    let fs = UnknownFs::new("/b");
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cell = cell_over(fs.clone(), Arc::clone(&budget));
    fs.inner.write("/a", b"old-a").unwrap();

    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[write_any("/a", b"new-a"), write_any("/b", b"bb")],
        )
        .unwrap_err();

    assert!(
        matches!(&err, SandboxError::Vfs(VfsError::OutcomeUnknown(m)) if m == "response lost"),
        "the error must stay typed, got: {err:?}"
    );
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        7,
        "5 + 2: the pending write may land, so the reservation is kept"
    );
    assert_eq!(
        fs.writes.load(SeqCst),
        2,
        "no compensating write after an unknown outcome"
    );
    assert_eq!(fs.removes.load(SeqCst), 0, "no compensating remove either");
    assert_eq!(
        text(&*fs, "/a"),
        "new-a",
        "the rollback must not run: it would race the pending write"
    );
}

#[test]
fn positive_control_a_known_fallback_failure_still_releases_and_rolls_back() {
    // The same shape with a definite failure: the reservation is released and
    // the earlier item is restored. Guards the tests above against passing
    // because the fallback never ran.
    struct RefusingB(Arc<UnknownFs>);
    impl VirtualFs for RefusingB {
        fn read(&self, p: &str) -> Result<Vec<u8>, VfsError> {
            self.0.read(p)
        }
        fn write(&self, p: &str, d: &[u8]) -> Result<(), VfsError> {
            if p == "/b" {
                return Err(VfsError::Io("disk full".to_owned()));
            }
            self.0.inner.write(p, d)
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
            self.0.inner.remove(p)
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
        fn apply_batch(&self, _m: &[FsMutation]) -> Result<(), VfsError> {
            Err(VfsError::Unsupported("apply_batch".to_owned()))
        }
    }
    let backing = UnknownFs::new("/never");
    backing.inner.write("/a", b"old-a").unwrap();
    let fs: Arc<dyn VirtualFs> = Arc::new(RefusingB(backing.clone()));
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        fs,
        capability(&["/**"], &["/**"], false, true),
        Arc::clone(&budget),
        journal,
        http,
    );

    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[write_any("/a", b"new-a"), write_any("/b", b"bb")],
        )
        .unwrap_err();

    assert!(
        matches!(&err, SandboxError::Vfs(VfsError::Io(_))),
        "{err:?}"
    );
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0, "released");
    assert_eq!(text(&*backing, "/a"), "old-a", "rolled back");
}
