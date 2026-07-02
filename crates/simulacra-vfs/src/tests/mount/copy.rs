use std::sync::Arc;

use simulacra_types::VirtualFs;

use crate::MemoryFs;
use crate::mount::{MountError, copy_host_dir_to_vfs};

#[test]
fn copy_host_dir_copies_files_recursively_into_vfs() {
    // S020 behavior 20: mount copies full host directory tree recursively
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Create a directory tree
    std::fs::create_dir_all(root.join("sub/nested")).unwrap();
    std::fs::write(root.join("top.txt"), b"top content").unwrap();
    std::fs::write(root.join("sub/mid.txt"), b"mid content").unwrap();
    std::fs::write(root.join("sub/nested/deep.txt"), b"deep content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, total_bytes) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 3);
    assert_eq!(
        total_bytes,
        b"top content".len() as u64 + b"mid content".len() as u64 + b"deep content".len() as u64
    );

    assert_eq!(vfs.read("/mount/top.txt").unwrap(), b"top content");
    assert_eq!(vfs.read("/mount/sub/mid.txt").unwrap(), b"mid content");
    assert_eq!(
        vfs.read("/mount/sub/nested/deep.txt").unwrap(),
        b"deep content"
    );
}

#[test]
fn copy_host_dir_creates_empty_directories_in_vfs() {
    // S020 behavior 23: empty host directories become empty VFS directories
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("empty_dir")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 0, "no files in an empty directory");
    assert!(
        vfs.exists("/mount/empty_dir"),
        "empty dir should exist in VFS"
    );
    let entries = vfs.list_dir("/mount/empty_dir").unwrap();
    assert!(entries.is_empty(), "empty dir should have no entries");
}

#[test]
fn copy_host_dir_includes_hidden_files() {
    // S020 behavior 24: hidden files (starting with .) are included
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join(".hidden"), b"secret").unwrap();
    std::fs::write(root.join("visible"), b"public").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 2);
    assert_eq!(vfs.read("/mount/.hidden").unwrap(), b"secret");
    assert_eq!(vfs.read("/mount/visible").unwrap(), b"public");
}

#[test]
fn copy_host_dir_copies_binary_files_as_raw_bytes() {
    // S020 behavior 22: binary files copied as-is
    let tmp = tempfile::tempdir().unwrap();
    let binary_data: Vec<u8> = (0u8..=255).collect();
    std::fs::write(tmp.path().join("binary.bin"), &binary_data).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(vfs.read("/mount/binary.bin").unwrap(), binary_data);
}

#[test]
fn copy_host_dir_file_limit_exceeded_returns_error() {
    // S020 behavior 29: exceeding max_files_per_mount fails
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(tmp.path().join(format!("file{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let result = copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 3, 10_000_000, "/mount");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::FileLimitExceeded {
            mount_target,
            actual,
            limit,
        } => {
            assert_eq!(mount_target, "/mount");
            assert!(actual > 3, "actual {actual} should exceed limit 3");
            assert_eq!(limit, 3);
        }
        other => panic!("expected FileLimitExceeded, got {other:?}"),
    }
}

#[test]
fn copy_host_dir_byte_limit_exceeded_returns_error() {
    // S020 behavior 29: exceeding max_bytes_per_mount fails
    let tmp = tempfile::tempdir().unwrap();
    let large_data = vec![0u8; 500];
    std::fs::write(tmp.path().join("big.bin"), &large_data).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let result = copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 100, "/mount");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::SizeLimitExceeded {
            mount_target,
            actual,
            limit,
        } => {
            assert_eq!(mount_target, "/mount");
            assert!(actual > 100, "actual {actual} should exceed limit 100");
            assert_eq!(limit, 100);
        }
        other => panic!("expected SizeLimitExceeded, got {other:?}"),
    }
}

#[test]
fn copy_host_dir_limits_are_per_mount_not_global() {
    // S020 behavior 30: limits are per-mount
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    for i in 0..3 {
        std::fs::write(tmp1.path().join(format!("a{i}.txt")), b"data").unwrap();
        std::fs::write(tmp2.path().join(format!("b{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    // Each mount has 3 files, limit is 5 — both should succeed independently
    let r1 = copy_host_dir_to_vfs(tmp1.path(), "/m1", &vfs, 5, 10_000_000, "/m1");
    let r2 = copy_host_dir_to_vfs(tmp2.path(), "/m2", &vfs, 5, 10_000_000, "/m2");
    assert!(r1.is_ok());
    assert!(r2.is_ok());
}

#[cfg(unix)]
#[test]
fn copy_host_dir_follows_symlinks() {
    // S020 behavior 21: host symlinks are resolved (followed) before copying
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("real.txt"), b"real content").unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    // Both the real file and the symlink target should be copied
    assert_eq!(file_count, 2);
    assert_eq!(vfs.read("/mount/real.txt").unwrap(), b"real content");
    assert_eq!(vfs.read("/mount/link.txt").unwrap(), b"real content");
}

#[cfg(unix)]
#[test]
fn copy_host_dir_detects_symlink_loops() {
    // S020 behavior 21: symlink loops are detected and skipped
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("dir_a")).unwrap();
    // Create a symlink loop: dir_a/loop -> root (which contains dir_a)
    std::os::unix::fs::symlink(root, root.join("dir_a/loop")).unwrap();
    std::fs::write(root.join("dir_a/file.txt"), b"content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    // Should not hang or error — should complete with the loop skipped
    let result = copy_host_dir_to_vfs(root, "/mount", &vfs, 10000, 100_000_000, "/mount");
    assert!(
        result.is_ok(),
        "symlink loop should be skipped, not cause an error: {result:?}"
    );

    // The real file should still be copied
    assert_eq!(vfs.read("/mount/dir_a/file.txt").unwrap(), b"content");
}
