use simulacra_types::VirtualFs;

use super::common::{assert_permission_denied, make_procfs};

#[test]
fn procfs_root_directory_listing_returns_expected_sorted_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc").unwrap(),
        vec![
            "agent",
            "budget",
            "capabilities",
            "hooks",
            "mailbox",
            "session",
            "tools"
        ]
    );
}

#[test]
fn procfs_agent_directory_listing_returns_all_agent_file_names_sorted() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/agent").unwrap(),
        vec!["id", "model", "name", "parent_id", "turn"]
    );
}

#[test]
fn procfs_tools_directory_listing_returns_one_entry_per_tool() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/tools").unwrap(),
        vec!["file_read", "list_dir"]
    );
}

#[test]
fn procfs_budget_directory_listing_returns_all_budget_file_names_sorted() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/budget").unwrap(),
        vec![
            "max_cost",
            "max_fuel",
            "max_tokens",
            "max_turns",
            "remaining_tokens",
            "remaining_turns",
            "used_cost",
            "used_fuel",
            "used_tokens",
            "used_turns",
        ]
    );
}

#[test]
fn procfs_mailbox_directory_listing_delegates_to_inner_vfs() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"body").unwrap();
    vfs.write("/proc/mailbox/data.json", b"{}").unwrap();
    let mut names = vfs.list_dir("/proc/mailbox").unwrap();
    names.sort();
    assert_eq!(names, vec!["data.json", "report.md"]);
}

#[test]
fn procfs_mailbox_listing_on_fresh_vfs_returns_empty_list() {
    // Before any writes, listing /proc/mailbox/ should return [] not NotFound.
    let fs = make_procfs();
    let names = fs.list_dir("/proc/mailbox").unwrap();
    assert!(
        names.is_empty(),
        "empty mailbox should return [] not NotFound; got {names:?}"
    );
}

// --- Trailing-slash path tests ----------------------------------------------

#[test]
fn procfs_list_dir_with_trailing_slash_on_proc_root_returns_expected_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/").unwrap(),
        vec![
            "agent",
            "budget",
            "capabilities",
            "hooks",
            "mailbox",
            "session",
            "tools"
        ]
    );
}

#[test]
fn procfs_list_dir_with_trailing_slash_on_agent_subdirectory_returns_expected_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/agent/").unwrap(),
        vec!["id", "model", "name", "parent_id", "turn"]
    );
}

#[test]
fn procfs_list_dir_with_trailing_slash_on_budget_subdirectory_returns_expected_children() {
    let fs = make_procfs();
    let listing = fs.list_dir("/proc/budget/").unwrap();
    assert!(listing.contains(&"max_tokens".to_string()));
    assert!(listing.contains(&"used_tokens".to_string()));
    assert!(listing.contains(&"remaining_tokens".to_string()));
}

#[test]
fn procfs_metadata_for_proc_root_with_trailing_slash_returns_directory_metadata() {
    let fs = make_procfs();
    let meta = fs.metadata("/proc/").unwrap();
    assert!(meta.is_dir);
    assert!(!meta.is_file);
}

// --- Write protection tests -------------------------------------------------

#[test]
fn procfs_write_to_agent_id_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .write("/proc/agent/id", b"mutated")
        .expect_err("/proc/agent/id should be read-only");
    assert_permission_denied(&err);
}

#[test]
fn procfs_write_to_budget_max_tokens_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .write("/proc/budget/max_tokens", b"999")
        .expect_err("/proc/budget/* should be read-only");
    assert_permission_denied(&err);
}

#[test]
fn procfs_remove_tool_entry_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .remove("/proc/tools/file_read")
        .expect_err("/proc/tools/* should be immutable");
    assert_permission_denied(&err);
}

#[test]
fn procfs_mkdir_under_proc_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .mkdir("/proc/custom")
        .expect_err("creating /proc directories should be rejected");
    assert_permission_denied(&err);
}

#[test]
fn procfs_rejects_all_non_mailbox_write_operations() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    assert_permission_denied(
        &vfs.write("/proc/session/id", b"x")
            .expect_err("session id write should be denied"),
    );
    assert_permission_denied(
        &vfs.remove("/proc/agent/id")
            .expect_err("agent id remove should be denied"),
    );
    assert_permission_denied(
        &vfs.mkdir("/proc/budget/new")
            .expect_err("budget mkdir should be denied"),
    );
}

// --- Mailbox tests ----------------------------------------------------------

#[test]
fn procfs_mailbox_write_to_report_md_succeeds() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
}

#[test]
fn procfs_mailbox_read_returns_written_content() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
    assert_eq!(vfs.read("/proc/mailbox/report.md").unwrap(), b"report body");
}

#[test]
fn procfs_mailbox_list_dir_shows_written_files() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"body").unwrap();
    vfs.write("/proc/mailbox/analysis.json", b"{}").unwrap();
    let mut names = vfs.list_dir("/proc/mailbox").unwrap();
    names.sort();
    assert_eq!(names, vec!["analysis.json", "report.md"]);
}

#[test]
fn procfs_mailbox_files_survive_snapshot_and_restore() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
    let snap = vfs.snapshot().unwrap();
    vfs.remove("/proc/mailbox/report.md").unwrap();
    vfs.restore(&snap).unwrap();
    assert_eq!(vfs.read("/proc/mailbox/report.md").unwrap(), b"report body");
}
