use simulacra_server::{LocalDiskArtifactStore, S3ArtifactStore};
use simulacra_types::{ArtifactEntry, ArtifactError, ArtifactStore};

#[test]
fn artifact_store_trait_exposes_put_get_list_and_delete_task_methods() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();
    let store: &dyn ArtifactStore = &store;

    store
        .put("task-123", "tenant-a", "summary.md", b"hello")
        .unwrap();
    assert_eq!(
        store.get("tenant-a", "task-123", "summary.md").unwrap(),
        b"hello"
    );
    assert_eq!(store.list("tenant-a", "task-123").unwrap().len(), 1);
    store.delete_task("tenant-a", "task-123").unwrap();
}

#[test]
fn artifact_entry_struct_exposes_path_and_size_fields() {
    let entry = ArtifactEntry {
        path: "reports/flagged.csv".to_string(),
        size: 1203,
    };

    assert_eq!(entry.path, "reports/flagged.csv");
    assert_eq!(entry.size, 1203);
}

#[test]
fn put_and_get_round_trip_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"# Quarterly summary")
        .unwrap();

    assert_eq!(
        store.get("tenant-a", "task-123", "summary.md").unwrap(),
        b"# Quarterly summary"
    );
}

#[test]
fn local_disk_artifact_store_writes_to_tenant_and_task_scoped_paths() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "reports/q1.csv", b"a,b,c")
        .unwrap();

    let expected_path = temp.path().join("tenant-a/task-123/reports/q1.csv");
    assert!(
        expected_path.exists(),
        "artifact bytes must land at {{dir}}/{{tenant}}/{{task_id}}/{{path}}"
    );
}

#[test]
fn put_is_atomic_via_temp_file_and_rename() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"old bytes")
        .unwrap();
    store
        .put("task-123", "tenant-a", "summary.md", b"new bytes")
        .unwrap();

    let final_path = temp.path().join("tenant-a/task-123/summary.md");
    assert_eq!(std::fs::read(&final_path).unwrap(), b"new bytes");

    let mut leftovers = std::fs::read_dir(final_path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp") || name.contains(".partial"))
        .collect::<Vec<_>>();
    leftovers.sort();

    assert!(
        leftovers.is_empty(),
        "atomic put should not leave temp files behind, found {leftovers:?}"
    );
}

#[test]
fn list_is_recursive_and_returns_relative_paths_only_for_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"summary")
        .unwrap();
    store
        .put("task-123", "tenant-a", "reports/q1/flagged.csv", b"flagged")
        .unwrap();
    store
        .put("task-123", "tenant-a", "reports/q2/clean.csv", b"clean")
        .unwrap();

    let mut entries = store.list("tenant-a", "task-123").unwrap();
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    assert_eq!(
        entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>(),
        vec![
            "reports/q1/flagged.csv",
            "reports/q2/clean.csv",
            "summary.md",
        ]
    );
}

#[test]
fn delete_task_removes_all_artifacts_for_a_task() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"summary")
        .unwrap();
    store
        .put("task-123", "tenant-a", "reports/q1.csv", b"q1")
        .unwrap();

    store.delete_task("tenant-a", "task-123").unwrap();

    assert!(matches!(
        store.get("tenant-a", "task-123", "summary.md"),
        Err(ArtifactError::NotFound(_))
    ));
    assert!(
        !temp.path().join("tenant-a/task-123").exists(),
        "delete_task must remove the task subtree"
    );
}

#[test]
fn path_validation_rejects_parent_segments_absolute_paths_and_null_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    for path in ["../escape.md", "/etc/passwd", "bad\0name.txt"] {
        let err = store
            .put("task-123", "tenant-a", path, b"boom")
            .expect_err("invalid artifact paths must be rejected");
        assert!(
            matches!(err, ArtifactError::InvalidPath(_)),
            "expected InvalidPath for {path:?}, got {err:?}"
        );
    }
}

#[test]
fn different_tenants_do_not_collide_for_the_same_task_id_and_artifact_path() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"tenant a")
        .unwrap();
    store
        .put("task-123", "tenant-b", "summary.md", b"tenant b")
        .unwrap();

    assert_eq!(
        std::fs::read(temp.path().join("tenant-a/task-123/summary.md")).unwrap(),
        b"tenant a"
    );
    assert_eq!(
        std::fs::read(temp.path().join("tenant-b/task-123/summary.md")).unwrap(),
        b"tenant b"
    );
}

#[test]
fn overwrite_semantics_are_last_write_wins() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalDiskArtifactStore::new(temp.path()).unwrap();

    store
        .put("task-123", "tenant-a", "summary.md", b"first")
        .unwrap();
    store
        .put("task-123", "tenant-a", "summary.md", b"second")
        .unwrap();

    assert_eq!(
        store.get("tenant-a", "task-123", "summary.md").unwrap(),
        b"second"
    );
}

#[test]
fn s3_artifact_store_is_a_send_sync_interface_only_trait() {
    struct FakeS3;

    impl S3ArtifactStore for FakeS3 {}

    fn assert_send_sync<T: S3ArtifactStore + Send + Sync>() {}

    assert_send_sync::<FakeS3>();
}
