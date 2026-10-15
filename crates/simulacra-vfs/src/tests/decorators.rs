use std::sync::Arc;

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};

use super::bounded::TruncatingFs;
use super::common::SharedFs;
use crate::{MemoryFs, OverlayFs, ReadOnlyPathGuard};

fn guarded_over(inner: Arc<MemoryFs>, prefix: &str) -> ReadOnlyPathGuard {
    let inner_dyn: Arc<dyn VirtualFs> = inner;
    ReadOnlyPathGuard::new(inner_dyn, prefix)
}

fn guarded() -> (Arc<MemoryFs>, ReadOnlyPathGuard) {
    let inner = Arc::new(MemoryFs::new());
    (inner.clone(), guarded_over(inner, "/skills"))
}

fn write_item(path: &str, data: &[u8], precondition: Precondition) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition,
    }
}

fn remove_item(path: &str) -> FsMutation {
    FsMutation::Remove {
        path: path.into(),
        precondition: Precondition::Unconditional,
    }
}

fn rename_item(from: &str, to: &str) -> FsMutation {
    FsMutation::Rename {
        from: from.into(),
        to: to.into(),
        precondition: Precondition::Unconditional,
    }
}

#[test]
fn guard_forwards_an_atomic_rename_outside_its_prefix() {
    let (inner, guard) = guarded();
    inner.write("/a", b"1").unwrap();
    guard.rename("/a", "/b").unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");
    assert!(!inner.exists("/a"));
}

#[test]
fn guard_refuses_rename_into_or_out_of_its_prefix() {
    let (inner, guard) = guarded();
    inner.write("/a", b"1").unwrap();
    inner.write("/skills/x/SKILL.md", b"s").unwrap();
    assert!(matches!(
        guard.rename("/a", "/skills/a"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        guard.rename("/skills/x", "/y"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(inner.exists("/a") && inner.exists("/skills/x/SKILL.md"));
    assert!(!inner.exists("/skills/a") && !inner.exists("/y"));
}

#[test]
fn guard_refuses_renaming_an_ancestor_of_its_prefix() {
    let inner = Arc::new(MemoryFs::new());
    inner.write("/var/skills/a.md", b"s").unwrap();
    inner.write("/var/other", b"o").unwrap();
    let guard = guarded_over(inner.clone(), "/var/skills");
    assert!(matches!(
        guard.rename("/var", "/moved"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        guard.rename("/", "/moved"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert_eq!(inner.read("/var/skills/a.md").unwrap(), b"s");
    assert!(!inner.exists("/moved"));
}

#[test]
fn guard_refuses_a_batch_that_touches_its_prefix_and_applies_none_of_it() {
    let (inner, guard) = guarded();
    let batch = [
        write_item("/ok", &[1], Precondition::Unconditional),
        remove_item("/skills"),
    ];
    assert!(matches!(
        guard.apply_batch(&batch),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(!inner.exists("/ok"));
}

#[test]
fn guard_refuses_a_batch_where_only_the_last_item_touches_its_prefix() {
    let (inner, guard) = guarded();
    inner.write("/a", b"1").unwrap();
    let batch = [
        write_item("/one", &[1], Precondition::Unconditional),
        FsMutation::Mkdir {
            path: "/dir".into(),
        },
        rename_item("/a", "/b"),
        write_item("/skills/x.md", b"tampered", Precondition::Unconditional),
    ];
    assert!(matches!(
        guard.apply_batch(&batch),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(!inner.exists("/one") && !inner.exists("/dir") && !inner.exists("/b"));
    assert_eq!(inner.read("/a").unwrap(), b"1");
    assert!(!inner.exists("/skills/x.md"));
}

#[test]
fn guard_refuses_a_batch_that_removes_or_renames_an_ancestor_of_its_prefix() {
    let inner = Arc::new(MemoryFs::new());
    inner.write("/var/skills/a.md", b"s").unwrap();
    let guard = guarded_over(inner.clone(), "/var/skills");
    for last in [remove_item("/var"), rename_item("/var", "/moved")] {
        let batch = [write_item("/ok", &[1], Precondition::Unconditional), last];
        assert!(matches!(
            guard.apply_batch(&batch),
            Err(VfsError::PermissionDenied(_))
        ));
        assert!(!inner.exists("/ok") && !inner.exists("/moved"));
        assert_eq!(inner.read("/var/skills/a.md").unwrap(), b"s");
    }
}

#[test]
fn guard_refuses_removing_an_ancestor_of_its_prefix() {
    let inner = Arc::new(MemoryFs::new());
    inner.write("/var/skills/a.md", b"s").unwrap();
    let guard = guarded_over(inner.clone(), "/var/skills");
    assert!(matches!(
        guard.remove("/var"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        guard.remove("/"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(inner.exists("/var/skills/a.md"));
}

#[test]
fn guard_still_removes_paths_that_merely_share_a_name_prefix_with_an_ancestor_check() {
    let inner = Arc::new(MemoryFs::new());
    inner.write("/skill/a.md", b"x").unwrap();
    inner.write("/sk", b"y").unwrap();
    inner.write("/skills/a.md", b"s").unwrap();
    let guard = guarded_over(inner.clone(), "/skills");
    inner.write("/skillx", b"z").unwrap();
    guard.rename("/skillx", "/skilly").unwrap();
    assert_eq!(inner.read("/skilly").unwrap(), b"z");
    guard.remove("/skill").unwrap();
    guard.remove("/sk").unwrap();
    assert!(!inner.exists("/skill") && !inner.exists("/sk"));
    assert!(inner.exists("/skills/a.md"));

    let nested = Arc::new(MemoryFs::new());
    nested.write("/va", b"v").unwrap();
    nested.write("/var/other", b"o").unwrap();
    nested.write("/var/skills/a.md", b"s").unwrap();
    let guard = guarded_over(nested.clone(), "/var/skills");
    guard.remove("/va").unwrap();
    guard.remove("/var/other").unwrap();
    assert!(!nested.exists("/va") && !nested.exists("/var/other"));
    assert!(nested.exists("/var/skills/a.md"));
}

#[test]
fn guard_forwards_write_if_and_its_conflict() {
    let (inner, guard) = guarded();
    inner.write("/a", b"1").unwrap();
    let err = guard
        .write_if("/a", b"2", Precondition::MustBeAbsent)
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert_eq!(inner.read("/a").unwrap(), b"1");
}

#[test]
fn guard_forwards_write_if_success_and_revision_checks_unchanged() {
    let (inner, guard) = guarded();
    guard
        .write_if("/a", b"1", Precondition::MustBeAbsent)
        .unwrap();
    let rev = inner.metadata("/a").unwrap().revision.unwrap();
    guard
        .write_if("/a", b"2", Precondition::MustMatch(rev))
        .unwrap();
    let stale = guard
        .write_if("/a", b"3", Precondition::MustMatch(rev))
        .unwrap_err();
    assert!(matches!(stale, VfsError::Conflict(_)), "{stale:?}");
    assert_eq!(inner.read("/a").unwrap(), b"2");
}

#[test]
fn guard_refuses_write_if_inside_its_prefix() {
    let (inner, guard) = guarded();
    for precondition in [Precondition::Unconditional, Precondition::MustBeAbsent] {
        let err = guard
            .write_if("/skills/x.md", b"s", precondition)
            .unwrap_err();
        assert!(matches!(err, VfsError::PermissionDenied(_)), "{err:?}");
    }
    assert!(!inner.exists("/skills/x.md"));
}

#[test]
fn guard_forwarded_batch_is_still_all_or_nothing() {
    let (inner, guard) = guarded();
    inner.write("/taken", b"t").unwrap();
    let batch = [
        write_item("/ok", &[1], Precondition::Unconditional),
        write_item("/taken", b"x", Precondition::MustBeAbsent),
    ];
    let err = guard.apply_batch(&batch).unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert!(!inner.exists("/ok"));
    assert_eq!(inner.read("/taken").unwrap(), b"t");
}

#[test]
fn guard_forwarded_batch_applies_every_item_in_order() {
    let (inner, guard) = guarded();
    let batch = [
        write_item("/a", b"1", Precondition::MustBeAbsent),
        rename_item("/a", "/b"),
        FsMutation::Mkdir { path: "/d".into() },
        write_item("/gone", b"g", Precondition::Unconditional),
        remove_item("/gone"),
    ];
    guard.apply_batch(&batch).unwrap();
    assert_eq!(inner.read("/b").unwrap(), b"1");
    assert!(inner.metadata("/d").unwrap().is_dir);
    assert!(!inner.exists("/a") && !inner.exists("/gone"));
}

#[test]
fn guard_forwards_list_dir_bounded_so_truncation_survives() {
    let inner = TruncatingFs::new();
    inner.write("/a", b"1").unwrap();
    let inner_dyn: Arc<dyn VirtualFs> = Arc::new(inner);
    let guard = ReadOnlyPathGuard::new(inner_dyn, "/skills");
    let listing = guard.list_dir_bounded("/").unwrap();
    assert_eq!(listing.entries, vec!["a".to_string()]);
    assert!(
        listing.truncated,
        "the guard must forward the bound, not hide it"
    );
}

#[test]
fn overlay_answers_unsupported_for_every_atomic_method_and_changes_nothing() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper.clone()));
    overlay.write("/a", b"1").unwrap();
    let batch = [write_item("/c", b"3", Precondition::Unconditional)];

    assert!(matches!(
        overlay.rename("/a", "/b"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        overlay.write_if("/a", b"2", Precondition::MustBeAbsent),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        overlay.write_if("/n", b"2", Precondition::Unconditional),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        overlay.apply_batch(&batch),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        overlay.apply_batch(&[]),
        Err(VfsError::Unsupported(_))
    ));

    assert_eq!(overlay.read("/a").unwrap(), b"1");
    for path in ["/b", "/n", "/c"] {
        assert!(!overlay.exists(path), "{path} must not exist");
        assert!(!lower.exists(path) && !upper.exists(path), "{path} leaked");
    }
    assert!(!lower.exists("/a"), "the lower layer stays read-only");
}

#[test]
fn overlay_bounded_listing_is_its_own_untruncated_listing() {
    let overlay = OverlayFs::new(Box::new(TruncatingFs::new()), Box::new(MemoryFs::new()));
    overlay.write("/a", b"1").unwrap();
    let listing = overlay.list_dir_bounded("/").unwrap();
    assert_eq!(listing.entries, overlay.list_dir("/").unwrap());
    assert!(!listing.truncated);
}
