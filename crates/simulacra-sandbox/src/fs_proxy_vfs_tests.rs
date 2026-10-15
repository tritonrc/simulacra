//! `AgentCellFsProxy`'s own `VirtualFs` impl (what the mediated shell gets)
//! forwards the atomic methods, but only after the same capability checks
//! the plain methods make. The proxy is crate-private, so this lives here.

use crate::fs_proxy::AgentCellFsProxy;
use crate::test_support::NullJournal;
use simulacra_types::{
    AgentId, CapabilityToken, DirListing, FsMetadata, FsMutation, JournalStorage, PathPattern,
    Precondition, ResourceBudget, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

/// A `MemoryFs` that counts calls to the four atomic methods.
#[derive(Default)]
struct CountingFs {
    inner: MemoryFs,
    write_ifs: AtomicUsize,
    renames: AtomicUsize,
    batches: AtomicUsize,
    bounded_lists: AtomicUsize,
}

impl CountingFs {
    fn calls(&self) -> [usize; 4] {
        [
            self.write_ifs.load(SeqCst),
            self.renames.load(SeqCst),
            self.batches.load(SeqCst),
            self.bounded_lists.load(SeqCst),
        ]
    }
}

impl VirtualFs for CountingFs {
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
    fn write_if(&self, path: &str, data: &[u8], pre: Precondition) -> Result<(), VfsError> {
        self.write_ifs.fetch_add(1, SeqCst);
        self.inner.write_if(path, data, pre)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.renames.fetch_add(1, SeqCst);
        self.inner.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.batches.fetch_add(1, SeqCst);
        self.inner.apply_batch(mutations)
    }
    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        self.bounded_lists.fetch_add(1, SeqCst);
        let mut entries = self.inner.list_dir(path)?;
        let truncated = entries.len() > 1;
        entries.truncate(1);
        Ok(DirListing { entries, truncated })
    }
}

/// A proxy over a fresh `CountingFs` that may read and write only `/ok/**`.
fn proxy() -> (Arc<CountingFs>, AgentCellFsProxy) {
    let fs = Arc::new(CountingFs::default());
    let vfs: Arc<dyn VirtualFs> = fs.clone();
    let capability = CapabilityToken {
        paths_read: vec![PathPattern("/ok/**".into())],
        paths_write: vec![PathPattern("/ok/**".into())],
        ..Default::default()
    };
    let journal: Arc<dyn JournalStorage> = Arc::new(NullJournal);
    let budget = Arc::new(Mutex::new(ResourceBudget::new(
        0,
        0,
        rust_decimal::Decimal::ZERO,
        0,
    )));
    let proxy = AgentCellFsProxy {
        vfs,
        capability,
        budget,
        journal,
        agent_id: AgentId("proxy-test".into()),
    };
    (fs, proxy)
}

fn refused(result: Result<(), VfsError>) -> bool {
    matches!(result, Err(ref e) if !matches!(e, VfsError::Unsupported(_)))
}

#[test]
fn write_if_is_forwarded_for_a_writable_path() {
    let (fs, proxy) = proxy();
    VirtualFs::write_if(&proxy, "/ok/a", b"1", Precondition::MustBeAbsent).unwrap();
    assert_eq!(fs.calls(), [1, 0, 0, 0]);
    assert_eq!(fs.inner.read("/ok/a").unwrap(), b"1");
}

#[test]
fn a_failed_write_if_precondition_surfaces_and_changes_nothing() {
    let (fs, proxy) = proxy();
    fs.inner.write("/ok/a", b"old").unwrap();
    let result = VirtualFs::write_if(&proxy, "/ok/a", b"new", Precondition::MustBeAbsent);
    assert!(result.is_err());
    assert_eq!(fs.calls()[0], 1, "the inner conditional write decided");
    assert_eq!(fs.inner.read("/ok/a").unwrap(), b"old");
}

#[test]
fn write_if_to_a_denied_path_is_refused_without_calling_the_inner_method() {
    let (fs, proxy) = proxy();
    let result = VirtualFs::write_if(&proxy, "/denied/a", b"1", Precondition::Unconditional);
    assert!(refused(result), "a capability denial, not Unsupported");
    assert_eq!(fs.calls(), [0, 0, 0, 0]);
    assert!(!fs.inner.exists("/denied/a"));
}

#[test]
fn rename_is_forwarded_when_both_ends_are_writable() {
    let (fs, proxy) = proxy();
    fs.inner.write("/ok/a", b"1").unwrap();
    VirtualFs::rename(&proxy, "/ok/a", "/ok/b").unwrap();
    assert_eq!(fs.calls(), [0, 1, 0, 0]);
    assert_eq!(fs.inner.read("/ok/b").unwrap(), b"1");
}

#[test]
fn rename_is_refused_when_either_end_is_denied() {
    let (fs, proxy) = proxy();
    fs.inner.write("/ok/a", b"1").unwrap();
    fs.inner.write("/denied/a", b"2").unwrap();
    assert!(refused(VirtualFs::rename(&proxy, "/ok/a", "/denied/b")));
    assert!(refused(VirtualFs::rename(&proxy, "/denied/a", "/ok/b")));
    assert_eq!(fs.calls(), [0, 0, 0, 0]);
    assert!(fs.inner.exists("/ok/a") && fs.inner.exists("/denied/a"));
}

#[test]
fn apply_batch_is_forwarded_when_every_path_is_writable() {
    let (fs, proxy) = proxy();
    let batch = [
        FsMutation::Write {
            path: "/ok/a".into(),
            data: b"1".to_vec(),
            precondition: Precondition::MustBeAbsent,
        },
        FsMutation::Mkdir {
            path: "/ok/d".into(),
        },
    ];
    VirtualFs::apply_batch(&proxy, &batch).unwrap();
    assert_eq!(fs.calls(), [0, 0, 1, 0]);
    assert!(fs.inner.exists("/ok/a") && fs.inner.exists("/ok/d"));
}

#[test]
fn apply_batch_with_one_denied_path_is_refused_whole() {
    let (fs, proxy) = proxy();
    fs.inner.write("/ok/a", b"1").unwrap();
    for denied in [
        FsMutation::Write {
            path: "/denied/a".into(),
            data: b"x".to_vec(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Remove {
            path: "/denied/a".into(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Mkdir {
            path: "/denied/d".into(),
        },
        FsMutation::Rename {
            from: "/ok/a".into(),
            to: "/denied/b".into(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Rename {
            from: "/denied/a".into(),
            to: "/ok/b".into(),
            precondition: Precondition::Unconditional,
        },
    ] {
        let batch = [
            FsMutation::Write {
                path: "/ok/new".into(),
                data: b"y".to_vec(),
                precondition: Precondition::Unconditional,
            },
            denied,
        ];
        assert!(refused(VirtualFs::apply_batch(&proxy, &batch)), "{batch:?}");
    }
    assert_eq!(fs.calls(), [0, 0, 0, 0]);
    assert!(
        !fs.inner.exists("/ok/new"),
        "nothing in a refused batch applied"
    );
}

#[test]
fn list_dir_bounded_is_forwarded_with_its_truncated_flag() {
    let (fs, proxy) = proxy();
    fs.inner.write("/ok/a", b"").unwrap();
    fs.inner.write("/ok/b", b"").unwrap();
    let listing = VirtualFs::list_dir_bounded(&proxy, "/ok").unwrap();
    assert_eq!(fs.calls(), [0, 0, 0, 1]);
    assert_eq!(listing.entries.len(), 1);
    assert!(listing.truncated);
}

#[test]
fn list_dir_bounded_of_a_denied_path_is_refused_without_calling_the_inner_method() {
    let (fs, proxy) = proxy();
    fs.inner.write("/denied/a", b"").unwrap();
    let result = VirtualFs::list_dir_bounded(&proxy, "/denied");
    assert!(matches!(result, Err(ref e) if !matches!(e, VfsError::Unsupported(_))));
    assert_eq!(fs.calls(), [0, 0, 0, 0]);
}
