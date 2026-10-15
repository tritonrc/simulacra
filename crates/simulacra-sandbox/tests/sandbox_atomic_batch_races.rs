//! Races and unknown outcomes around `AgentCell::apply_vfs_mutations` over an
//! atomic-batch filesystem: the revision a precondition pins must be read
//! before the bytes it validates, a revision that cannot be read is an error
//! rather than an unconditional commit, and an `OutcomeUnknown` batch keeps its
//! reservation.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_sandbox::{SandboxError, VfsMutation, VfsWritePrecondition};
use simulacra_types::FsMutation;
use std::sync::atomic::Ordering::SeqCst;

enum Mode {
    /// The first `read` of this path returns its current bytes, then a
    /// competing writer replaces them with `theirs` before the read returns, so
    /// the revision changes after the bytes were read.
    ReadThenReplace(&'static str, &'static str),
    /// The 2nd `metadata` call for this path fails with `Io`; every other call
    /// succeeds. Assumption: in a batch over one `Matches` write, call 1 is the
    /// byte validation and call 2 is the revision pin (or, if the fix reads the
    /// revision first, the validation); either way an error must surface.
    SecondMetadataFails(&'static str),
    /// `apply_batch` reports `OutcomeUnknown` without applying anything.
    BatchOutcomeUnknown,
}

/// A `MemoryFs` recording batches and counting per-item primitives.
struct Probe {
    inner: Arc<MemoryFs>,
    mode: Mode,
    writes: AtomicUsize,
    removes: AtomicUsize,
    renames: AtomicUsize,
    reads_of_target: AtomicUsize,
    metadata_of_target: AtomicUsize,
    batches: Mutex<Vec<Vec<FsMutation>>>,
}

impl Probe {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryFs::new()),
            mode,
            writes: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            renames: AtomicUsize::new(0),
            reads_of_target: AtomicUsize::new(0),
            metadata_of_target: AtomicUsize::new(0),
            batches: Mutex::new(Vec::new()),
        })
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
        let bytes = self.inner.read(path)?;
        if let Mode::ReadThenReplace(target, theirs) = self.mode
            && path == target
            && self.reads_of_target.fetch_add(1, SeqCst) == 0
        {
            self.inner.write(path, theirs.as_bytes()).unwrap();
        }
        Ok(bytes)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes.fetch_add(1, SeqCst);
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
        if let Mode::SecondMetadataFails(target) = self.mode
            && path == target
            && self.metadata_of_target.fetch_add(1, SeqCst) == 1
        {
            return Err(VfsError::Io(format!("injected metadata failure on {path}")));
        }
        self.inner.metadata(path)
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
        self.batches.lock().unwrap().push(mutations.to_vec());
        if matches!(self.mode, Mode::BatchOutcomeUnknown) {
            return Err(VfsError::OutcomeUnknown("connection lost".to_owned()));
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

fn write_matching_old(path: &str, data: &[u8]) -> VfsMutation {
    VfsMutation::Write {
        path: path.into(),
        data: data.to_vec(),
        precondition: VfsWritePrecondition::Matches(b"old".to_vec()),
    }
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

/// Every journaled tool result, error or not.
fn tool_result_texts(journal: &FakeJournalStorage) -> Vec<String> {
    journal
        .entries()
        .into_iter()
        .filter_map(|e| match e.entry {
            JournalEntryKind::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

#[test]
fn a_write_landing_between_the_byte_check_and_the_revision_read_conflicts() {
    let fs = Probe::new(Mode::ReadThenReplace("/a", "theirs"));
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations("apply_patch", &[write_matching_old("/a", b"mine")])
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::Vfs(VfsError::Conflict(_))),
        "the bytes validated are not the revision pinned: {err}"
    );
    assert_eq!(
        text(&*fs, "/a"),
        "theirs",
        "the other writer's bytes survive"
    );
    assert_eq!(fs.per_item_calls(), 0);
}

#[test]
fn a_move_whose_source_changes_between_the_byte_check_and_the_revision_read_conflicts() {
    let fs = Probe::new(Mode::ReadThenReplace("/a", "theirs"));
    let cell = plain_cell(fs.clone());
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations(
            "apply_patch",
            &[VfsMutation::MoveAndWrite {
                from: "/a".into(),
                to: "/b".into(),
                data: b"mine".to_vec(),
                from_precondition: Some(b"old".to_vec()),
            }],
        )
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::Vfs(VfsError::Conflict(_))),
        "{err}"
    );
    assert_eq!(text(&*fs, "/a"), "theirs", "the source is not removed");
    assert!(!fs.exists("/b"), "the move target is not written");
    assert_eq!(fs.per_item_calls(), 0);
}

#[test]
fn an_unreadable_revision_is_an_error_and_never_an_unconditional_commit() {
    let fs = Probe::new(Mode::SecondMetadataFails("/a"));
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cell = cell_over(
        fs.clone(),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );
    fs.inner.write("/a", b"old").unwrap();
    let result = cell.apply_vfs_mutations("apply_patch", &[write_matching_old("/a", b"mine")]);
    assert!(
        result.is_err(),
        "a metadata failure while pinning must surface"
    );
    assert!(
        fs.batches().is_empty(),
        "apply_batch must never run with a silently unconditional item: {:?}",
        fs.batches()
    );
    assert_eq!(text(&*fs, "/a"), "old");
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0, "released");
    assert!(
        fs.metadata_of_target.load(SeqCst) >= 2,
        "positive control: the injected failure point was reached"
    );
}

#[test]
fn an_unknown_batch_outcome_surfaces_unchanged_and_keeps_its_reservation() {
    let fs = Probe::new(Mode::BatchOutcomeUnknown);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let journal = Arc::new(FakeJournalStorage::default());
    let cell = cell_over(fs.clone(), Arc::clone(&budget), Arc::clone(&journal));
    fs.inner.write("/a", b"old").unwrap();
    let err = cell
        .apply_vfs_mutations("apply_patch", &[write_matching_old("/a", b"mine!")])
        .unwrap_err();
    assert!(
        matches!(&err, SandboxError::Vfs(VfsError::OutcomeUnknown(m)) if m == "connection lost"),
        "{err}"
    );
    assert_eq!(fs.batches().len(), 1, "one commit attempt");
    assert_eq!(
        fs.per_item_calls(),
        0,
        "no per-item execution and no rollback writes"
    );
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        5,
        "the batch may have landed, so its bytes stay reserved"
    );
    let results = tool_result_texts(&journal);
    assert!(
        results.iter().any(|t| t.contains("may have")),
        "the journal must say the batch may have been applied: {results:?}"
    );
}
