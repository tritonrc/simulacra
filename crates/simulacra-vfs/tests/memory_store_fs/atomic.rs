use std::sync::Arc;

use simulacra_memory::{MemoryStore, SqliteMemoryStore};
use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::{MemoryFs, MemoryStoreFs};

use super::{memory_capability, memory_path, tenant};

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

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<dyn MemoryStore>,
    inner: Arc<dyn VirtualFs>,
    fs: MemoryStoreFs<Arc<dyn VirtualFs>>,
}

fn fixture_over(inner: Arc<dyn VirtualFs>, write_scopes: &[&str]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let capability = memory_capability(&["/var/memory/self", "/mnt"], write_scopes);
    let fs = MemoryStoreFs::new(Arc::clone(&inner), tenant(), Arc::clone(&store), capability);
    Fixture {
        _dir: dir,
        store,
        inner,
        fs,
    }
}

fn fixture() -> Fixture {
    fixture_over(Arc::new(MemoryFs::new()), &["/var/memory/self"])
}

fn write_item(path: &str, data: &[u8], precondition: Precondition) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition,
    }
}

fn stored(fx: &Fixture, path: &str) -> Option<Vec<u8>> {
    fx.fs.read(path).ok()
}

#[test]
fn memory_store_answers_unsupported_for_a_rename_into_memory() {
    let fx = fixture();
    fx.inner.write("/a", b"1").unwrap();
    assert!(matches!(
        fx.fs.rename("/a", "/var/memory/self/x.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(fx.inner.exists("/a"));
    assert!(stored(&fx, "/var/memory/self/x.md").is_none());
}

#[test]
fn memory_store_answers_unsupported_for_a_rename_out_of_memory_and_keeps_the_entry() {
    let fx = fixture();
    fx.store
        .put(&tenant(), &memory_path("/var/memory/self/x.md"), b"note")
        .unwrap();
    assert!(matches!(
        fx.fs.rename("/var/memory/self/x.md", "/copy.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(!fx.inner.exists("/copy.md"));
    assert_eq!(stored(&fx, "/var/memory/self/x.md").unwrap(), b"note");
}

#[test]
fn memory_store_answers_unsupported_for_write_if_into_memory_and_mnt() {
    let fx = fixture();
    for path in ["/var/memory/self/x.md", "/mnt/policies/hr.md"] {
        for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
            let err = fx.fs.write_if(path, b"1", precondition).unwrap_err();
            assert!(matches!(err, VfsError::Unsupported(_)), "{path}: {err:?}");
        }
        assert!(stored(&fx, path).is_none(), "{path} must stay absent");
    }
}

#[test]
fn memory_store_answers_unsupported_even_when_the_write_scope_would_refuse() {
    // The caller's per-half emulation reaches `write`, which enforces scopes
    // and answers PermissionDenied; the atomic call must not pre-empt that.
    let fx = fixture_over(Arc::new(MemoryFs::new()), &[]);
    fx.inner.write("/a", b"1").unwrap();
    assert!(matches!(
        fx.fs.rename("/a", "/var/memory/self/x.md"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        fx.fs.write("/var/memory/self/x.md", b"1"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(fx.inner.exists("/a"));
}

#[test]
fn memory_store_answers_unsupported_for_a_batch_whose_last_item_alone_touches_memory() {
    let fx = fixture();
    fx.inner.write("/a", b"1").unwrap();
    for last in [
        write_item("/var/memory/self/y.md", &[2], Precondition::Unconditional),
        FsMutation::Remove {
            path: "/mnt/policies/hr.md".into(),
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
        assert!(matches!(
            fx.fs.apply_batch(&batch),
            Err(VfsError::Unsupported(_))
        ));
        assert!(!fx.inner.exists("/one") && !fx.inner.exists("/b"));
        assert_eq!(fx.inner.read("/a").unwrap(), b"1");
    }
}

#[test]
fn memory_store_forwards_rename_write_if_and_batches_outside_memory() {
    let fx = fixture();
    fx.inner.write("/a", b"1").unwrap();
    fx.fs.rename("/a", "/b").unwrap();
    assert_eq!(fx.inner.read("/b").unwrap(), b"1");
    assert!(!fx.inner.exists("/a"));

    let err = fx
        .fs
        .write_if("/b", b"x", Precondition::MustBeAbsent)
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    fx.fs
        .write_if("/c", b"3", Precondition::MustBeAbsent)
        .unwrap();
    assert_eq!(fx.inner.read("/c").unwrap(), b"3");

    let failing = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/c", &[2], Precondition::MustBeAbsent),
    ];
    assert!(matches!(
        fx.fs.apply_batch(&failing),
        Err(VfsError::Conflict(_))
    ));
    assert!(!fx.inner.exists("/ok"));
    fx.fs
        .apply_batch(&[write_item("/ok", &[1], Precondition::MustBeAbsent)])
        .unwrap();
    assert_eq!(fx.inner.read("/ok").unwrap(), vec![1]);
}

#[test]
fn memory_store_forwards_list_dir_bounded_outside_memory_and_answers_memory_itself() {
    let fx = fixture_over(
        Arc::new(TruncatingFs(MemoryFs::new())),
        &["/var/memory/self"],
    );
    fx.inner.write("/work/a", b"1").unwrap();
    let work = fx.fs.list_dir_bounded("/work").unwrap();
    assert_eq!(work.entries, vec!["a".to_string()]);
    assert!(work.truncated);

    fx.store
        .put(&tenant(), &memory_path("/var/memory/self/x.md"), b"note")
        .unwrap();
    let memory = fx.fs.list_dir_bounded("/var/memory/self").unwrap();
    assert_eq!(memory.entries, fx.fs.list_dir("/var/memory/self").unwrap());
    assert!(!memory.entries.is_empty());
    assert!(!memory.truncated, "the memory listing comes from the store");
}

/// Non-canonical spellings of memory (`/var/memory/self`) and `/mnt` paths.
const MEMORY_SPELLINGS: [&str; 7] = [
    "/x/../var/memory/self/x.md",
    "//var/memory/self/x.md",
    "/var/./memory/self/x.md",
    "/var/memory/self/y/../x.md",
    "/x/../mnt/policies/hr.md",
    "//mnt/policies/hr.md",
    "/mnt/./policies/hr.md",
];

/// The normalized path a spelling in `MEMORY_SPELLINGS` names.
fn normalized(spelling: &str) -> &'static str {
    if spelling.contains("mnt") {
        "/mnt/policies/hr.md"
    } else {
        "/var/memory/self/x.md"
    }
}

fn unsupported<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(result, Err(VfsError::Unsupported(_))),
        "{what}: {result:?}"
    );
}

#[test]
fn memory_store_answers_unsupported_for_write_if_on_a_non_canonical_path_and_writes_nothing() {
    let fx = fixture();
    for path in MEMORY_SPELLINGS {
        for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
            unsupported(fx.fs.write_if(path, b"1", precondition), path);
        }
        assert!(
            !fx.inner.exists(normalized(path)),
            "{path} must not land in the inner filesystem"
        );
    }
}

#[test]
fn memory_store_answers_unsupported_for_a_rename_into_memory_by_a_non_canonical_path() {
    let fx = fixture();
    fx.inner.write("/a", b"1").unwrap();
    for path in MEMORY_SPELLINGS {
        unsupported(fx.fs.rename("/a", path), path);
        assert_eq!(fx.inner.read("/a").unwrap(), b"1", "{path}");
        assert!(!fx.inner.exists(normalized(path)), "{path}");
    }
}

#[test]
fn memory_store_answers_unsupported_for_a_rename_out_of_memory_by_a_non_canonical_path() {
    let fx = fixture();
    for target in ["/var/memory/self/x.md", "/mnt/policies/hr.md"] {
        fx.inner.write(target, b"seeded").unwrap();
    }
    for path in MEMORY_SPELLINGS {
        unsupported(fx.fs.rename(path, "/copy.md"), path);
        assert_eq!(
            fx.inner.read(normalized(path)).unwrap(),
            b"seeded",
            "{path}"
        );
        assert!(!fx.inner.exists("/copy.md"), "{path}");
    }
}

#[test]
fn memory_store_answers_unsupported_for_a_batch_touching_memory_by_a_non_canonical_path() {
    let fx = fixture();
    fx.inner.write("/a", b"1").unwrap();
    for target in ["/var/memory/self/x.md", "/mnt/policies/hr.md"] {
        fx.inner.write(target, b"seeded").unwrap();
    }
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
            unsupported(fx.fs.apply_batch(&batch), path);
            assert!(
                !fx.inner.exists("/one") && !fx.inner.exists("/copy.md"),
                "{path}"
            );
            assert_eq!(fx.inner.read("/a").unwrap(), b"1", "{path}");
            assert_eq!(
                fx.inner.read(normalized(path)).unwrap(),
                b"seeded",
                "{path}"
            );
        }
    }
}

#[test]
fn memory_store_forwards_non_canonical_paths_outside_memory_to_the_normalized_path() {
    let fx = fixture();
    fx.fs
        .write_if("/a/../b", b"1", Precondition::MustBeAbsent)
        .unwrap();
    assert_eq!(fx.inner.read("/b").unwrap(), b"1");

    fx.fs.rename("//b", "/./c").unwrap();
    assert_eq!(fx.inner.read("/c").unwrap(), b"1");
    assert!(!fx.inner.exists("/b"));

    fx.fs
        .apply_batch(&[
            write_item("/a/../d", &[9], Precondition::Unconditional),
            write_item("//e", &[9], Precondition::Unconditional),
        ])
        .unwrap();
    assert_eq!(fx.inner.read("/d").unwrap(), vec![9]);
    assert_eq!(fx.inner.read("/e").unwrap(), vec![9]);
}
