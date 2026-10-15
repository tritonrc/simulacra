//! JS `fs` host functions over filesystems with atomic operations: rename,
//! conditional append, and truncated readdir.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_types::{DirListing, FsMutation, Precondition};
use simulacra_vfs::OverlayFs;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

/// How a [`ProbeFs`] misbehaves.
enum Mode {
    /// Forwards everything, only counting.
    Plain,
    /// The first `write` or `write_if` is preceded by a competing writer
    /// appending `other;` to the target, as if it landed after the read.
    RaceOnce,
    /// Every `write_if` answers `Conflict`.
    AlwaysConflict,
    /// `list_dir_bounded` cuts the listing at this many entries.
    TruncateAt(usize),
}

/// A `MemoryFs` that counts the emulation primitives and the atomic methods,
/// so a test can tell which one the code under test used.
struct ProbeFs {
    inner: Arc<MemoryFs>,
    mode: Mode,
    writes: AtomicUsize,
    renames: AtomicUsize,
    raced: AtomicBool,
    preconditions: Mutex<Vec<Precondition>>,
}

impl ProbeFs {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryFs::new()),
            mode,
            writes: AtomicUsize::new(0),
            renames: AtomicUsize::new(0),
            raced: AtomicBool::new(false),
            preconditions: Mutex::new(Vec::new()),
        })
    }

    fn write_if_calls(&self) -> usize {
        self.preconditions.lock().unwrap().len()
    }

    fn race(&self, path: &str) {
        if matches!(self.mode, Mode::RaceOnce) && !self.raced.swap(true, SeqCst) {
            let mut text = self.inner.read(path).unwrap_or_default();
            text.extend_from_slice(b"other;");
            self.inner.write(path, &text).unwrap();
        }
    }
}

impl VirtualFs for ProbeFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes.fetch_add(1, SeqCst);
        self.race(path);
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
        self.inner.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
    fn write_if(&self, path: &str, data: &[u8], pre: Precondition) -> Result<(), VfsError> {
        self.preconditions.lock().unwrap().push(pre);
        if matches!(self.mode, Mode::AlwaysConflict) {
            return Err(VfsError::Conflict(path.to_string()));
        }
        self.race(path);
        self.inner.write_if(path, data, pre)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.renames.fetch_add(1, SeqCst);
        self.inner.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.inner.apply_batch(mutations)
    }
    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        let mut entries = self.inner.list_dir(path)?;
        let Mode::TruncateAt(limit) = self.mode else {
            return Ok(DirListing::complete(entries));
        };
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        Ok(DirListing { entries, truncated })
    }
}

fn overlay() -> Arc<OverlayFs> {
    Arc::new(OverlayFs::new(
        Box::new(MemoryFs::new()),
        Box::new(MemoryFs::new()),
    ))
}

fn cell_with(
    fs: Arc<dyn VirtualFs>,
    capability: CapabilityToken,
    budget: Arc<Mutex<ResourceBudget>>,
) -> AgentCell {
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
    let http: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    AgentCell::new(fs, capability, budget, journal, http)
}

/// A cell over `fs` that may read, write and run JavaScript anywhere.
fn js_cell_over(fs: Arc<dyn VirtualFs>) -> AgentCell {
    cell_with(
        fs,
        capability(&["/**"], &["/**"], false, true),
        unlimited_budget(),
    )
}

fn result_of(cell: &AgentCell, code: &str) -> String {
    let out = cell
        .execute_js(code)
        .unwrap_or_else(|e| panic!("{code}: {e}"));
    out.result.unwrap_or_default()
}

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

// ── rename ───────────────────────────────────────────────────────────

#[test]
fn js_rename_uses_the_atomic_rename() {
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    fs.inner.write("/a.txt", b"1").unwrap();
    cell.execute_js("fs.renameSync('/a.txt', '/b.txt')")
        .unwrap();
    assert_eq!(fs.renames.load(SeqCst), 1);
    assert_eq!(fs.writes.load(SeqCst), 0, "no copy emulation");
    assert_eq!(text(&*fs, "/b.txt"), "1");
    assert!(!fs.exists("/a.txt"));
}

#[test]
fn js_rename_moves_a_directory_with_one_atomic_rename() {
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    fs.inner.write("/d/x.txt", b"x").unwrap();
    cell.execute_js("fs.renameSync('/d', '/e')").unwrap();
    assert_eq!(fs.renames.load(SeqCst), 1);
    assert_eq!(fs.writes.load(SeqCst), 0, "no per-file copy");
    assert_eq!(text(&*fs, "/e/x.txt"), "x");
    assert!(!fs.exists("/d"));
}

#[test]
fn js_rename_onto_an_existing_file_throws_and_changes_nothing() {
    // The atomic rename's Conflict is the answer: it must not be "fixed" by
    // falling back to the overwriting copy.
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    fs.inner.write("/a.txt", b"mine").unwrap();
    fs.inner.write("/b.txt", b"keep").unwrap();
    assert!(
        cell.execute_js("fs.renameSync('/a.txt', '/b.txt')")
            .is_err()
    );
    assert_eq!(text(&*fs, "/b.txt"), "keep");
    assert_eq!(text(&*fs, "/a.txt"), "mine");
    assert_eq!(fs.writes.load(SeqCst), 0);
}

#[test]
fn js_rename_to_a_path_without_write_capability_is_refused_before_the_rename() {
    let fs = ProbeFs::new(Mode::Plain);
    let cap = capability(&["/**"], &["/ok/**"], false, true);
    let cell = cell_with(fs.clone(), cap, unlimited_budget());
    fs.inner.write("/ok/a.txt", b"1").unwrap();
    assert!(
        cell.execute_js("fs.renameSync('/ok/a.txt', '/denied/b.txt')")
            .is_err()
    );
    assert_eq!(fs.renames.load(SeqCst), 0);
    assert!(fs.exists("/ok/a.txt") && !fs.exists("/denied/b.txt"));
}

#[test]
fn js_rename_falls_back_to_copy_when_the_fs_cannot_rename() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    fs.write("/a.txt", b"1").unwrap();
    cell.execute_js("fs.renameSync('/a.txt', '/b.txt')")
        .unwrap();
    assert_eq!(text(&*fs, "/b.txt"), "1");
    assert!(!fs.exists("/a.txt"));
}

#[test]
fn js_rename_fallback_still_moves_a_directory_tree() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    fs.write("/d/x.txt", b"x").unwrap();
    cell.execute_js("fs.renameSync('/d', '/e')").unwrap();
    assert_eq!(text(&*fs, "/e/x.txt"), "x");
    assert!(!fs.exists("/d/x.txt"));
}

// ── append ───────────────────────────────────────────────────────────

#[test]
fn js_append_to_a_missing_file_creates_it_with_must_be_absent() {
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    cell.execute_js("fs.appendFileSync('/log.txt', 'a')")
        .unwrap();
    assert_eq!(text(&*fs, "/log.txt"), "a");
    assert_eq!(
        *fs.preconditions.lock().unwrap(),
        vec![Precondition::MustBeAbsent]
    );
}

#[test]
fn js_append_to_an_existing_file_is_conditional_on_the_revision_it_read() {
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    fs.inner.write("/log.txt", b"a;").unwrap();
    let revision = fs
        .inner
        .metadata("/log.txt")
        .unwrap()
        .revision
        .expect("MemoryFs revisions");
    cell.execute_js("fs.appendFileSync('/log.txt', 'b;')")
        .unwrap();
    assert_eq!(text(&*fs, "/log.txt"), "a;b;");
    assert_eq!(
        *fs.preconditions.lock().unwrap(),
        vec![Precondition::MustMatch(revision)]
    );
    assert_eq!(fs.writes.load(SeqCst), 0, "no unconditional write");
}

#[test]
fn two_racing_appends_both_land() {
    let fs = ProbeFs::new(Mode::RaceOnce);
    let cell = js_cell_over(fs.clone());
    fs.inner.write("/log.txt", b"start;").unwrap();
    cell.execute_js("fs.appendFileSync('/log.txt', 'mine;')")
        .unwrap();
    assert_eq!(text(&*fs, "/log.txt"), "start;other;mine;");
    assert_eq!(fs.write_if_calls(), 2, "one conflict, one retry");
}

#[test]
fn an_append_racing_the_creation_of_the_file_keeps_the_other_writers_bytes() {
    let fs = ProbeFs::new(Mode::RaceOnce);
    let cell = js_cell_over(fs.clone());
    cell.execute_js("fs.appendFileSync('/log.txt', 'mine;')")
        .unwrap();
    assert_eq!(text(&*fs, "/log.txt"), "other;mine;");
}

#[test]
fn js_append_whose_retry_conflicts_again_throws_and_releases_the_write_budget() {
    let fs = ProbeFs::new(Mode::AlwaysConflict);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cap = capability(&["/**"], &["/**"], false, true);
    let cell = cell_with(fs.clone(), cap, Arc::clone(&budget));
    fs.inner.write("/log.txt", b"start;").unwrap();
    assert!(
        cell.execute_js("fs.appendFileSync('/log.txt', 'mine;')")
            .is_err()
    );
    assert_eq!(fs.write_if_calls(), 2, "exactly one retry");
    assert_eq!(text(&*fs, "/log.txt"), "start;");
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        0,
        "reservation released"
    );
}

#[test]
fn a_successful_append_keeps_its_budget_reservation() {
    // Positive control for the release assertions: the counter does move.
    let fs = ProbeFs::new(Mode::Plain);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cap = capability(&["/**"], &["/**"], false, true);
    let cell = cell_with(fs, cap, Arc::clone(&budget));
    cell.execute_js("fs.appendFileSync('/log.txt', 'hello')")
        .unwrap();
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 5);
}

#[test]
fn js_append_to_a_directory_throws_and_releases_the_write_budget() {
    let fs = ProbeFs::new(Mode::Plain);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cap = capability(&["/**"], &["/**"], false, true);
    let cell = cell_with(fs.clone(), cap, Arc::clone(&budget));
    fs.inner.mkdir("/d").unwrap();
    assert!(cell.execute_js("fs.appendFileSync('/d', 'x')").is_err());
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0);
}

#[test]
fn js_append_falls_back_to_a_plain_write_when_write_if_is_unsupported() {
    let fs = overlay();
    let cell = js_cell_over(fs.clone());
    cell.execute_js("fs.appendFileSync('/log.txt', 'a;')")
        .unwrap();
    cell.execute_js("fs.appendFileSync('/log.txt', 'b;')")
        .unwrap();
    assert_eq!(text(&*fs, "/log.txt"), "a;b;");
}

// ── readdir ──────────────────────────────────────────────────────────

const LISTING: &str = "const e = fs.readdirSync('/d'); JSON.stringify([Array.isArray(e), e.length, e.truncated === true])";

#[test]
fn js_readdir_marks_a_truncated_listing() {
    let fs = ProbeFs::new(Mode::TruncateAt(2));
    let cell = js_cell_over(fs.clone());
    for name in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{name}"), b"").unwrap();
    }
    assert_eq!(result_of(&cell, LISTING), "[true,2,true]");
}

#[test]
fn js_readdir_does_not_mark_a_listing_that_fits_the_bound() {
    let fs = ProbeFs::new(Mode::TruncateAt(3));
    let cell = js_cell_over(fs.clone());
    for name in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{name}"), b"").unwrap();
    }
    assert_eq!(result_of(&cell, LISTING), "[true,3,false]");
}

#[test]
fn js_readdir_on_an_unbounded_fs_is_a_plain_array() {
    let fs = ProbeFs::new(Mode::Plain);
    let cell = js_cell_over(fs.clone());
    for name in ["a", "b", "c"] {
        fs.inner.write(&format!("/d/{name}"), b"").unwrap();
    }
    assert_eq!(result_of(&cell, LISTING), "[true,3,false]");
}

#[test]
fn js_readdir_of_a_missing_directory_throws() {
    let cell = js_cell_over(ProbeFs::new(Mode::TruncateAt(2)));
    assert!(cell.execute_js("fs.readdirSync('/nope')").is_err());
}

#[test]
fn js_readdir_without_read_capability_throws_before_listing() {
    let fs = ProbeFs::new(Mode::TruncateAt(2));
    fs.inner.write("/d/a", b"").unwrap();
    let cap = capability(&["/ok/**"], &["/**"], false, true);
    let cell = cell_with(fs, cap, unlimited_budget());
    assert!(cell.execute_js("fs.readdirSync('/d')").is_err());
}
