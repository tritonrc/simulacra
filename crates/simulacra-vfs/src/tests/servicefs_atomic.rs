//! ServiceFs answers the atomic calls for `/svc` by the normalized path, so a
//! non-canonical spelling cannot reach the inner filesystem.

use std::sync::Arc;

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};

use crate::{IntegrationLister, MemoryFs, ServiceFs};

struct NoIntegrations;

impl IntegrationLister for NoIntegrations {
    fn integration_names(&self) -> Vec<String> {
        Vec::new()
    }
    fn integration_metadata(&self, _name: &str) -> Option<String> {
        None
    }
    fn integration_readme(&self, _name: &str) -> Option<String> {
        None
    }
    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        Vec::new()
    }
}

fn service_over_memory() -> (Arc<MemoryFs>, ServiceFs<Arc<dyn VirtualFs>>) {
    let inner = Arc::new(MemoryFs::new());
    let fs = ServiceFs::new(
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        Arc::new(NoIntegrations),
    );
    (inner, fs)
}

/// Spellings of `/svc/a` that are not the canonical one.
const SVC_SPELLINGS: [&str; 4] = ["/x/../svc/a", "//svc/a", "/svc/./a", "/svc/b/../a"];

fn write_item(path: &str) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: vec![9],
        precondition: Precondition::Unconditional,
    }
}

fn assert_denied<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(result, Err(VfsError::PermissionDenied(_))),
        "{what}: {result:?}"
    );
}

#[test]
fn servicefs_denies_write_if_on_a_non_canonical_svc_path_and_writes_nothing() {
    let (inner, fs) = service_over_memory();
    for path in SVC_SPELLINGS {
        for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
            assert_denied(fs.write_if(path, b"1", precondition), path);
        }
        assert!(!inner.exists("/svc/a"), "{path} must not land in /svc/a");
    }
}

#[test]
fn servicefs_denies_a_rename_into_svc_by_a_non_canonical_path_and_changes_nothing() {
    let (inner, fs) = service_over_memory();
    inner.write("/a", b"1").unwrap();
    for path in SVC_SPELLINGS {
        assert_denied(fs.rename("/a", path), path);
        assert_eq!(inner.read("/a").unwrap(), b"1", "{path}");
        assert!(!inner.exists("/svc/a"), "{path} must not land in /svc/a");
    }
}

#[test]
fn servicefs_denies_a_rename_out_of_svc_by_a_non_canonical_path_and_changes_nothing() {
    let (inner, fs) = service_over_memory();
    inner.write("/svc/a", b"seeded").unwrap();
    for path in SVC_SPELLINGS {
        assert_denied(fs.rename(path, "/copy"), path);
        assert_eq!(inner.read("/svc/a").unwrap(), b"seeded", "{path}");
        assert!(!inner.exists("/copy"), "{path} must not move out of /svc");
    }
}

#[test]
fn servicefs_denies_a_batch_touching_svc_by_a_non_canonical_path_and_applies_nothing() {
    let (inner, fs) = service_over_memory();
    inner.write("/a", b"1").unwrap();
    inner.write("/svc/a", b"seeded").unwrap();
    for path in SVC_SPELLINGS {
        let last_items = [
            write_item(path),
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
                to: "/copy".into(),
                precondition: Precondition::Unconditional,
            },
        ];
        for last in last_items {
            let batch = [write_item("/one"), last];
            assert_denied(fs.apply_batch(&batch), path);
            assert!(!inner.exists("/one") && !inner.exists("/copy"), "{path}");
            assert_eq!(inner.read("/a").unwrap(), b"1", "{path}");
            assert_eq!(inner.read("/svc/a").unwrap(), b"seeded", "{path}");
        }
    }
}

#[test]
fn servicefs_forwards_non_canonical_paths_outside_svc_to_the_normalized_path() {
    let (inner, fs) = service_over_memory();
    fs.write_if("/a/../b", b"1", Precondition::MustBeAbsent)
        .unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");

    fs.rename("//b", "/./c").unwrap();
    assert_eq!(inner.read("/c").unwrap(), b"1");
    assert!(!inner.exists("/b"));

    fs.apply_batch(&[write_item("/a/../d"), write_item("//e")])
        .unwrap();
    assert_eq!(inner.read("/d").unwrap(), vec![9]);
    assert_eq!(inner.read("/e").unwrap(), vec![9]);
}
