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

/// Non-canonical spellings of the memory paths (`/var/memory/**`, `/mnt/**`).
const MEMORY_SPELLINGS: [&str; 8] = [
    "/x/../var/memory/self/x.md",
    "//var/memory/self/x.md",
    "/var/./memory/self/x.md",
    "/var/memory/self/y/../x.md",
    "/x/../mnt/policies/x.md",
    "//mnt/policies/x.md",
    "/mnt/./policies/x.md",
    "/mnt/y/../policies/x.md",
];

/// The normalized path a spelling in `MEMORY_SPELLINGS` names.
fn normalized(spelling: &str) -> &'static str {
    if spelling.contains("mnt") {
        "/mnt/policies/x.md"
    } else {
        "/var/memory/self/x.md"
    }
}

fn not_found<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(result, Err(VfsError::NotFound(_))),
        "{what}: {result:?}"
    );
}

#[test]
fn non_canonical_memory_paths_answer_not_found_for_write_if_and_write_nothing() {
    let (inner, fs) = rejecting();
    for path in MEMORY_SPELLINGS {
        for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
            not_found(fs.write_if(path, b"1", precondition), path);
        }
        assert!(
            !inner.exists(normalized(path)),
            "{path} must not land in the inner filesystem"
        );
    }
}

#[test]
fn non_canonical_memory_paths_answer_not_found_for_a_rename_in_either_direction() {
    let (inner, fs) = rejecting();
    inner.write("/a", b"1").unwrap();
    inner.write("/var/memory/self/x.md", b"seeded").unwrap();
    inner.write("/mnt/policies/x.md", b"seeded").unwrap();
    for path in MEMORY_SPELLINGS {
        not_found(fs.rename("/a", path), path);
        not_found(fs.rename(path, "/copy.md"), path);
        assert_eq!(inner.read("/a").unwrap(), b"1", "{path}");
        assert_eq!(inner.read(normalized(path)).unwrap(), b"seeded", "{path}");
        assert!(!inner.exists("/copy.md"), "{path}");
    }
}

#[test]
fn a_batch_touching_memory_by_a_non_canonical_path_is_not_found_and_applies_nothing() {
    let (inner, fs) = rejecting();
    inner.write("/a", b"1").unwrap();
    inner.write("/var/memory/self/x.md", b"seeded").unwrap();
    inner.write("/mnt/policies/x.md", b"seeded").unwrap();
    for path in MEMORY_SPELLINGS {
        let last_items = [
            write_item(path, &[2], Precondition::Unconditional),
            FsMutation::Remove {
                path: path.into(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Rename {
                from: "/a".into(),
                to: path.into(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Rename {
                from: path.into(),
                to: "/copy.md".into(),
                precondition: Precondition::Unconditional,
            },
        ];
        for last in last_items {
            let batch = [write_item("/one", &[1], Precondition::Unconditional), last];
            not_found(fs.apply_batch(&batch), path);
            assert!(!inner.exists("/one") && !inner.exists("/copy.md"), "{path}");
            assert_eq!(inner.read("/a").unwrap(), b"1", "{path}");
            assert_eq!(inner.read(normalized(path)).unwrap(), b"seeded", "{path}");
        }
    }
}

#[test]
fn non_canonical_paths_outside_memory_forward_to_the_normalized_path() {
    let (inner, fs) = rejecting();
    fs.write_if("/a/../b", b"1", Precondition::MustBeAbsent)
        .unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");

    fs.rename("//b", "/./c").unwrap();
    assert_eq!(inner.read("/c").unwrap(), b"1");
    assert!(!inner.exists("/b"));

    fs.apply_batch(&[
        write_item("/a/../d", &[9], Precondition::Unconditional),
        write_item("//e", &[9], Precondition::Unconditional),
    ])
    .unwrap();
    assert_eq!(inner.read("/d").unwrap(), vec![9]);
    assert_eq!(inner.read("/e").unwrap(), vec![9]);
}
