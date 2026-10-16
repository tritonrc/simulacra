use std::sync::Arc;
use std::time::Duration;

use simulacra_types::{FsMutation, Precondition, TenantId, VfsError, VfsEvent, VirtualFs};

use super::common::{assert_permission_denied, make_procfs, make_procfs_over};
use crate::tests::bounded::TruncatingFs;
use crate::{MemoryFs, NotifyingFsLayer};

fn write_item(path: &str, data: &[u8], precondition: Precondition) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition,
    }
}

#[test]
fn procfs_forwards_a_rename_outside_proc() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    fs.rename("/a", "/b").unwrap();
    assert_eq!(fs.read("/b").unwrap(), b"1");
    assert!(!fs.exists("/a"));
}

#[test]
fn procfs_refuses_a_rename_into_proc() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    assert_permission_denied(&fs.rename("/a", "/proc/x").unwrap_err());
    assert!(fs.exists("/a"));
}

#[test]
fn procfs_refuses_a_rename_out_of_proc() {
    let fs = make_procfs();
    assert_permission_denied(&fs.rename("/proc/agent/id", "/b").unwrap_err());
    assert!(!fs.exists("/b"));
}

#[test]
fn procfs_refuses_write_if_into_proc_for_every_precondition() {
    let fs = make_procfs();
    for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
        let err = fs.write_if("/proc/x", b"1", precondition).unwrap_err();
        assert_permission_denied(&err);
    }
    assert!(!fs.exists("/proc/x"));
}

#[test]
fn procfs_refuses_a_batch_whose_last_item_alone_touches_proc() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    let batch = [
        write_item("/one", &[1], Precondition::Unconditional),
        FsMutation::Rename {
            from: "/a".into(),
            to: "/b".into(),
            precondition: Precondition::Unconditional,
        },
        write_item("/proc/x", &[2], Precondition::Unconditional),
    ];
    assert_permission_denied(&fs.apply_batch(&batch).unwrap_err());
    assert!(!fs.exists("/one") && !fs.exists("/b"));
    assert_eq!(fs.read("/a").unwrap(), b"1");
}

#[test]
fn procfs_refuses_removing_the_root_that_would_take_proc_with_it() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    assert_permission_denied(&fs.remove("/").unwrap_err());
    let batch = [FsMutation::Remove {
        path: "/".into(),
        precondition: Precondition::Unconditional,
    }];
    assert_permission_denied(&fs.apply_batch(&batch).unwrap_err());
    assert!(fs.exists("/a"));
}

#[test]
fn procfs_forwards_mailbox_paths_to_its_inner_filesystem() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    fs.rename("/a", "/proc/mailbox/out.md").unwrap();
    assert_eq!(fs.read("/proc/mailbox/out.md").unwrap(), b"1");
    fs.write_if("/proc/mailbox/two.md", b"2", Precondition::MustBeAbsent)
        .unwrap();
    fs.apply_batch(&[write_item(
        "/proc/mailbox/three.md",
        b"3",
        Precondition::MustBeAbsent,
    )])
    .unwrap();
    assert_eq!(fs.read("/proc/mailbox/three.md").unwrap(), b"3");
}

#[test]
fn procfs_lists_its_own_directories_untruncated() {
    let fs = make_procfs_over(TruncatingFs::new());
    let listing = fs.list_dir_bounded("/proc").unwrap();
    assert_eq!(listing.entries, fs.list_dir("/proc").unwrap());
    assert!(!listing.entries.is_empty());
    assert!(
        !listing.truncated,
        "ProcFs answers /proc itself; the inner bound does not apply"
    );
}

#[test]
fn procfs_forwards_list_dir_bounded_for_other_paths() {
    let fs = make_procfs_over(TruncatingFs::new());
    fs.write("/work/a", b"1").unwrap();
    let listing = fs.list_dir_bounded("/work").unwrap();
    assert_eq!(listing.entries, vec!["a".to_string()]);
    assert!(listing.truncated);
}

#[test]
fn procfs_forwards_write_if_conflicts_from_below() {
    let fs = make_procfs();
    fs.write("/a", b"1").unwrap();
    assert!(matches!(
        fs.write_if("/a", b"2", Precondition::MustBeAbsent),
        Err(VfsError::Conflict(_))
    ));
    assert_eq!(fs.read("/a").unwrap(), b"1");
}

#[test]
fn procfs_forwarded_batch_is_still_all_or_nothing() {
    let fs = make_procfs();
    fs.write("/taken", b"t").unwrap();
    let batch = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/taken", &[2], Precondition::MustBeAbsent),
    ];
    assert!(matches!(fs.apply_batch(&batch), Err(VfsError::Conflict(_))));
    assert!(!fs.exists("/ok"));
    fs.apply_batch(&[write_item("/ok", &[1], Precondition::MustBeAbsent)])
        .unwrap();
    assert_eq!(fs.read("/ok").unwrap(), vec![1]);
}

#[tokio::test]
async fn procfs_forwards_subscribe_to_its_inner_filesystem() {
    let tenant = TenantId::parse("tenant-a").unwrap();
    let layer = NotifyingFsLayer::for_tenant(
        tenant.clone(),
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
    );
    let fs = make_procfs_over(layer);
    let mut watcher = fs.subscribe("/");

    fs.write("/a.txt", b"hello").unwrap();

    let event = tokio::time::timeout(Duration::from_millis(100), watcher.recv())
        .await
        .expect("a write through ProcFs must reach a subscriber");
    assert!(
        matches!(&event, Some(VfsEvent::Written { tenant: t, path, len: 5 })
            if *t == tenant && path == std::path::Path::new("/a.txt")),
        "{event:?}"
    );
}
