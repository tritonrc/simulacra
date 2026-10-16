use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};

use crate::MemoryFs;

/// Every entry as (path, is_file, bytes), sorted, so two trees compare equal
/// exactly when nothing about them differs.
fn tree(fs: &MemoryFs) -> Vec<(String, bool, Vec<u8>)> {
    let snapshot = fs.snapshot().unwrap();
    let mut items: Vec<(String, bool, Vec<u8>)> = serde_json::from_slice(&snapshot.data).unwrap();
    items.sort();
    items
}

fn fs_with_file_f() -> MemoryFs {
    let fs = MemoryFs::new();
    fs.write("/f", b"file").unwrap();
    fs.write("/a", b"a").unwrap();
    fs
}

#[test]
fn write_if_below_a_file_is_not_a_directory_and_changes_nothing() {
    let fs = fs_with_file_f();
    let before = tree(&fs);
    let err = fs
        .write_if("/f/x", b"y", Precondition::Unconditional)
        .unwrap_err();
    assert!(matches!(err, VfsError::NotADirectory(_)), "{err:?}");
    assert_eq!(tree(&fs), before);
    assert_eq!(fs.read("/f").unwrap(), b"file");
}

#[test]
fn rename_to_a_target_below_a_file_is_not_a_directory_and_changes_nothing() {
    let fs = fs_with_file_f();
    let before = tree(&fs);
    let err = fs.rename("/a", "/f/x").unwrap_err();
    assert!(matches!(err, VfsError::NotADirectory(_)), "{err:?}");
    assert_eq!(tree(&fs), before);
    assert_eq!(fs.read("/a").unwrap(), b"a");
}

#[test]
fn a_mkdir_batch_below_a_file_is_not_a_directory_and_changes_nothing() {
    let fs = fs_with_file_f();
    let before = tree(&fs);
    let err = fs
        .apply_batch(&[FsMutation::Mkdir {
            path: "/f/d".into(),
        }])
        .unwrap_err();
    assert!(matches!(err, VfsError::NotADirectory(_)), "{err:?}");
    assert_eq!(tree(&fs), before);
}

#[test]
fn a_bad_second_item_rolls_back_the_valid_first_item() {
    let fs = fs_with_file_f();
    let before = tree(&fs);
    let batch = [
        FsMutation::Write {
            path: "/first".into(),
            data: b"1".to_vec(),
            precondition: Precondition::Unconditional,
        },
        FsMutation::Write {
            path: "/f/x".into(),
            data: b"2".to_vec(),
            precondition: Precondition::Unconditional,
        },
    ];
    let err = fs.apply_batch(&batch).unwrap_err();
    assert!(matches!(err, VfsError::NotADirectory(_)), "{err:?}");
    assert!(!fs.exists("/first"), "the first item must be rolled back");
    assert_eq!(tree(&fs), before);
}
