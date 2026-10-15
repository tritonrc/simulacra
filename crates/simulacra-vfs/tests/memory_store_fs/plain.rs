//! MemoryStoreFs answers the plain calls for memory paths by the normalized
//! path, so a non-canonical spelling gets the canonical answer and cannot
//! reach the inner filesystem.

use std::sync::Arc;

use simulacra_memory::{MemoryStore, SqliteMemoryStore};
use simulacra_types::{VfsError, VirtualFs};
use simulacra_vfs::{MemoryFs, MemoryStoreFs};

use super::{memory_capability, memory_path, tenant};

const IN_SCOPE: &str = "/var/memory/self/x.md";
const OUT_OF_SCOPE: &str = "/var/memory/other/x.md";
const MNT: &str = "/mnt/policies/hr.md";

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<dyn MemoryStore>,
    inner: Arc<MemoryFs>,
    fs: MemoryStoreFs<Arc<dyn VirtualFs>>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let inner = Arc::new(MemoryFs::new());
    let capability = memory_capability(&["/var/memory/self", "/mnt"], &["/var/memory/self"]);
    let fs = MemoryStoreFs::new(
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        tenant(),
        Arc::clone(&store),
        capability,
    );
    Fixture {
        _dir: dir,
        store,
        inner,
        fs,
    }
}

impl Fixture {
    fn seed_store(&self, path: &str, data: &[u8]) {
        self.store.put(&tenant(), &memory_path(path), data).unwrap();
    }

    fn in_store(&self, path: &str) -> Option<Vec<u8>> {
        self.store
            .get(&tenant(), &memory_path(path))
            .ok()
            .map(|(bytes, _version)| bytes)
    }
}

/// Spellings of the absolute path `canonical` that are not the canonical one.
fn spellings(canonical: &str) -> Vec<String> {
    let (first, tail) = canonical[1..].split_once('/').unwrap();
    vec![
        format!("/x/..{canonical}"),
        format!("/{canonical}"),
        format!("/{first}/./{tail}"),
    ]
}

fn denied<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(result, Err(VfsError::PermissionDenied(_))),
        "{what}: {result:?}"
    );
}

#[test]
fn memory_store_routes_write_on_a_non_canonical_in_scope_path_to_the_store() {
    let fx = fixture();
    fx.fs.write(IN_SCOPE, b"canonical").unwrap();
    assert_eq!(fx.in_store(IN_SCOPE).unwrap(), b"canonical");
    for path in spellings(IN_SCOPE) {
        let fx = fixture();
        fx.fs.write(&path, b"v").unwrap();
        assert_eq!(
            fx.in_store(IN_SCOPE),
            Some(b"v".to_vec()),
            "{path} must reach the memory store"
        );
        assert!(
            !fx.inner.exists(IN_SCOPE),
            "{path} must not land in the inner filesystem"
        );
    }
}

#[test]
fn memory_store_denies_write_on_a_non_canonical_path_outside_scope_or_under_mnt() {
    let fx = fixture();
    for canonical in [OUT_OF_SCOPE, MNT] {
        denied(fx.fs.write(canonical, b"1"), canonical);
        for path in spellings(canonical) {
            denied(fx.fs.write(&path, b"1"), &path);
            assert!(
                !fx.inner.exists(canonical),
                "{path} must not land in the inner filesystem"
            );
            assert!(fx.in_store(canonical).is_none(), "{path}");
        }
    }
}

#[test]
fn memory_store_answers_mkdir_on_a_non_canonical_path_as_the_canonical_one() {
    let fx = fixture();
    fx.fs.mkdir("/var/memory/self/d").unwrap();
    for path in spellings("/var/memory/self/d") {
        fx.fs.mkdir(&path).unwrap();
        assert!(
            !fx.inner.exists("/var/memory/self/d"),
            "{path} must not create a directory in the inner filesystem"
        );
    }
    for canonical in ["/var/memory/other/d", "/mnt/policies"] {
        denied(fx.fs.mkdir(canonical), canonical);
        for path in spellings(canonical) {
            denied(fx.fs.mkdir(&path), &path);
            assert!(!fx.inner.exists(canonical), "{path}");
        }
    }
}

#[test]
fn memory_store_removes_a_non_canonical_in_scope_path_from_the_store_only() {
    for path in spellings(IN_SCOPE) {
        let fx = fixture();
        fx.seed_store(IN_SCOPE, b"stored");
        fx.inner.write(IN_SCOPE, b"inner").unwrap();
        fx.fs.remove(&path).unwrap();
        assert!(
            fx.in_store(IN_SCOPE).is_none(),
            "{path} must remove the store entry"
        );
        assert_eq!(fx.inner.read(IN_SCOPE).unwrap(), b"inner", "{path}");
    }
}

#[test]
fn memory_store_denies_remove_on_a_non_canonical_path_outside_scope_or_under_mnt() {
    let fx = fixture();
    for canonical in [OUT_OF_SCOPE, MNT] {
        fx.seed_store(canonical, b"stored");
        fx.inner.write(canonical, b"inner").unwrap();
        denied(fx.fs.remove(canonical), canonical);
        for path in spellings(canonical) {
            denied(fx.fs.remove(&path), &path);
            assert_eq!(fx.inner.read(canonical).unwrap(), b"inner", "{path}");
            assert_eq!(fx.in_store(canonical).unwrap(), b"stored", "{path}");
        }
    }
}

#[test]
fn memory_store_reads_a_non_canonical_path_as_the_canonical_one() {
    let fx = fixture();
    fx.seed_store(IN_SCOPE, b"note");
    fx.seed_store(OUT_OF_SCOPE, b"secret");
    fx.seed_store(MNT, b"policy");

    for (canonical, expected) in [(IN_SCOPE, &b"note"[..]), (MNT, &b"policy"[..])] {
        let canonical_meta = format!("{:?}", fx.fs.metadata(canonical).unwrap());
        for path in spellings(canonical) {
            assert_eq!(fx.fs.read(&path).unwrap(), expected, "read {path}");
            assert!(fx.fs.exists(&path), "exists {path}");
            assert_eq!(
                format!("{:?}", fx.fs.metadata(&path).unwrap()),
                canonical_meta,
                "metadata {path}"
            );
        }
    }
    for path in spellings("/var/memory/self") {
        assert_eq!(
            fx.fs.list_dir(&path).unwrap(),
            vec!["x.md".to_string()],
            "list_dir {path}"
        );
    }

    // The scope check is the canonical answer for a path outside the search
    // scopes; a spelling must not read around it from the inner filesystem.
    fx.inner.write(OUT_OF_SCOPE, b"inner").unwrap();
    denied(fx.fs.read(OUT_OF_SCOPE), OUT_OF_SCOPE);
    for path in spellings(OUT_OF_SCOPE) {
        denied(fx.fs.read(&path), &path);
        assert!(!fx.fs.exists(&path), "exists {path}");
    }
}
