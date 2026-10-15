use std::sync::Arc;

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::MemoryFs;

use super::MemoryRejectFs;

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

fn rejecting() -> (Arc<MemoryFs>, MemoryRejectFs<Arc<dyn VirtualFs>>) {
    let inner = Arc::new(MemoryFs::new());
    let fs = MemoryRejectFs::new(Arc::clone(&inner) as Arc<dyn VirtualFs>);
    (inner, fs)
}

#[test]
fn memory_paths_answer_not_found_for_rename_and_write_if_and_change_nothing() {
    let (inner, fs) = rejecting();
    inner.write("/a", b"1").unwrap();
    inner.write("/var/memory/self/x.md", b"seeded").unwrap();

    for result in [
        fs.rename("/a", "/var/memory/self/y.md"),
        fs.rename("/a", "/mnt/policies/y.md"),
        fs.rename("/var/memory/self/x.md", "/copy.md"),
        fs.write_if("/var/memory/self/z.md", b"1", Precondition::Unconditional),
        fs.write_if("/mnt/policies/z.md", b"1", Precondition::MustBeAbsent),
    ] {
        assert!(matches!(result, Err(VfsError::NotFound(_))), "{result:?}");
    }

    assert_eq!(inner.read("/a").unwrap(), b"1");
    assert_eq!(inner.read("/var/memory/self/x.md").unwrap(), b"seeded");
    for path in [
        "/var/memory/self/y.md",
        "/mnt/policies/y.md",
        "/copy.md",
        "/var/memory/self/z.md",
    ] {
        assert!(!inner.exists(path), "{path} must not exist");
    }
}

#[test]
fn a_batch_whose_last_item_alone_touches_memory_is_not_found_and_applies_nothing() {
    let (inner, fs) = rejecting();
    inner.write("/a", b"1").unwrap();
    for last in [
        write_item("/var/memory/self/y.md", &[2], Precondition::Unconditional),
        FsMutation::Remove {
            path: "/mnt/policies/hr.md".into(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Rename {
            from: "/a".into(),
            to: "/mnt/a".into(),
            precondition: Precondition::Unconditional,
        },
    ] {
        let batch = [
            write_item("/one", &[1], Precondition::Unconditional),
            FsMutation::Rename {
                from: "/a".into(),
                to: "/b".into(),
                precondition: Precondition::Unconditional,
            },
            last,
        ];
        let err = fs.apply_batch(&batch).unwrap_err();
        assert!(matches!(err, VfsError::NotFound(_)), "{err:?}");
        assert!(!inner.exists("/one") && !inner.exists("/b"));
        assert_eq!(inner.read("/a").unwrap(), b"1");
    }
}

#[test]
fn paths_outside_memory_forward_with_their_atomicity() {
    let (inner, fs) = rejecting();
    inner.write("/a", b"1").unwrap();
    fs.rename("/a", "/b").unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");
    assert!(!inner.exists("/a"));

    let err = fs
        .write_if("/b", b"x", Precondition::MustBeAbsent)
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    fs.write_if("/c", b"3", Precondition::MustBeAbsent).unwrap();

    let failing = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/c", &[2], Precondition::MustBeAbsent),
    ];
    assert!(matches!(
        fs.apply_batch(&failing),
        Err(VfsError::Conflict(_))
    ));
    assert!(!inner.exists("/ok"));
    fs.apply_batch(&[write_item("/ok", &[1], Precondition::MustBeAbsent)])
        .unwrap();
    assert_eq!(inner.read("/ok").unwrap(), vec![1]);
}

#[test]
fn bounded_listing_is_not_found_for_memory_and_forwarded_elsewhere() {
    let inner = Arc::new(TruncatingFs(MemoryFs::new()));
    let fs = MemoryRejectFs::new(Arc::clone(&inner) as Arc<dyn VirtualFs>);
    inner.write("/work/a", b"1").unwrap();
    inner.write("/var/memory/self/x.md", b"seeded").unwrap();

    let err = fs.list_dir_bounded("/var/memory/self").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)), "{err:?}");
    let err = fs.list_dir_bounded("/mnt").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)), "{err:?}");

    let listing = fs.list_dir_bounded("/work").unwrap();
    assert_eq!(listing.entries, vec!["a".to_string()]);
    assert!(listing.truncated);
}
