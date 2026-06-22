use chrono::Utc;
use simulacra_catalog::{AgentFile, AgentFileId, AgentId, CatalogAgentFileFs};
use simulacra_types::{VfsError, VfsSnapshot, VirtualFs};

fn file(name: &str, bytes: &[u8]) -> (AgentFile, Vec<u8>) {
    let now = Utc::now();
    (
        AgentFile {
            id: AgentFileId::new(),
            agent_id: AgentId::new(),
            name: name.to_owned(),
            mime_type: "application/octet-stream".to_owned(),
            size_bytes: bytes.len() as u64,
            created_at: now,
            updated_at: now,
        },
        bytes.to_vec(),
    )
}

fn fs(files: Vec<(AgentFile, Vec<u8>)>) -> Box<dyn VirtualFs> {
    Box::new(CatalogAgentFileFs::new(files))
}

#[test]
fn list_dir_root_returns_one_entry_per_file_name_verbatim() {
    let fs = fs(vec![
        file("handbook.pdf", b"%PDF-1.7"),
        file("report.csv", b"col1,col2\n1,2\n"),
    ]);

    let mut entries = fs.list_dir("/").unwrap();
    entries.sort();

    assert_eq!(
        entries,
        vec!["handbook.pdf".to_owned(), "report.csv".to_owned()]
    );
}

#[test]
fn list_dir_root_returns_empty_vec_for_empty_catalog_agent_file_fs() {
    let fs = fs(vec![]);

    assert_eq!(fs.list_dir("/").unwrap(), Vec::<String>::new());
}

#[test]
fn list_dir_non_root_returns_not_found() {
    let fs = fs(vec![file("handbook.pdf", b"bytes")]);

    let err = fs.list_dir("/anything-else").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(path) if path == "/anything-else"));
}

#[test]
fn read_known_name_returns_bytes_verbatim() {
    let bytes = vec![0x00, 0xFF, 0xF0, 0x9F, 0xA6, 0x80, 0xE2, 0x82, 0xAC];
    let fs = fs(vec![file("payload.bin", &bytes)]);

    let read = fs.read("/payload.bin").unwrap();

    assert_eq!(read, bytes);
}

#[test]
fn read_unknown_returns_not_found() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let err = fs.read("/unknown").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(path) if path == "/unknown"));
}

#[test]
fn read_root_returns_not_found() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let err = fs.read("/").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(path) if path == "/"));
}

#[test]
fn write_returns_permission_denied_and_mentions_path() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let err = fs.write("/foo.txt", b"x").unwrap_err();

    match err {
        VfsError::PermissionDenied(message) => assert!(message.contains("/foo.txt")),
        other => panic!("expected permission denied, got {other:?}"),
    }
}

#[test]
fn mkdir_returns_permission_denied() {
    let fs = fs(vec![]);

    let err = fs.mkdir("/dir").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn remove_known_name_returns_permission_denied() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let err = fs.remove("/known.txt").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn exists_reports_root_empty_known_and_unknown_paths_correctly() {
    let fs = fs(vec![file("known.txt", b"known")]);

    assert!(fs.exists("/"));
    assert!(fs.exists(""));
    assert!(fs.exists("/known.txt"));
    assert!(!fs.exists("/unknown"));
}

#[test]
fn metadata_root_returns_directory_metadata_with_zero_size() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let metadata = fs.metadata("/").unwrap();

    assert!(!metadata.is_file);
    assert!(metadata.is_dir);
    assert_eq!(metadata.size, 0);
}

#[test]
fn metadata_uses_bytes_length_not_size_bytes_metadata() {
    let bytes = vec![1, 2, 3, 4];
    let (mut agent_file, stored_bytes) = file("report.pdf", &bytes);
    agent_file.size_bytes = 999;
    let fs = fs(vec![(agent_file, stored_bytes)]);

    let metadata = fs.metadata("/report.pdf").unwrap();

    assert!(metadata.is_file);
    assert!(!metadata.is_dir);
    assert_eq!(metadata.size, bytes.len() as u64);
}

#[test]
fn metadata_unknown_returns_not_found() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let err = fs.metadata("/unknown").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(path) if path == "/unknown"));
}

#[test]
fn snapshot_returns_empty_vfs_snapshot() {
    let fs = fs(vec![file("known.txt", b"known")]);

    let snapshot = fs.snapshot().unwrap();

    assert_eq!(snapshot.data, Vec::<u8>::new());
}

#[test]
fn restore_returns_permission_denied() {
    let fs = fs(vec![file("known.txt", b"known")]);
    let snapshot = VfsSnapshot {
        data: vec![1, 2, 3],
    };

    let err = fs.restore(&snapshot).unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn read_duplicate_names_returns_first_matching_file_bytes() {
    let fs = fs(vec![
        file("duplicate.txt", b"first"),
        file("duplicate.txt", b"second"),
    ]);

    let read = fs.read("/duplicate.txt").unwrap();

    assert_eq!(read, b"first".to_vec());
}
