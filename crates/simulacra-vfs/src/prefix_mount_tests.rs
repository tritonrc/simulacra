use std::sync::Arc;

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VirtualFs,
};

use crate::{MemoryFs, PrefixMountFs};

struct Fixture {
    base: Arc<MemoryFs>,
    skills: Arc<MemoryFs>,
    fs: PrefixMountFs,
}

fn fixture() -> Fixture {
    let base = Arc::new(MemoryFs::new());
    base.write("/notes/a.md", b"base").unwrap();
    let skills = Arc::new(MemoryFs::new());
    skills.write("/skills/x/SKILL.md", b"skill").unwrap();
    let fs = PrefixMountFs::new(base.clone(), "/skills", skills.clone());
    Fixture { base, skills, fs }
}

/// Answers only what a test scripts; every other call is "unused", so a mount
/// that reaches for the wrong method fails loudly instead of passing quietly.
#[derive(Default)]
struct Scripted {
    listing: Option<DirListing>,
    metadata_failure: Option<String>,
    list_failure: Option<String>,
}

fn unused<T>() -> Result<T, VfsError> {
    Err(VfsError::Io("unused".into()))
}

impl VirtualFs for Scripted {
    fn read(&self, _path: &str) -> Result<Vec<u8>, VfsError> {
        unused()
    }
    fn write(&self, _path: &str, _data: &[u8]) -> Result<(), VfsError> {
        unused()
    }
    fn exists(&self, _path: &str) -> bool {
        false
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, VfsError> {
        match &self.list_failure {
            Some(message) => Err(VfsError::Io(message.clone())),
            None => unused(),
        }
    }
    fn mkdir(&self, _path: &str) -> Result<(), VfsError> {
        unused()
    }
    fn remove(&self, _path: &str) -> Result<(), VfsError> {
        unused()
    }
    fn metadata(&self, _path: &str) -> Result<FsMetadata, VfsError> {
        match &self.metadata_failure {
            Some(message) => Err(VfsError::Io(message.clone())),
            None => unused(),
        }
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        unused()
    }
    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        unused()
    }
    fn list_dir_bounded(&self, _path: &str) -> Result<DirListing, VfsError> {
        if let Some(message) = &self.list_failure {
            return Err(VfsError::Io(message.clone()));
        }
        self.listing.clone().map_or_else(unused, Ok)
    }
}

fn write_item(path: &str, data: &[u8]) -> FsMutation {
    FsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition: Precondition::MustBeAbsent,
    }
}

fn remove_item(path: &str) -> FsMutation {
    FsMutation::Remove {
        path: path.into(),
        precondition: Precondition::Unconditional,
    }
}

/// (limit, base entries, base truncated, expected entries, expected truncated)
type ListingCase = (
    Option<usize>,
    &'static [&'static str],
    bool,
    &'static [&'static str],
    bool,
);

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| (*entry).to_owned()).collect()
}

fn is_not_found<T: std::fmt::Debug>(result: Result<T, VfsError>) -> bool {
    matches!(result, Err(VfsError::NotFound(_)))
}

fn assert_denied<T: std::fmt::Debug>(result: Result<T, VfsError>) {
    assert!(
        matches!(result, Err(VfsError::PermissionDenied(_))),
        "expected PermissionDenied, got {result:?}"
    );
}

/// Neither side has changed since `fixture()`.
fn assert_untouched(f: &Fixture) {
    assert_eq!(f.base.read("/notes/a.md").unwrap(), b"base");
    assert_eq!(f.skills.read("/skills/x/SKILL.md").unwrap(), b"skill");
    assert_eq!(f.base.list_dir("/").unwrap(), ["notes"]);
    assert_eq!(f.skills.list_dir("/").unwrap(), ["skills"]);
}

#[test]
fn reads_under_the_prefix_come_from_the_mount_and_others_from_the_base() {
    let f = fixture();

    assert_eq!(f.fs.read("/skills/x/SKILL.md").unwrap(), b"skill");
    assert_eq!(f.fs.read("/notes/a.md").unwrap(), b"base");
    // Routing happens after normalisation, so `..` cannot step around the mount.
    assert_eq!(f.fs.read("/notes/../skills/x/SKILL.md").unwrap(), b"skill");
}

#[test]
fn a_sibling_that_merely_starts_with_the_prefix_stays_on_the_base() {
    let f = fixture();

    f.fs.write("/skillset/a.md", b"sibling").unwrap();

    assert_eq!(f.base.read("/skillset/a.md").unwrap(), b"sibling");
    assert!(is_not_found(f.skills.read("/skillset/a.md")));
}

#[test]
fn the_root_listing_shows_the_mount_once() {
    let f = fixture();

    assert_eq!(sorted(f.fs.list_dir("/").unwrap()), ["notes", "skills"]);

    // A stray /skills directory in the base must not produce a second entry.
    f.base.mkdir("/skills").unwrap();
    assert_eq!(sorted(f.fs.list_dir("/").unwrap()), ["notes", "skills"]);
    let bounded = f.fs.list_dir_bounded("/").unwrap();
    assert_eq!(sorted(bounded.entries), ["notes", "skills"]);
    assert!(!bounded.truncated);
}

#[test]
fn the_root_listing_shows_the_mount_even_when_it_is_empty() {
    let base: Arc<MemoryFs> = Arc::new(MemoryFs::new());
    base.write("/notes/a.md", b"base").unwrap();
    let fs = PrefixMountFs::new(base, "/skills", Arc::new(MemoryFs::new()));

    assert_eq!(sorted(fs.list_dir("/").unwrap()), ["notes", "skills"]);
}

#[test]
fn a_root_listing_never_exceeds_the_listing_limit() {
    let cases: [ListingCase; 8] = [
        // The limit leaves room for the mount alone: every base entry is displaced.
        (Some(1), &["a"], false, &["skills"], true),
        (Some(3), &["a", "b", "c"], true, &["a", "b", "skills"], true),
        // Exactly at the limit once the mount is added: a base entry is displaced.
        (
            Some(3),
            &["a", "b", "c"],
            false,
            &["a", "b", "skills"],
            true,
        ),
        (Some(3), &["a", "b"], false, &["a", "b", "skills"], false),
        // The base's own truncation survives even when nothing is displaced.
        (Some(3), &["a", "b"], true, &["a", "b", "skills"], true),
        // A stray base entry for the mount is deduplicated and the rest still fits.
        (
            Some(3),
            &["a", "skills", "b"],
            false,
            &["a", "b", "skills"],
            false,
        ),
        (
            None,
            &["a", "b", "c"],
            false,
            &["a", "b", "c", "skills"],
            false,
        ),
        (None, &["a", "b"], true, &["a", "b", "skills"], true),
    ];
    for (limit, base_entries, base_truncated, expected, expected_truncated) in cases {
        let base = Arc::new(Scripted {
            listing: Some(DirListing {
                entries: names(base_entries),
                truncated: base_truncated,
            }),
            ..Scripted::default()
        });
        let mut fs = PrefixMountFs::new(base, "/skills", Arc::new(MemoryFs::new()));
        if let Some(limit) = limit {
            fs = fs.with_listing_limit(limit);
        }

        let listing = fs.list_dir_bounded("/").unwrap();

        assert_eq!(
            (listing.entries, listing.truncated),
            (names(expected), expected_truncated),
            "limit {limit:?}, base {base_entries:?} truncated {base_truncated}"
        );
    }
}

#[test]
fn listing_a_directory_goes_to_the_side_that_owns_it() {
    let f = fixture();
    // The base's own /skills content must never leak into the mounted listing.
    f.base.write("/skills/stray.md", b"stray").unwrap();

    assert_eq!(f.fs.list_dir("/skills").unwrap(), ["x"]);
    assert_eq!(f.fs.list_dir("/skills/x").unwrap(), ["SKILL.md"]);
    assert_eq!(f.fs.list_dir("/notes").unwrap(), ["a.md"]);
    assert_eq!(f.fs.list_dir_bounded("/skills").unwrap().entries, ["x"]);
}

#[test]
fn listing_the_mount_point_of_an_empty_mount_is_empty_not_missing() {
    let base = Arc::new(MemoryFs::new());
    let fs = PrefixMountFs::new(base, "/skills", Arc::new(MemoryFs::new()));

    // The mount point exists, is a directory and is listed at the root, so
    // listing it must not answer NotFound.
    assert!(fs.exists("/skills"));
    assert_eq!(fs.list_dir("/skills").unwrap(), Vec::<String>::new());
    assert_eq!(
        fs.list_dir_bounded("/skills").unwrap(),
        DirListing {
            entries: vec![],
            truncated: false
        }
    );
}

#[test]
fn listing_the_mount_point_after_its_directory_was_removed_is_empty() {
    let f = fixture();
    f.skills.remove("/skills").unwrap();

    assert_eq!(f.fs.list_dir("/skills").unwrap(), Vec::<String>::new());
    assert_eq!(
        f.fs.list_dir_bounded("/skills").unwrap(),
        DirListing {
            entries: vec![],
            truncated: false
        }
    );
}

#[test]
fn a_mount_point_listing_error_other_than_not_found_is_reported() {
    let mounted = Arc::new(Scripted {
        list_failure: Some("down".into()),
        ..Scripted::default()
    });
    let fs = PrefixMountFs::new(Arc::new(MemoryFs::new()), "/skills", mounted);

    // An outage must not read as an empty directory.
    match fs.list_dir("/skills") {
        Err(VfsError::Io(message)) => assert_eq!(message, "down"),
        other => panic!("expected the mount's Io error, got {other:?}"),
    }
    match fs.list_dir_bounded("/skills") {
        Err(VfsError::Io(message)) => assert_eq!(message, "down"),
        other => panic!("expected the mount's Io error, got {other:?}"),
    }
}

#[test]
fn a_missing_directory_below_the_mount_point_is_still_not_found() {
    let fs = PrefixMountFs::new(
        Arc::new(MemoryFs::new()),
        "/skills",
        Arc::new(MemoryFs::new()),
    );

    assert!(is_not_found(fs.list_dir("/skills/missing")));
    assert!(is_not_found(fs.list_dir_bounded("/skills/missing")));
}

#[test]
fn the_mount_point_is_a_directory_even_when_the_mount_has_no_entry_for_it() {
    let f = fixture();
    assert!(f.fs.metadata("/skills").unwrap().is_dir);
    assert!(f.fs.exists("/skills"));

    let empty = PrefixMountFs::new(f.base.clone(), "/skills", Arc::new(MemoryFs::new()));

    assert!(empty.metadata("/skills").unwrap().is_dir);
    assert!(empty.exists("/skills"));
}

#[test]
fn a_mount_point_error_other_than_not_found_is_reported() {
    let mounted = Arc::new(Scripted {
        metadata_failure: Some("backend down".into()),
        ..Scripted::default()
    });
    let fs = PrefixMountFs::new(Arc::new(MemoryFs::new()), "/skills", mounted);

    // Answering "a directory" here would hide an outage behind a healthy mount.
    match fs.metadata("/skills") {
        Err(VfsError::Io(message)) => assert_eq!(message, "backend down"),
        other => panic!("expected the mount's Io error, got {other:?}"),
    }
}

#[test]
fn writes_route_by_path() {
    let f = fixture();

    f.fs.write("/skills/y/SKILL.md", b"n").unwrap();
    f.fs.write_if("/notes/b.md", b"b", Precondition::MustBeAbsent)
        .unwrap();
    f.fs.mkdir("/skills/d").unwrap();

    assert_eq!(f.skills.read("/skills/y/SKILL.md").unwrap(), b"n");
    assert!(is_not_found(f.base.read("/skills/y/SKILL.md")));
    assert_eq!(f.base.read("/notes/b.md").unwrap(), b"b");
    assert!(is_not_found(f.skills.read("/notes/b.md")));
    assert!(f.skills.metadata("/skills/d").unwrap().is_dir);
    assert!(!f.base.exists("/skills/d"));
    // A conditional write keeps its precondition on the routed side.
    assert!(
        f.fs.write_if("/notes/b.md", b"again", Precondition::MustBeAbsent)
            .is_err()
    );
    assert_eq!(f.base.read("/notes/b.md").unwrap(), b"b");
}

#[test]
fn removes_route_by_path() {
    let f = fixture();

    f.fs.remove("/skills/x/SKILL.md").unwrap();
    assert!(!f.skills.exists("/skills/x/SKILL.md"));
    assert_eq!(f.base.read("/notes/a.md").unwrap(), b"base");

    f.fs.remove("/notes/a.md").unwrap();
    assert!(!f.base.exists("/notes/a.md"));
}

#[test]
fn rename_and_batch_across_the_mount_are_refused_as_permission_denied_and_change_nothing() {
    let f = fixture();

    // PermissionDenied, not Unsupported: callers emulate an unsupported rename
    // or batch with separate writes and removes, which would half-move a file.
    assert_denied(f.fs.rename("/notes/a.md", "/skills/a.md"));
    assert!(!f.skills.exists("/skills/a.md"));
    assert_untouched(&f);

    assert_denied(f.fs.rename("/skills/x/SKILL.md", "/notes/s.md"));
    assert!(!f.base.exists("/notes/s.md"));
    assert_untouched(&f);

    assert_denied(f.fs.apply_batch(&[
        write_item("/notes/c.md", b"c"),
        write_item("/skills/z/SKILL.md", b"z"),
    ]));
    assert!(!f.base.exists("/notes/c.md"));
    assert!(!f.skills.exists("/skills/z/SKILL.md"));

    // A rename item that spans the two sides is a crossing too.
    assert_denied(f.fs.apply_batch(&[FsMutation::Rename {
        from: "/notes/a.md".into(),
        to: "/skills/a.md".into(),
        precondition: Precondition::Unconditional,
    }]));
    assert_untouched(&f);
}

#[test]
fn rename_within_each_side_works() {
    let f = fixture();

    f.fs.rename("/notes/a.md", "/notes/b.md").unwrap();
    f.fs.rename("/skills/x/SKILL.md", "/skills/x/RENAMED.md")
        .unwrap();

    assert_eq!(f.base.read("/notes/b.md").unwrap(), b"base");
    assert!(!f.base.exists("/notes/a.md"));
    assert_eq!(f.skills.read("/skills/x/RENAMED.md").unwrap(), b"skill");
    assert!(!f.skills.exists("/skills/x/SKILL.md"));
}

#[test]
fn a_batch_wholly_on_one_side_commits_there() {
    let f = fixture();

    f.fs.apply_batch(&[
        write_item("/notes/c.md", b"c"),
        write_item("/notes/d.md", b"d"),
    ])
    .unwrap();
    f.fs.apply_batch(&[
        write_item("/skills/y/SKILL.md", b"y"),
        write_item("/skills/z/SKILL.md", b"z"),
    ])
    .unwrap();

    assert_eq!(f.base.read("/notes/c.md").unwrap(), b"c");
    assert_eq!(f.base.read("/notes/d.md").unwrap(), b"d");
    assert_eq!(f.skills.read("/skills/y/SKILL.md").unwrap(), b"y");
    assert_eq!(f.skills.read("/skills/z/SKILL.md").unwrap(), b"z");
    assert!(!f.base.exists("/skills/y/SKILL.md"));
    assert!(!f.skills.exists("/notes/c.md"));
}

#[test]
fn a_batch_is_judged_on_normalised_paths() {
    let f = fixture();

    // Spelled with `..`, the first item is on the mount; the second is on the base.
    assert_denied(f.fs.apply_batch(&[
        write_item("/notes/../skills/y/SKILL.md", b"y"),
        write_item("/notes/c.md", b"c"),
    ]));

    assert!(!f.base.exists("/notes/c.md"));
    assert!(!f.skills.exists("/skills/y/SKILL.md"));
}

#[test]
fn anything_that_removes_or_moves_the_root_is_refused() {
    let f = fixture();

    // Taking `/` would take the mount with it, from a base that cannot see it.
    assert_denied(f.fs.remove("/"));
    assert_untouched(&f);

    assert_denied(f.fs.rename("/", "/elsewhere"));
    assert_untouched(&f);

    assert_denied(f.fs.apply_batch(&[remove_item("/")]));
    assert_untouched(&f);

    // `/notes/..` is `/` once normalised.
    assert_denied(f.fs.apply_batch(&[remove_item("/notes/..")]));
    assert_untouched(&f);
}

#[test]
#[should_panic(expected = "room for the mount")]
fn a_listing_limit_of_zero_is_a_programming_error() {
    let f = fixture();
    let _ = f.fs.with_listing_limit(0);
}

#[test]
#[should_panic(expected = "top-level")]
fn a_nested_prefix_is_a_programming_error() {
    let _ = PrefixMountFs::new(Arc::new(MemoryFs::new()), "/a/b", Arc::new(MemoryFs::new()));
}

#[test]
#[should_panic(expected = "top-level")]
fn the_root_is_not_a_mount_prefix() {
    let _ = PrefixMountFs::new(Arc::new(MemoryFs::new()), "/", Arc::new(MemoryFs::new()));
}
