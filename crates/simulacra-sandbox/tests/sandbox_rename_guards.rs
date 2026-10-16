//! JS `renameSync` over an existing destination: the source is pinned to the
//! file that was inspected, and the copy fallback (a filesystem that answers
//! `Unsupported` to renames and batches) is charged against the VFS write
//! budget like any other write.

mod common;
#[allow(unused_imports)]
use common::*;

use simulacra_types::{FsMutation, Precondition};
use simulacra_vfs::OverlayFs;
use std::sync::atomic::Ordering::SeqCst;

/// A `MemoryFs` that, just before its first batch commits, replaces the file
/// `/a` with a directory holding a child: the source changes type after the
/// caller inspected it.
struct SwapSourceBeforeBatch {
    inner: Arc<MemoryFs>,
    swapped: AtomicBool,
}

impl VirtualFs for SwapSourceBeforeBatch {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
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
        self.inner.write_if(path, data, pre)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.inner.rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        if !self.swapped.swap(true, SeqCst) {
            self.inner.remove("/a").unwrap();
            self.inner.mkdir("/a").unwrap();
            self.inner.write("/a/child", b"child").unwrap();
        }
        self.inner.apply_batch(mutations)
    }
}

fn js_cell_over(fs: Arc<dyn VirtualFs>, budget: Arc<Mutex<ResourceBudget>>) -> AgentCell {
    let journal: Arc<dyn JournalStorage> = Arc::new(FakeJournalStorage::default());
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

fn text(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).unwrap()).unwrap()
}

#[test]
fn a_source_swapped_for_a_directory_before_the_commit_fails_and_keeps_the_destination() {
    let fs = Arc::new(SwapSourceBeforeBatch {
        inner: Arc::new(MemoryFs::new()),
        swapped: AtomicBool::new(false),
    });
    fs.inner.write("/a", b"source").unwrap();
    fs.inner.write("/b", b"keep-b").unwrap();
    let cell = js_cell_over(fs.clone(), unlimited_budget());

    let result = cell.execute_js("fs.renameSync('/a', '/b')");

    assert!(
        fs.swapped.load(SeqCst),
        "positive control: the batch was attempted and the source swapped"
    );
    assert!(
        result.is_err(),
        "a directory must not replace a file, whatever the race"
    );
    assert_eq!(text(&*fs, "/b"), "keep-b", "the destination file is intact");
    assert!(
        fs.metadata("/a").unwrap().is_dir,
        "the swapped-in directory was not moved"
    );
    assert_eq!(text(&*fs, "/a/child"), "child");
}

fn overlay_with(path: &str, data: &[u8]) -> Arc<OverlayFs> {
    let fs = Arc::new(OverlayFs::new(
        Box::new(MemoryFs::new()),
        Box::new(MemoryFs::new()),
    ));
    fs.write(path, data).unwrap();
    fs
}

#[test]
fn the_copy_fallback_is_refused_when_the_copied_bytes_exceed_the_budget() {
    let fs = overlay_with("/a", &[7u8; 100]);
    let budget = budget_with_overrides(0, 0, 10, 0);
    let cell = js_cell_over(fs.clone(), Arc::clone(&budget));

    let result = cell.execute_js("fs.renameSync('/a', '/b')");

    assert!(
        result.is_err(),
        "100 bytes copied against a 10-byte write budget must be refused"
    );
    assert!(!fs.exists("/b"), "nothing was copied");
    assert_eq!(fs.read("/a").unwrap().len(), 100, "the source is intact");
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 0);
}

#[test]
fn the_copy_fallback_charges_the_copied_bytes_when_the_budget_covers_them() {
    let fs = overlay_with("/a", &[7u8; 100]);
    let budget = budget_with_overrides(0, 0, 1000, 0);
    let cell = js_cell_over(fs.clone(), Arc::clone(&budget));

    cell.execute_js("fs.renameSync('/a', '/b')").unwrap();

    assert_eq!(fs.read("/b").unwrap().len(), 100, "positive control");
    assert!(!fs.exists("/a"));
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        100,
        "the copy wrote 100 bytes through the backend"
    );
}

#[test]
fn the_copy_fallback_over_an_existing_destination_is_charged_too() {
    let fs = overlay_with("/a", &[7u8; 100]);
    fs.write("/b", b"old").unwrap();
    let budget = budget_with_overrides(0, 0, 10, 0);
    let cell = js_cell_over(fs.clone(), Arc::clone(&budget));

    let result = cell.execute_js("fs.renameSync('/a', '/b')");

    assert!(result.is_err(), "refused for budget");
    assert_eq!(text(&*fs, "/b"), "old", "the destination is intact");
    assert_eq!(fs.read("/a").unwrap().len(), 100);
}
