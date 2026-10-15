use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};

use crate::MemoryFs;

fn rev(fs: &MemoryFs, path: &str) -> u64 {
    fs.metadata(path)
        .unwrap()
        .revision
        .expect("MemoryFs files carry revisions")
}

#[test]
fn every_write_gets_a_new_revision_and_revisions_never_repeat() {
    let fs = MemoryFs::new();
    fs.write("/a", b"1").unwrap();
    let first = rev(&fs, "/a");
    fs.write("/a", b"2").unwrap();
    let second = rev(&fs, "/a");
    assert!(second > first);
    fs.remove("/a").unwrap();
    fs.write("/a", b"3").unwrap();
    assert!(
        rev(&fs, "/a") > second,
        "a re-created file must not reuse an old revision"
    );
}

#[test]
fn write_if_must_match_refuses_a_stale_revision_and_changes_nothing() {
    let fs = MemoryFs::new();
    fs.write("/a", b"old").unwrap();
    let stale = rev(&fs, "/a");
    fs.write("/a", b"new").unwrap();
    let err = fs
        .write_if("/a", b"lost", Precondition::MustMatch(stale))
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert_eq!(fs.read("/a").unwrap(), b"new");
}

#[test]
fn write_if_must_match_accepts_the_current_revision_and_advances_it() {
    let fs = MemoryFs::new();
    fs.write("/a", b"old").unwrap();
    let current = rev(&fs, "/a");
    fs.write_if("/a", b"new", Precondition::MustMatch(current))
        .unwrap();
    assert_eq!(fs.read("/a").unwrap(), b"new");
    assert!(
        rev(&fs, "/a") > current,
        "a successful write is a new revision"
    );
    let err = fs
        .write_if("/a", b"again", Precondition::MustMatch(current))
        .unwrap_err();
    assert!(
        matches!(err, VfsError::Conflict(_)),
        "the old revision is stale now: {err:?}"
    );
}

#[test]
fn write_if_must_match_on_a_missing_file_is_a_conflict() {
    let fs = MemoryFs::new();
    let err = fs
        .write_if("/missing", b"x", Precondition::MustMatch(1))
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert!(!fs.exists("/missing"));
}

#[test]
fn write_if_must_be_absent_refuses_an_existing_file() {
    let fs = MemoryFs::new();
    fs.write("/a", b"x").unwrap();
    let err = fs
        .write_if("/a", b"y", Precondition::MustBeAbsent)
        .unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    fs.write_if("/b", b"y", Precondition::MustBeAbsent).unwrap();
    assert_eq!(fs.read("/b").unwrap(), b"y");
}

#[test]
fn write_if_creates_missing_parent_directories() {
    let fs = MemoryFs::new();
    fs.write_if("/p/q/file", b"x", Precondition::MustBeAbsent)
        .unwrap();
    assert!(fs.exists("/p") && fs.exists("/p/q"));
    assert_eq!(fs.read("/p/q/file").unwrap(), b"x");
}

#[test]
fn write_if_on_a_directory_path_is_not_a_file_and_keeps_the_directory() {
    let fs = MemoryFs::new();
    fs.write("/d/x", b"1").unwrap();
    let err = fs
        .write_if("/d", b"y", Precondition::Unconditional)
        .unwrap_err();
    assert!(matches!(err, VfsError::NotAFile(_)), "{err:?}");
    assert_eq!(fs.read("/d/x").unwrap(), b"1");
}

#[test]
fn rename_moves_a_directory_tree_and_refuses_an_existing_target() {
    let fs = MemoryFs::new();
    fs.write("/d/x", b"1").unwrap();
    fs.write("/d/sub/y", b"2").unwrap();
    fs.write("/taken", b"t").unwrap();
    assert!(matches!(
        fs.rename("/d", "/taken"),
        Err(VfsError::Conflict(_))
    ));
    fs.rename("/d", "/e").unwrap();
    assert!(!fs.exists("/d") && !fs.exists("/d/x"));
    assert_eq!(fs.read("/e/x").unwrap(), b"1");
    assert_eq!(fs.read("/e/sub/y").unwrap(), b"2");
}

#[test]
fn rename_of_a_missing_source_is_not_found() {
    let fs = MemoryFs::new();
    fs.write("/other", b"o").unwrap();
    let err = fs.rename("/missing", "/dest").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)), "{err:?}");
    assert!(!fs.exists("/dest"));
}

#[test]
fn rename_refuses_moving_a_directory_into_itself() {
    let fs = MemoryFs::new();
    fs.write("/d/x", b"1").unwrap();
    let err = fs.rename("/d", "/d/inner").unwrap_err();
    assert!(matches!(err, VfsError::Io(_)), "{err:?}");
    assert_eq!(fs.read("/d/x").unwrap(), b"1");
    assert!(!fs.exists("/d/inner"));
}

#[test]
fn a_batch_with_one_failing_precondition_applies_nothing() {
    let fs = MemoryFs::new();
    fs.write("/keep", b"k").unwrap();
    let batch = [
        FsMutation::Write {
            path: "/new".into(),
            data: b"n".to_vec(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Remove {
            path: "/keep".into(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Write {
            path: "/keep".into(),
            data: b"x".to_vec(),
            precondition: Precondition::MustBeAbsent,
        },
        FsMutation::Write {
            path: "/new".into(),
            data: b"y".to_vec(),
            precondition: Precondition::MustBeAbsent,
        },
    ];
    let err = fs.apply_batch(&batch).unwrap_err();
    assert!(matches!(err, VfsError::Conflict(_)), "{err:?}");
    assert!(!fs.exists("/new"), "no item of a failed batch may land");
    assert_eq!(fs.read("/keep").unwrap(), b"k");
}

#[test]
fn a_batch_sees_its_own_earlier_items() {
    let fs = MemoryFs::new();
    let batch = [
        FsMutation::Write {
            path: "/a".into(),
            data: b"1".to_vec(),
            precondition: Precondition::MustBeAbsent,
        },
        FsMutation::Rename {
            from: "/a".into(),
            to: "/b".into(),
            precondition: Precondition::Unconditional,
        },
    ];
    fs.apply_batch(&batch).unwrap();
    assert!(!fs.exists("/a"));
    assert_eq!(fs.read("/b").unwrap(), b"1");
}

#[test]
fn restore_gives_files_fresh_revisions_above_any_seen_before() {
    let fs = MemoryFs::new();
    fs.write("/a", b"1").unwrap();
    fs.write("/b", b"2").unwrap();
    let snapshot = fs.snapshot().unwrap();
    fs.write("/a", b"changed").unwrap();
    fs.write("/c", b"later").unwrap();
    let seen_before_restore = ["/a", "/b", "/c"]
        .map(|p| rev(&fs, p))
        .into_iter()
        .max()
        .unwrap();

    fs.restore(&snapshot).unwrap();

    assert_eq!(fs.read("/a").unwrap(), b"1");
    let (a, b) = (rev(&fs, "/a"), rev(&fs, "/b"));
    assert!(a > seen_before_restore, "{a} <= {seen_before_restore}");
    assert!(b > seen_before_restore, "{b} <= {seen_before_restore}");
    assert_ne!(a, b, "two restored files never share a revision");
}

#[test]
fn readers_never_see_half_a_batch() {
    const ROUNDS: usize = 3_000;
    let fs = Arc::new(MemoryFs::new());
    fs.write("/a", b"AAAA").unwrap();
    fs.write("/b", b"").unwrap();
    let done = Arc::new(AtomicBool::new(false));

    let writer = {
        let (fs, done) = (Arc::clone(&fs), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut outcome = Ok(());
            for round in 0..ROUNDS {
                let (a, b): (&[u8], &[u8]) = if round % 2 == 0 {
                    (b"", b"AAAA")
                } else {
                    (b"AAAA", b"")
                };
                let batch = [
                    FsMutation::Write {
                        path: "/a".into(),
                        data: a.to_vec(),
                        precondition: Precondition::Unconditional,
                    },
                    FsMutation::Write {
                        path: "/b".into(),
                        data: b.to_vec(),
                        precondition: Precondition::Unconditional,
                    },
                ];
                if let Err(err) = fs.apply_batch(&batch) {
                    outcome = Err(err);
                    break;
                }
            }
            done.store(true, Ordering::SeqCst);
            outcome
        })
    };

    // A snapshot is taken under one read lock, so it is the only way for a
    // reader to observe both files at a single instant.
    let mut observed = 0usize;
    while !done.load(Ordering::SeqCst) {
        let snapshot = fs.snapshot().unwrap();
        let items: Vec<(String, bool, Vec<u8>)> = serde_json::from_slice(&snapshot.data).unwrap();
        let content = |name: &str| {
            items
                .iter()
                .find(|(p, ..)| p == name)
                .unwrap_or_else(|| panic!("{name} missing from snapshot"))
                .2
                .clone()
        };
        let pair = (content("/a"), content("/b"));
        let valid =
            pair == (b"AAAA".to_vec(), Vec::new()) || pair == (Vec::new(), b"AAAA".to_vec());
        assert!(valid, "a reader saw half a batch: {pair:?}");
        observed += 1;
    }

    writer
        .join()
        .expect("writer thread panicked")
        .expect("every batch applies");
    assert!(
        observed > 0,
        "the reader must have observed the writer at work"
    );
}

#[test]
fn list_dir_bounded_on_memory_fs_is_never_truncated() {
    let fs = MemoryFs::new();
    for i in 0..1_500 {
        fs.write(&format!("/many/{i:04}"), b"").unwrap();
    }
    let listing = fs.list_dir_bounded("/many").unwrap();
    assert_eq!((listing.entries.len(), listing.truncated), (1_500, false));
}
