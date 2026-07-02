use simulacra_types::{VfsError, VirtualFs};

use crate::OverlayFs;

use super::common::SharedFs;

#[test]
fn overlay_write_to_upper_does_not_mutate_lower() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/shared.txt", b"lower").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.write("/shared.txt", b"upper").unwrap();

    assert_eq!(vfs.read("/shared.txt").unwrap(), b"upper");
    assert_eq!(lower_view.read("/shared.txt").unwrap(), b"lower");
}

#[test]
fn overlay_read_falls_through_to_lower_when_upper_has_no_entry() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/from-lower.txt", b"lower-layer").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    assert_eq!(vfs.read("/from-lower.txt").unwrap(), b"lower-layer");
}

#[test]
fn overlay_delete_in_upper_shadows_lower_entry() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/masked.txt", b"still in lower").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/masked.txt").unwrap();

    assert!(matches!(
        vfs.read("/masked.txt"),
        Err(VfsError::NotFound(_))
    ));
    assert!(!vfs.exists("/masked.txt"));
    assert_eq!(lower_view.read("/masked.txt").unwrap(), b"still in lower");
}

#[test]
fn overlay_write_implicitly_revives_whited_out_ancestor() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    lower_vfs.write("/dir/old.txt", b"old").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    // 1. Remove /dir (creates whiteout for /dir)
    // Note: lower still has it, but it's shadowed.
    vfs.remove("/dir").unwrap();
    assert!(!vfs.exists("/dir/old.txt"));

    // 2. Write to /dir/new.txt (should implicitly recreate /dir)
    vfs.write("/dir/new.txt", b"new content").unwrap();

    // 3. Should be able to read /dir/new.txt
    // If this fails, it means /dir is still whited out and shadowing /dir/new.txt
    let content = vfs.read("/dir/new.txt");
    assert!(
        content.is_ok(),
        "Should be able to read file after writing to deleted directory, got {:?}",
        content.err()
    );
    assert_eq!(content.unwrap(), b"new content");
}

#[test]
fn overlay_snapshot_then_restore_preserves_whiteout_state() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/visible.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    // Delete the file (creates a whiteout)
    vfs.remove("/visible.txt").unwrap();
    assert!(!vfs.exists("/visible.txt"));

    // Snapshot should capture the whiteout
    let snapshot = vfs.snapshot().unwrap();

    // Write something new to dirty the state
    vfs.write("/visible.txt", b"revived").unwrap();
    assert!(vfs.exists("/visible.txt"));

    // Restore should bring back the whiteout
    vfs.restore(&snapshot).unwrap();
    assert!(
        !vfs.exists("/visible.txt"),
        "whiteout should survive snapshot/restore — /visible.txt should remain deleted"
    );
}
#[test]
fn overlay_list_dir_merges_upper_and_lower_entries() {
    // S001 behavior 7 + V7: OverlayFs list_dir produces union of lower and upper
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/from_lower.txt", b"l").unwrap();
    upper_vfs.write("/dir/from_upper.txt", b"u").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    assert!(
        entries.contains(&"from_lower.txt".to_string()),
        "list_dir should include lower entries: {entries:?}"
    );
    assert!(
        entries.contains(&"from_upper.txt".to_string()),
        "list_dir should include upper entries: {entries:?}"
    );
}

#[test]
fn overlay_list_dir_returns_sorted_entries() {
    // S001 behavior 4: list_dir returns entries sorted by name
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/zebra.txt", b"z").unwrap();
    lower_vfs.write("/dir/alpha.txt", b"a").unwrap();
    upper_vfs.write("/dir/middle.txt", b"m").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    assert_eq!(entries, vec!["alpha.txt", "middle.txt", "zebra.txt"]);
}

#[test]
fn overlay_list_dir_deduplicates_entries_present_in_both_layers() {
    // When a file exists in both upper and lower, list_dir should show it once
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/shared.txt", b"lower").unwrap();
    upper_vfs.write("/dir/shared.txt", b"upper").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    let count = entries.iter().filter(|e| *e == "shared.txt").count();
    assert_eq!(
        count, 1,
        "shared.txt should appear exactly once, got {entries:?}"
    );
}

#[test]
fn overlay_list_dir_excludes_whited_out_entries() {
    // Deleted lower entries should not appear in list_dir
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/dir/keep.txt", b"keep").unwrap();
    lower_vfs.write("/dir/delete.txt", b"delete").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/dir/delete.txt").unwrap();

    let entries = vfs.list_dir("/dir").unwrap();
    assert!(
        entries.contains(&"keep.txt".to_string()),
        "keep.txt should remain: {entries:?}"
    );
    assert!(
        !entries.contains(&"delete.txt".to_string()),
        "deleted entry should not appear: {entries:?}"
    );
}

#[test]
fn overlay_list_dir_on_whited_out_directory_returns_not_found() {
    // If the directory itself is whited out, list_dir should return NotFound
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/dir/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/dir").unwrap();

    assert!(matches!(vfs.list_dir("/dir"), Err(VfsError::NotFound(_))));
}

#[test]
fn overlay_list_dir_upper_only_directory() {
    // list_dir works when directory exists only in upper
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let upper_vfs: &dyn VirtualFs = &upper;

    upper_vfs.write("/upper_only/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/upper_only").unwrap();
    assert_eq!(entries, vec!["file.txt"]);
}

#[test]
fn overlay_list_dir_lower_only_directory() {
    // list_dir works when directory exists only in lower
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/lower_only/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/lower_only").unwrap();
    assert_eq!(entries, vec!["file.txt"]);
}

#[test]
fn overlay_list_dir_nonexistent_directory_returns_error() {
    // list_dir on a path that doesn't exist in either layer returns NotFound
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    assert!(matches!(
        vfs.list_dir("/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}
