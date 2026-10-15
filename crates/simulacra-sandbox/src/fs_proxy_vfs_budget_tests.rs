//! `AgentCellFsProxy`'s `VirtualFs` forwards of `write_if`, `rename` and
//! `apply_batch` account like its plain methods: they reserve against the VFS
//! write budget, release on failure, and journal.

use crate::fs_proxy::AgentCellFsProxy;
use crate::test_support::CapturingJournal;
use simulacra_types::{
    AgentId, CapabilityToken, FsMetadata, FsMutation, JournalEntryKind, JournalStorage,
    PathPattern, Precondition, ResourceBudget, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::sync::{Arc, Mutex};

/// A `MemoryFs` whose `write_if` always fails, noting how many bytes the
/// budget held at the moment it was called.
struct FailingWriteIfFs {
    inner: MemoryFs,
    budget: Mutex<Option<Arc<Mutex<ResourceBudget>>>>,
    used_during_call: Mutex<Option<u64>>,
}

impl VirtualFs for FailingWriteIfFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
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
    fn write_if(&self, path: &str, _data: &[u8], _pre: Precondition) -> Result<(), VfsError> {
        if let Some(budget) = self.budget.lock().unwrap().as_ref() {
            *self.used_during_call.lock().unwrap() = Some(budget.lock().unwrap().used_vfs_bytes);
        }
        Err(VfsError::Conflict(path.to_string()))
    }
}

struct Fixture {
    proxy: AgentCellFsProxy,
    budget: Arc<Mutex<ResourceBudget>>,
    journal: Arc<CapturingJournal>,
}

impl Fixture {
    fn used(&self) -> u64 {
        self.budget.lock().unwrap().used_vfs_bytes
    }

    fn file_writes(&self) -> Vec<(String, u64)> {
        self.journal
            .entries()
            .into_iter()
            .filter_map(|e| match e.entry {
                JournalEntryKind::FileWrite { path, size_bytes } => Some((path, size_bytes)),
                _ => None,
            })
            .collect()
    }
}

/// A proxy over `vfs` that may write `/ok/**`, with a VFS write budget of
/// `max` bytes of which `used` are spent.
fn fixture(vfs: Arc<dyn VirtualFs>, max: u64, used: u64) -> Fixture {
    let mut budget = ResourceBudget::new(0, 0, rust_decimal::Decimal::ZERO, 0);
    budget.max_vfs_bytes = max;
    budget.used_vfs_bytes = used;
    let budget = Arc::new(Mutex::new(budget));
    let journal = Arc::new(CapturingJournal::default());
    let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
    let proxy = AgentCellFsProxy {
        vfs,
        capability: CapabilityToken {
            paths_read: vec![PathPattern("/ok/**".into())],
            paths_write: vec![PathPattern("/ok/**".into())],
            ..Default::default()
        },
        budget: Arc::clone(&budget),
        journal: journal_dyn,
        agent_id: AgentId("proxy-budget-test".into()),
    };
    Fixture {
        proxy,
        budget,
        journal,
    }
}

fn memory_fixture(max: u64, used: u64) -> (Arc<MemoryFs>, Fixture) {
    let fs = Arc::new(MemoryFs::new());
    let vfs: Arc<dyn VirtualFs> = fs.clone();
    (fs, fixture(vfs, max, used))
}

fn write_item(path: &str, data: &[u8]) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition: Precondition::Unconditional,
    }
}

fn refused(result: Result<(), VfsError>) -> bool {
    matches!(result, Err(ref e) if !matches!(e, VfsError::Unsupported(_)))
}

// ── write_if ─────────────────────────────────────────────────────────

#[test]
fn write_if_reserves_its_bytes_against_the_write_budget() {
    let (fs, f) = memory_fixture(10, 1);
    VirtualFs::write_if(&f.proxy, "/ok/a", b"1234", Precondition::MustBeAbsent).unwrap();
    assert_eq!(f.used(), 5);
    assert_eq!(fs.read("/ok/a").unwrap(), b"1234");
}

#[test]
fn write_if_the_budget_cannot_cover_is_refused_without_calling_the_inner_method() {
    let (fs, f) = memory_fixture(10, 8);
    let result = VirtualFs::write_if(&f.proxy, "/ok/a", b"12345", Precondition::Unconditional);
    assert!(refused(result));
    assert!(!fs.exists("/ok/a"), "the inner write_if was not called");
    assert_eq!(f.used(), 8);
    assert!(
        f.file_writes().is_empty(),
        "a refused write is not journaled"
    );
}

#[test]
fn write_if_holds_its_reservation_during_the_call_and_releases_it_on_failure() {
    let fs = Arc::new(FailingWriteIfFs {
        inner: MemoryFs::new(),
        budget: Mutex::new(None),
        used_during_call: Mutex::new(None),
    });
    let vfs: Arc<dyn VirtualFs> = fs.clone();
    let f = fixture(vfs, 100, 3);
    *fs.budget.lock().unwrap() = Some(Arc::clone(&f.budget));

    let result = VirtualFs::write_if(&f.proxy, "/ok/a", b"1234", Precondition::Unconditional);

    assert!(result.is_err());
    assert_eq!(
        *fs.used_during_call.lock().unwrap(),
        Some(7),
        "reserved before the inner call"
    );
    assert_eq!(f.used(), 3, "reservation released after the failure");
}

#[test]
fn write_if_journals_a_file_write_like_write_does() {
    let (_fs, f) = memory_fixture(0, 0);
    VirtualFs::write_if(&f.proxy, "/ok/a", b"abc", Precondition::MustBeAbsent).unwrap();
    assert_eq!(f.file_writes(), vec![("/ok/a".to_string(), 3)]);
}

// ── apply_batch ──────────────────────────────────────────────────────

#[test]
fn apply_batch_reserves_the_sum_of_its_write_items() {
    let (fs, f) = memory_fixture(100, 0);
    fs.write("/ok/old", b"o").unwrap();
    let batch = [
        write_item("/ok/a", b"123"),
        FsMutation::Mkdir {
            path: "/ok/d".into(),
        },
        write_item("/ok/b", b"4567"),
        FsMutation::Remove {
            path: "/ok/old".into(),
            precondition: Precondition::Unconditional,
        },
    ];
    VirtualFs::apply_batch(&f.proxy, &batch).unwrap();
    assert_eq!(f.used(), 7, "3 + 4; mkdir and remove cost nothing");
}

#[test]
fn apply_batch_over_the_budget_is_refused_whole() {
    // Each write fits alone; the sum does not.
    let (fs, f) = memory_fixture(10, 0);
    let batch = [
        write_item("/ok/a", b"123456"),
        write_item("/ok/b", b"123456"),
    ];
    assert!(refused(VirtualFs::apply_batch(&f.proxy, &batch)));
    assert!(!fs.exists("/ok/a") && !fs.exists("/ok/b"));
    assert_eq!(f.used(), 0);
    assert!(f.file_writes().is_empty());
}

#[test]
fn apply_batch_journals_each_write() {
    let (_fs, f) = memory_fixture(0, 0);
    let batch = [
        write_item("/ok/a", b"12"),
        FsMutation::Mkdir {
            path: "/ok/d".into(),
        },
        write_item("/ok/b", b"345"),
    ];
    VirtualFs::apply_batch(&f.proxy, &batch).unwrap();
    assert_eq!(
        f.file_writes(),
        vec![("/ok/a".to_string(), 2), ("/ok/b".to_string(), 3)]
    );
}

// ── rename ───────────────────────────────────────────────────────────

#[test]
fn rename_with_an_exhausted_budget_is_refused_without_calling_the_inner_method() {
    let (fs, f) = memory_fixture(10, 10);
    fs.write("/ok/a", b"1").unwrap();
    assert!(refused(VirtualFs::rename(&f.proxy, "/ok/a", "/ok/b")));
    assert!(fs.exists("/ok/a") && !fs.exists("/ok/b"));
}

#[test]
fn rename_reserves_no_bytes() {
    let (fs, f) = memory_fixture(10, 4);
    fs.write("/ok/a", b"1234").unwrap();
    VirtualFs::rename(&f.proxy, "/ok/a", "/ok/b").unwrap();
    assert_eq!(f.used(), 4);
    assert!(fs.exists("/ok/b") && !fs.exists("/ok/a"));
}

#[test]
fn rename_journals_its_result_like_the_js_rename_does() {
    let (fs, f) = memory_fixture(0, 0);
    fs.write("/ok/a", b"1").unwrap();
    VirtualFs::rename(&f.proxy, "/ok/a", "/ok/b").unwrap();
    let renames: Vec<_> = f
        .journal
        .entries()
        .into_iter()
        .filter_map(|e| match e.entry {
            JournalEntryKind::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == "rename" => Some((content, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(renames, vec![("renamed /ok/a to /ok/b".to_string(), false)]);
}

#[test]
fn a_failed_rename_journals_the_error() {
    let (fs, f) = memory_fixture(0, 0);
    fs.write("/ok/a", b"1").unwrap();
    fs.write("/ok/b", b"2").unwrap();
    assert!(VirtualFs::rename(&f.proxy, "/ok/a", "/ok/b").is_err());
    assert!(f.journal.entries().iter().any(|e| matches!(
        &e.entry,
        JournalEntryKind::ToolResult { tool_name, is_error: true, .. } if tool_name == "rename"
    )));
}
