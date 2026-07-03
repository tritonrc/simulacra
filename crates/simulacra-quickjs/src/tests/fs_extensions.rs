use super::support::*;

#[test]
fn fs_extended_host_functions_without_proxy_fail_instead_of_touching_raw_vfs() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"data").unwrap();
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = JsRuntime::new(Arc::clone(&vfs) as Arc<dyn VirtualFs>).unwrap();

    for script in [
        r#"fs.mkdirSync("/workspace/new-dir")"#,
        r#"fs.readdirSync("/workspace")"#,
        r#"fs.statSync("/workspace/file.txt")"#,
        r#"fs.unlinkSync("/workspace/file.txt")"#,
        r#"fs.renameSync("/workspace/a.txt", "/workspace/b.txt")"#,
        r#"fs.appendFileSync("/workspace/file.txt", "more")"#,
    ] {
        let error = rt
            .eval(script)
            .expect_err("filesystem access without FsProxy should fail");
        assert!(
            error
                .to_string()
                .contains("fs proxy not configured for mediated filesystem access"),
            "unexpected error for {script}: {error}"
        );
    }

    assert!(vfs.exists("/workspace/file.txt"));
    assert!(vfs.exists("/workspace/a.txt"));
    assert!(!vfs.exists("/workspace/b.txt"));
}

#[test]
fn fs_readdir_sync() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"a").unwrap();
    vfs.write("/workspace/b.txt", b"b").unwrap();
    vfs.mkdir("/workspace/subdir").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(r#"JSON.stringify(fs.readdirSync("/workspace").sort())"#)
        .unwrap();
    let result: Vec<String> = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert!(result.contains(&"a.txt".to_string()));
    assert!(result.contains(&"b.txt".to_string()));
    assert!(result.contains(&"subdir".to_string()));
}

#[test]
fn fs_readdir_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.readdirSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_stat_sync_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(
            r#"
        const s = fs.statSync("/workspace/file.txt");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory, size: s.size })
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], true);
    assert_eq!(val["isDirectory"], false);
    assert_eq!(val["size"], 5);
}

#[test]
fn fs_stat_sync_directory() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.mkdir("/workspace/dir").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    let out = rt
        .eval(
            r#"
        const s = fs.statSync("/workspace/dir");
        JSON.stringify({ isFile: s.isFile, isDirectory: s.isDirectory })
    "#,
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(out.result.as_deref().unwrap()).unwrap();
    assert_eq!(val["isFile"], false);
    assert_eq!(val["isDirectory"], true);
}

#[test]
fn fs_stat_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.statSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_unlink_sync_deletes_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.unlinkSync("/workspace/file.txt")"#).unwrap();
    assert!(!vfs.exists("/workspace/file.txt"));
}

#[test]
fn fs_unlink_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.unlinkSync("/nonexistent")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_moves_file() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/b.txt")"#)
        .unwrap();
    assert!(!vfs.exists("/workspace/a.txt"));
    assert!(vfs.exists("/workspace/b.txt"));
    assert_eq!(vfs.read("/workspace/b.txt").unwrap(), b"data");
}

#[test]
fn fs_rename_sync_nonexistent_throws() {
    let (rt, _) = make_runtime();
    let result = rt.eval(r#"fs.renameSync("/nonexistent", "/workspace/b.txt")"#);
    assert!(result.is_err());
}

#[test]
fn fs_rename_sync_creates_parent_dirs() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/a.txt", b"data").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.renameSync("/workspace/a.txt", "/workspace/sub/dir/b.txt")"#)
        .unwrap();
    assert!(vfs.exists("/workspace/sub/dir/b.txt"));
}

#[test]
fn fs_append_file_sync_appends() {
    let vfs = Arc::new(MemoryFs::new());
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.appendFileSync("/workspace/file.txt", " world")"#)
        .unwrap();
    assert_eq!(vfs.read("/workspace/file.txt").unwrap(), b"hello world");
}

#[test]
fn fs_append_file_sync_creates_file() {
    let vfs = Arc::new(MemoryFs::new());
    let rt = make_runtime_with_vfs_proxy(Arc::clone(&vfs));
    rt.eval(r#"fs.appendFileSync("/workspace/new.txt", "created")"#)
        .unwrap();
    assert_eq!(vfs.read("/workspace/new.txt").unwrap(), b"created");
}
