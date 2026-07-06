use super::*;

#[test]
fn shell_commands_never_touch_real_filesystem() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let sentinel = "/tmp/simulacra_shell_test_sentinel_should_not_exist.txt";

    // Remove sentinel if it somehow exists
    let _ = std::fs::remove_file(sentinel);

    // Write to the sentinel path through the shell
    run_shell(vfs, HashMap::new(), &format!("echo leaked > {sentinel}"));

    // The real filesystem should NOT have this file
    assert!(
        !std::path::Path::new(sentinel).exists(),
        "shell echo > path should write to VFS, not real filesystem"
    );

    // But VFS should have it
    assert!(vfs.read(sentinel).is_ok(), "VFS should have the file");
}
