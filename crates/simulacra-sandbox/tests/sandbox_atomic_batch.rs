//! `AgentCell::apply_vfs_mutations` over filesystems with atomic batches: one
//! `apply_batch` commit with revision-pinned preconditions, and the
//! execute-and-roll-back path only where batches are unsupported.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_sandbox::{SandboxError, VfsMutation, VfsWritePrecondition};
use simulacra_types::{FsMutation, Precondition};
use simulacra_vfs::OverlayFs;
use std::sync::atomic::Ordering::SeqCst;

enum Mode {
    Plain,
    /// The first `apply_batch` is preceded by a competing writer replacing
    /// this path with `theirs`, as if it landed after prepare read the revision.
    ReplaceBeforeBatch(&'static str),
    NoRevisions,
    BatchFails,
    NoBatchFailingWrite(usize),
}

/// A `MemoryFs` recording batches and counting per-item primitives.
struct Probe {
    inner: Arc<MemoryFs>,
    mode: Mode,
    writes: AtomicUsize,
    removes: AtomicUsize,
    renames: AtomicUsize,
    batches: Mutex<Vec<Vec<FsMutation>>>,
    raced: AtomicBool,
}

impl Probe {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryFs::new()),
            mode,
            writes: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            renames: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
            raced: AtomicBool::new(false),
        })
    }

    fn revision(&self, path: &str) -> u64 {
        self.inner.metadata(path).unwrap().revision.unwrap()
    }

    fn batches(&self) -> Vec<Vec<FsMutation>> {
        self.batches.lock().unwrap().clone()
    }

    fn per_item_calls(&self) -> usize {
        self.writes.load(SeqCst) + self.removes.load(SeqCst) + self.renames.load(SeqCst)
    }
}

impl VirtualFs for Probe {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let nth = self.writes.fetch_add(1, SeqCst) + 1;
        if matches!(self.mode, Mode::NoBatchFailingWrite(n) if n == nth) {
            return Err(VfsError::Io(format!("injected write failure on {path}")));
        }
        self.inner.write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.removes.fetch_add(1, SeqCst);
        self.inner.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let mut meta = self.inner.metadata(path)?;
        if matches!(self.mode, Mode::NoRevisions) {
            meta.revision = None;
        }
        Ok(meta)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.renames.fetch_add(1, SeqCst);
        self.inner.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        if matches!(self.mode, Mode::NoBatchFailingWrite(_)) {
            return Err(VfsError::Unsupported("apply_batch".to_owned()));
        }
        self.batches.lock().unwrap().push(mutations.to_vec());
        if let Mode::ReplaceBeforeBatch(path) = self.mode
            && !self.raced.swap(true, SeqCst)
        {
            self.inner.write(path, b"theirs").unwrap();
        }
        if matches!(self.mode, Mode::BatchFails) {
            return Err(VfsError::Io("injected batch failure".to_owned()));
        }
        self.inner.apply_batch(mutations)
    }
}

fn cell_over(
    fs: Arc<dyn VirtualFs>,
    budget: Arc<Mutex<ResourceBudget>>,
    journal: Arc<FakeJournalStorage>,
) -> AgentCell {
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(
        fs,
        capability(&["/**"], &["/**"], false, true),
        budget,
        journal,
        http,
    )
}

fn plain_cell(fs: Arc<dyn VirtualFs>) -> AgentCell {
    cell_over(
        fs,
        unlimited_budget(),
        Arc::new(FakeJournalStorage::default()),
    )
}

fn write(path: &str, data: &[u8], precondition: VfsWritePrecondition) -> VfsMutation {
    VfsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition,
    }
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

/// Journal entries recorded as failed tool results.
fn error_results(journal: &FakeJournalStorage) -> Vec<String> {
    journal
        .entries()
        .into_iter()
        .filter_map(|e| match e.entry {
            JournalEntryKind::ToolResult {
                content, is_error, ..
            } if is_error => Some(content),
            _ => None,
        })
        .collect()
}

#[test]
fn a_mutation_batch_commits_through_exactly_one_apply_batch() {
    let fs = Probe::new(Mode::Plain);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    let rev = fs.revision("/a");
    cell.apply_vfs_mutations(
        "apply_patch",
        &[
            write("/a", b"new", VfsWritePrecondition::Matches(b"old".to_vec())),
            write("/b", b"b", VfsWritePrecondition::Missing),
            write("/c", b"c", VfsWritePrecondition::Any),
        ],
    )
    .unwrap();
    assert_eq!(
        fs.batches(),
        vec![vec![
            FsMutation::Write {
                path: "/a".into(),
                data: b"new".to_vec(),
                precondition: Precondition::MustMatch(rev),
            },
            FsMutation::Write {
                path: "/b".into(),
                data: b"b".to_vec(),
                precondition: Precondition::MustBeAbsent,
            },
            FsMutation::Write {
                path: "/c".into(),
                data: b"c".to_vec(),
                precondition: Precondition::Unconditional,
            },
        ]]
    );
    assert_eq!(fs.per_item_calls(), 0, "no per-item write/remove/rename");
    assert_eq!(text(&*fs, "/a"), "new");
    assert_eq!(text(&*fs, "/b"), "b");
}

#[test]
fn a_write_landing_after_prepare_fails_the_batch_and_the_other_writers_bytes_survive() {
    let fs = Probe::new(Mode::ReplaceBeforeBatch("/a"));
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[
                write(
                    "/a",
                    b"mine",
                    VfsWritePrecondition::Matches(b"old".to_vec()),
                ),
                write("/b", b"b", VfsWritePrecondition::Missing),
            ],
        )
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::Vfs(VfsError::Conflict(_))),
        "{err}"
    );
    assert_eq!(
        text(&*fs, "/a"),
        "theirs",
        "the other writer's bytes survive"
    );
    assert!(!fs.exists("/b"), "the batch is all or nothing");
    assert_eq!(
        fs.per_item_calls(),
        0,
        "a failed atomic batch is not rolled back"
    );
}

#[test]
fn delete_commits_a_remove_pinned_to_the_revision_seen_at_prepare() {
    let fs = Probe::new(Mode::Plain);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"x").unwrap();
    let rev = fs.revision("/a");
    cell.apply_vfs_mutations("remove_path", &[VfsMutation::Delete { path: "/a".into() }])
        .unwrap();
    assert_eq!(
        fs.batches(),
        vec![vec![FsMutation::Remove {
            path: "/a".into(),
            precondition: Precondition::MustMatch(rev),
        }]]
    );
    assert_eq!(fs.per_item_calls(), 0);
    assert!(!fs.exists("/a"));
}

#[test]
fn move_commits_one_rename_pinned_to_the_source_revision() {
    let fs = Probe::new(Mode::Plain);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"x").unwrap();
    let rev = fs.revision("/a");
    cell.apply_vfs_mutations(
        "move_path",
        &[VfsMutation::Move {
            from: "/a".into(),
            to: "/d/b".into(),
        }],
    )
    .unwrap();
    assert_eq!(
        fs.batches(),
        vec![vec![FsMutation::Rename {
            from: "/a".into(),
            to: "/d/b".into(),
            precondition: Precondition::MustMatch(rev),
        }]]
    );
    assert_eq!(fs.per_item_calls(), 0, "rename is not write + remove");
    assert_eq!(text(&*fs, "/d/b"), "x");
    assert!(!fs.exists("/a"));
}

#[test]
fn move_and_write_commits_an_absent_write_then_a_pinned_remove() {
    let fs = Probe::new(Mode::Plain);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    let rev = fs.revision("/a");
    cell.apply_vfs_mutations(
        "apply_patch",
        &[VfsMutation::MoveAndWrite {
            from: "/a".into(),
            to: "/b".into(),
            data: b"new".to_vec(),
            from_precondition: Some(b"old".to_vec()),
        }],
    )
    .unwrap();
    assert_eq!(
        fs.batches(),
        vec![vec![
            FsMutation::Write {
                path: "/b".into(),
                data: b"new".to_vec(),
                precondition: Precondition::MustBeAbsent,
            },
            FsMutation::Remove {
                path: "/a".into(),
                precondition: Precondition::MustMatch(rev),
            },
        ]]
    );
    assert_eq!(fs.per_item_calls(), 0);
    assert_eq!(text(&*fs, "/b"), "new");
    assert!(!fs.exists("/a"));
}

#[test]
fn without_revisions_matches_delete_and_move_become_unconditional() {
    let fs = Probe::new(Mode::NoRevisions);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    fs.inner.write("/d", b"d").unwrap();
    fs.inner.write("/m", b"m").unwrap();
    cell.apply_vfs_mutations(
        "apply_patch",
        &[
            write("/a", b"new", VfsWritePrecondition::Matches(b"old".to_vec())),
            VfsMutation::Delete { path: "/d".into() },
            VfsMutation::Move {
                from: "/m".into(),
                to: "/m2".into(),
            },
        ],
    )
    .unwrap();
    assert_eq!(
        fs.batches(),
        vec![vec![
            FsMutation::Write {
                path: "/a".into(),
                data: b"new".to_vec(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Remove {
                path: "/d".into(),
                precondition: Precondition::Unconditional,
            },
            FsMutation::Rename {
                from: "/m".into(),
                to: "/m2".into(),
                precondition: Precondition::Unconditional,
            },
        ]]
    );
    assert_eq!(text(&*fs, "/a"), "new");
}

#[test]
fn without_revisions_the_bytes_are_still_validated_at_prepare() {
    let fs = Probe::new(Mode::NoRevisions);
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"actual").unwrap();
    assert!(
        cell.apply_vfs_mutations(
            "apply_patch",
            &[write(
                "/a",
                b"new",
                VfsWritePrecondition::Matches(b"stale".to_vec())
            )],
        )
        .is_err()
    );
    assert!(fs.batches().is_empty(), "nothing reaches the commit");
    assert_eq!(text(&*fs, "/a"), "actual");
}

#[test]
fn a_conflicting_batch_releases_the_write_budget_and_journals_the_failure() {
    let fs = Probe::new(Mode::ReplaceBeforeBatch("/a"));
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal = Arc::new(FakeJournalStorage::default());
    let cell = cell_over(fs.clone(), Arc::clone(&budget), Arc::clone(&journal));
    fs.inner.write("/a", b"old").unwrap();
    assert!(
        cell.apply_vfs_mutations(
            "apply_patch",
            &[write(
                "/a",
                b"mine",
                VfsWritePrecondition::Matches(b"old".to_vec())
            )],
        )
        .is_err()
    );
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0, "released");
    let failures = error_results(&journal);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("conflict"), "{failures:?}");
}

#[test]
fn a_successful_batch_keeps_its_reservation_and_journals_no_failure() {
    // Positive control for the release assertions: the counter does move.
    let fs = Probe::new(Mode::Plain);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal = Arc::new(FakeJournalStorage::default());
    let cell = cell_over(fs.clone(), Arc::clone(&budget), Arc::clone(&journal));
    cell.apply_vfs_mutations(
        "apply_patch",
        &[write("/a", b"hello", VfsWritePrecondition::Any)],
    )
    .unwrap();
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 5);
    assert!(error_results(&journal).is_empty());
}

#[test]
fn any_other_batch_error_changes_nothing_and_is_not_rolled_back() {
    let fs = Probe::new(Mode::BatchFails);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal = Arc::new(FakeJournalStorage::default());
    let cell = cell_over(fs.clone(), Arc::clone(&budget), Arc::clone(&journal));
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[write(
                "/a",
                b"new",
                VfsWritePrecondition::Matches(b"old".to_vec()),
            )],
        )
        .unwrap_err();
    assert!(matches!(err, SandboxError::Vfs(VfsError::Io(_))), "{err}");
    assert_eq!(fs.batches().len(), 1, "one commit, no fallback execution");
    assert_eq!(
        fs.per_item_calls(),
        0,
        "no per-item execution, no rollback writes"
    );
    assert_eq!(text(&*fs, "/a"), "old");
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0, "released");
    assert_eq!(error_results(&journal).len(), 1);
}

#[test]
fn a_filesystem_without_batches_still_executes_item_by_item() {
    let fs = Arc::new(OverlayFs::new(
        Box::new(MemoryFs::new()),
        Box::new(MemoryFs::new()),
    ));
    let cell = plain_cell(fs.clone());
    fs.write("/a", b"old").unwrap();
    cell.apply_vfs_mutations(
        "apply_patch",
        &[
            write("/a", b"new", VfsWritePrecondition::Matches(b"old".to_vec())),
            write("/b", b"b", VfsWritePrecondition::Missing),
        ],
    )
    .unwrap();
    assert_eq!(text(&*fs, "/a"), "new");
    assert_eq!(text(&*fs, "/b"), "b");
}

#[test]
fn a_mid_batch_failure_without_batch_support_is_rolled_back() {
    // Write 1 (/a) lands, write 2 (/b) fails, then the rollback restores /a.
    let fs = Probe::new(Mode::NoBatchFailingWrite(2));
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal = Arc::new(FakeJournalStorage::default());
    let cell = cell_over(fs.clone(), Arc::clone(&budget), Arc::clone(&journal));
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[
                write("/a", b"new", VfsWritePrecondition::Matches(b"old".to_vec())),
                write("/b", b"b", VfsWritePrecondition::Missing),
            ],
        )
        .unwrap_err();
    assert!(matches!(err, SandboxError::Vfs(VfsError::Io(_))), "{err}");
    assert_eq!(text(&*fs, "/a"), "old", "rolled back");
    assert!(!fs.exists("/b"));
    assert!(fs.writes.load(SeqCst) >= 3, "item writes, then the restore");
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0, "released");
    assert_eq!(error_results(&journal).len(), 1);
}
