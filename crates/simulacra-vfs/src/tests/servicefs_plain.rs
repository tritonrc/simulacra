//! ServiceFs answers the plain calls for `/svc` by the normalized path, so a
//! non-canonical spelling gets the answer the canonical one gets and cannot
//! reach the inner filesystem.

use std::sync::Arc;

use simulacra_types::{VfsError, VirtualFs};

use crate::{IntegrationLister, MemoryFs, ServiceFs};

struct ToyIntegrations;

impl IntegrationLister for ToyIntegrations {
    fn integration_names(&self) -> Vec<String> {
        vec!["toy".to_string()]
    }
    fn integration_metadata(&self, name: &str) -> Option<String> {
        (name == "toy").then(|| r#"{"name":"toy"}"#.to_string())
    }
    fn integration_readme(&self, name: &str) -> Option<String> {
        (name == "toy").then(|| "# Toy".to_string())
    }
    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        Vec::new()
    }
}

fn service_over_memory() -> (Arc<MemoryFs>, ServiceFs<Arc<dyn VirtualFs>>) {
    let inner = Arc::new(MemoryFs::new());
    let fs = ServiceFs::new(
        Arc::clone(&inner) as Arc<dyn VirtualFs>,
        Arc::new(ToyIntegrations),
    );
    (inner, fs)
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

fn assert_denied<T: std::fmt::Debug>(result: Result<T, VfsError>, what: &str) {
    assert!(
        matches!(result, Err(VfsError::PermissionDenied(_))),
        "{what}: {result:?}"
    );
}

#[test]
fn servicefs_denies_write_on_a_non_canonical_svc_path_and_writes_nothing() {
    let (inner, fs) = service_over_memory();
    assert_denied(fs.write("/svc/a", b"1"), "canonical");
    for path in spellings("/svc/a") {
        assert_denied(fs.write(&path, b"1"), &path);
        assert!(!inner.exists("/svc/a"), "{path} must not land in /svc/a");
    }
}

#[test]
fn servicefs_denies_mkdir_on_a_non_canonical_svc_path_and_creates_nothing() {
    let (inner, fs) = service_over_memory();
    assert_denied(fs.mkdir("/svc/d"), "canonical");
    for path in spellings("/svc/d") {
        assert_denied(fs.mkdir(&path), &path);
        assert!(!inner.exists("/svc/d"), "{path} must not create /svc/d");
    }
}

#[test]
fn servicefs_denies_remove_on_a_non_canonical_svc_path_and_removes_nothing() {
    let (inner, fs) = service_over_memory();
    inner.write("/svc/a", b"seeded").unwrap();
    assert_denied(fs.remove("/svc/a"), "canonical");
    for path in spellings("/svc/a") {
        assert_denied(fs.remove(&path), &path);
        assert_eq!(inner.read("/svc/a").unwrap(), b"seeded", "{path}");
    }
}

#[test]
fn servicefs_reads_a_non_canonical_svc_path_as_the_canonical_one() {
    let (_inner, fs) = service_over_memory();
    let file = "/svc/toy/README.md";
    let dir = "/svc/toy";
    assert_eq!(fs.read(file).unwrap(), b"# Toy");
    assert!(fs.exists(file));
    assert_eq!(fs.list_dir("/svc").unwrap(), vec!["toy".to_string()]);
    let canonical_meta = format!("{:?}", fs.metadata(file).unwrap());

    for path in spellings(file) {
        assert_eq!(fs.read(&path).unwrap(), b"# Toy", "read {path}");
        assert!(fs.exists(&path), "exists {path}");
        assert_eq!(
            format!("{:?}", fs.metadata(&path).unwrap()),
            canonical_meta,
            "metadata {path}"
        );
    }
    for path in spellings(dir) {
        assert!(fs.exists(&path), "exists {path}");
        assert_eq!(fs.list_dir(&path).unwrap().len(), 3, "list_dir {path}");
    }
    for path in ["/x/../svc", "//svc", "/svc/./"] {
        assert_eq!(
            fs.list_dir(path).unwrap(),
            vec!["toy".to_string()],
            "list_dir {path}"
        );
    }
}
