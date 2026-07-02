use proptest::prelude::*;
use simulacra_types::{VfsError, VirtualFs};

use crate::MemoryFs;

use super::common::{SharedFs, assert_span, assert_span_with_path, capture_spans};

#[test]
fn write_then_read_roundtrip_returns_identical_bytes() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    let data = b"roundtrip bytes \0 with punctuation!";

    vfs.write("/artifacts/output.bin", data).unwrap();

    let roundtrip = vfs.read("/artifacts/output.bin").unwrap();
    assert_eq!(roundtrip, data);
}

proptest! {
    #[test]
    fn dotdot_at_root_resolves_to_root(
        climbs in 1usize..8,
        segments in prop::collection::vec("[a-z]{1,8}", 1..4),
    ) {
        let fs = SharedFs::memory();
        let vfs: &dyn VirtualFs = &fs;
        let canonical_path = format!("/{}", segments.join("/"));
        let traversed_path = format!("/{}{}/./", "../".repeat(climbs), segments.join("//"));
        let payload = canonical_path.as_bytes().to_vec();

        vfs.write(&canonical_path, &payload).unwrap();

        let read_back = vfs.read(&traversed_path).unwrap();
        prop_assert_eq!(read_back, payload);
    }
}

#[test]
fn snapshot_then_restore_is_a_no_op() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/alpha.txt", b"alpha").unwrap();
    vfs.write("/nested/beta.txt", b"beta").unwrap();
    let snapshot = vfs.snapshot().unwrap();

    vfs.write("/alpha.txt", b"mutated").unwrap();
    vfs.remove("/nested").unwrap();
    vfs.write("/new.txt", b"new").unwrap();

    vfs.restore(&snapshot).unwrap();

    let restored = vfs.snapshot().unwrap();
    assert_eq!(restored.data, snapshot.data);
    assert_eq!(vfs.read("/alpha.txt").unwrap(), b"alpha");
    assert_eq!(vfs.read("/nested/beta.txt").unwrap(), b"beta");
    assert!(matches!(vfs.read("/new.txt"), Err(VfsError::NotFound(_))));
}

#[test]
fn list_dir_on_nonexistent_path_returns_error_not_empty_list() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    assert!(matches!(
        vfs.list_dir("/does/not/exist"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn metadata_returns_correct_size_after_write() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    let data = b"1234567890";

    vfs.write("/sizes/payload.bin", data).unwrap();

    let metadata = vfs.metadata("/sizes/payload.bin").unwrap();
    assert!(metadata.is_file);
    assert!(!metadata.is_dir);
    assert_eq!(metadata.size, data.len() as u64);
}

#[test]
fn write_produces_vfs_write_span_with_path() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| vfs.write("/logs/write.txt", b"hello").unwrap());

    assert_span_with_path(&spans, "vfs_write", "/logs/write.txt");
}

#[test]
fn read_produces_vfs_read_span_with_path() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/logs/read.txt", b"hello").unwrap();

    let (_, spans) = capture_spans(|| vfs.read("/logs/read.txt").unwrap());

    assert_span_with_path(&spans, "vfs_read", "/logs/read.txt");
}

#[test]
fn snapshot_and_restore_produce_vfs_spans() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/logs/state.txt", b"hello").unwrap();
    let snapshot = vfs.snapshot().unwrap();

    let (_, spans) = capture_spans(|| {
        let captured = vfs.snapshot().unwrap();
        vfs.restore(&captured).unwrap();
    });

    assert_span(&spans, "vfs_snapshot");
    assert_span(&spans, "vfs_restore");

    let current = vfs.snapshot().unwrap();
    assert_eq!(current.data, snapshot.data);
}

#[test]
fn list_dir_returns_entries_sorted_by_name() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/dir/c.txt", b"").unwrap();
    vfs.write("/dir/a.txt", b"").unwrap();
    vfs.write("/dir/b.txt", b"").unwrap();

    let entries = vfs.list_dir("/dir").unwrap();
    assert_eq!(entries, vec!["a.txt", "b.txt", "c.txt"]);
}

#[test]
fn write_creates_parent_directories_implicitly() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/a/b/c/file.txt", b"content").unwrap();

    assert!(vfs.exists("/a"));
    assert!(vfs.exists("/a/b"));
    assert!(vfs.exists("/a/b/c"));
    assert!(vfs.exists("/a/b/c/file.txt"));

    let entries = vfs.list_dir("/a/b").unwrap();
    assert_eq!(entries, vec!["c"]);
}

// ---------------------------------------------------------------------------
// S001 gap-fill: remove() on non-existent path returns error
// ---------------------------------------------------------------------------

#[test]
fn remove_nonexistent_path_returns_not_found_error() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    let result = vfs.remove("/does/not/exist.txt");
    assert!(
        matches!(result, Err(VfsError::NotFound(_))),
        "remove() on non-existent path should return NotFound, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// S001 gap-fill: OverlayFs snapshot/restore preserves whiteout state
// ---------------------------------------------------------------------------

#[test]
fn concurrent_reads_and_writes_do_not_corrupt_state() {
    use std::sync::Arc;

    let fs = Arc::new(MemoryFs::new());

    let mut handles = vec![];
    for i in 0..10 {
        let fs_clone = Arc::clone(&fs);
        handles.push(std::thread::spawn(move || {
            let path = format!("/concurrent_{i}.txt");
            let data = format!("data_{i}");
            fs_clone.write(&path, data.as_bytes()).unwrap();
            let read_back = fs_clone.read(&path).unwrap();
            assert_eq!(read_back, data.as_bytes());
        }));
    }

    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    // Verify all files exist with correct content
    for i in 0..10 {
        let path = format!("/concurrent_{i}.txt");
        let data = fs.read(&path).unwrap();
        assert_eq!(data, format!("data_{i}").as_bytes());
    }
}
