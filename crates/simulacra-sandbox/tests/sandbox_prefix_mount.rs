//! A rename or batch that crosses a prefix mount is refused for good: the
//! JavaScript `fs` and the batch entry point both fall back to emulating an
//! `Unsupported` move with separate writes and removes, which across two
//! filesystems would leave half a move behind.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_sandbox::{VfsMutation, VfsWritePrecondition};
use simulacra_vfs::PrefixMountFs;

struct Mounted {
    base: Arc<MemoryFs>,
    skills: Arc<MemoryFs>,
    cell: AgentCell,
}

fn mounted() -> Mounted {
    let base = Arc::new(MemoryFs::new());
    base.write("/notes/a.md", b"base").unwrap();
    let skills = Arc::new(MemoryFs::new());
    skills.write("/skills/x/SKILL.md", b"skill").unwrap();
    let fs: Arc<dyn VirtualFs> =
        Arc::new(PrefixMountFs::new(base.clone(), "/skills", skills.clone()));
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    let cell = AgentCell::new(
        fs,
        capability(&["/**"], &["/**"], false, true),
        unlimited_budget(),
        journal,
        http,
    );
    Mounted { base, skills, cell }
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

/// Neither side has changed since `mounted()`.
fn assert_untouched(m: &Mounted) {
    assert_eq!(text(&*m.base, "/notes/a.md"), "base");
    assert_eq!(text(&*m.skills, "/skills/x/SKILL.md"), "skill");
    assert_eq!(m.base.list_dir("/").unwrap(), ["notes"]);
    assert_eq!(m.skills.list_dir("/").unwrap(), ["skills"]);
}

#[test]
fn js_rename_within_one_side_still_works() {
    let m = mounted();

    m.cell
        .execute_js("fs.renameSync('/notes/a.md', '/notes/b.md')")
        .unwrap();
    m.cell
        .execute_js("fs.renameSync('/skills/x/SKILL.md', '/skills/x/RENAMED.md')")
        .unwrap();

    assert_eq!(text(&*m.base, "/notes/b.md"), "base");
    assert!(!m.base.exists("/notes/a.md"));
    assert_eq!(text(&*m.skills, "/skills/x/RENAMED.md"), "skill");
    assert!(!m.skills.exists("/skills/x/SKILL.md"));
}

#[test]
fn js_rename_across_the_mount_throws_and_does_not_copy_then_remove() {
    let m = mounted();

    let into_mount = m
        .cell
        .execute_js("fs.renameSync('/notes/a.md', '/skills/a.md')");
    let out_of_mount = m
        .cell
        .execute_js("fs.renameSync('/skills/x/SKILL.md', '/notes/s.md')");

    assert!(into_mount.is_err(), "a crossing rename must throw");
    assert!(out_of_mount.is_err(), "a crossing rename must throw");
    assert!(!m.skills.exists("/skills/a.md"));
    assert!(!m.base.exists("/notes/s.md"));
    assert_untouched(&m);
}

#[test]
fn js_rename_over_an_existing_file_across_the_mount_leaves_both_files() {
    let m = mounted();

    // The destination exists, so the proxy takes the remove-and-rename batch
    // path rather than a plain rename.
    let result = m
        .cell
        .execute_js("fs.renameSync('/notes/a.md', '/skills/x/SKILL.md')");

    assert!(result.is_err(), "a crossing rename must throw");
    assert_untouched(&m);
}

#[test]
fn a_mutation_batch_across_the_mount_applies_nothing() {
    let m = mounted();

    let result = m.cell.apply_vfs_mutations(
        "apply_patch",
        &[
            VfsMutation::Write {
                path: "/notes/c.md".into(),
                data: b"c".to_vec(),
                precondition: VfsWritePrecondition::Missing,
            },
            VfsMutation::Write {
                path: "/skills/z/SKILL.md".into(),
                data: b"z".to_vec(),
                precondition: VfsWritePrecondition::Missing,
            },
        ],
    );

    assert!(result.is_err(), "a crossing batch must be refused");
    assert!(!m.base.exists("/notes/c.md"));
    assert!(!m.skills.exists("/skills/z/SKILL.md"));
    assert_untouched(&m);
}

#[test]
fn a_move_across_the_mount_applies_nothing() {
    let m = mounted();

    let result = m.cell.apply_vfs_mutations(
        "apply_patch",
        &[VfsMutation::Move {
            from: "/notes/a.md".into(),
            to: "/skills/a.md".into(),
        }],
    );

    assert!(result.is_err(), "a crossing move must be refused");
    assert!(!m.skills.exists("/skills/a.md"));
    assert_untouched(&m);
}

#[test]
fn a_mutation_batch_within_one_side_still_commits() {
    let m = mounted();

    m.cell
        .apply_vfs_mutations(
            "apply_patch",
            &[
                VfsMutation::Write {
                    path: "/skills/y/SKILL.md".into(),
                    data: b"y".to_vec(),
                    precondition: VfsWritePrecondition::Missing,
                },
                VfsMutation::Write {
                    path: "/skills/z/SKILL.md".into(),
                    data: b"z".to_vec(),
                    precondition: VfsWritePrecondition::Missing,
                },
            ],
        )
        .unwrap();

    assert_eq!(text(&*m.skills, "/skills/y/SKILL.md"), "y");
    assert_eq!(text(&*m.skills, "/skills/z/SKILL.md"), "z");
    assert!(!m.base.exists("/skills/y/SKILL.md"));
}
