use simulacra_types::{VfsError, VirtualFs};

use super::common::make_procfs;

#[test]
fn procfs_exists_returns_true_for_agent_id() {
    let fs = make_procfs();
    assert!(fs.exists("/proc/agent/id"));
}

#[test]
fn procfs_exists_returns_false_for_nonexistent_proc_path() {
    let fs = make_procfs();
    assert!(!fs.exists("/proc/nonexistent"));
}

#[test]
fn procfs_metadata_for_agent_directory_returns_directory_metadata() {
    let fs = make_procfs();
    let md = fs.metadata("/proc/agent").unwrap();
    assert!(md.is_dir);
    assert!(!md.is_file);
}

#[test]
fn procfs_metadata_for_agent_id_returns_file_metadata_with_correct_size() {
    let fs = make_procfs();
    let md = fs.metadata("/proc/agent/id").unwrap();
    assert!(md.is_file);
    assert!(!md.is_dir);
    assert_eq!(md.size, "agent-abc123".len() as u64);
}

#[test]
fn procfs_metadata_for_proc_root_returns_directory_metadata() {
    let fs = make_procfs();
    let md = fs.metadata("/proc").unwrap();
    assert!(md.is_dir);
    assert!(!md.is_file);
}

// --- Unknown paths tests ----------------------------------------------------

#[test]
fn procfs_read_unknown_proc_path_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_read_unknown_agent_child_path_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/agent/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_list_dir_unknown_subtree_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.list_dir("/proc/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

// --- Non-proc paths delegate to inner VFS -----------------------------------

#[test]
fn procfs_non_proc_paths_delegate_to_inner_vfs() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    assert_eq!(vfs.read("/workspace/file.txt").unwrap(), b"hello");
}
