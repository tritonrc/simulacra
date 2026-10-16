//! A JavaScript evaluation publishes its cutoff to host calls made on its
//! thread. The engine cannot interrupt a host call in flight, so a slow host
//! filesystem reads the cutoff and stops starting work at the same instant the
//! engine would have interrupted the script.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simulacra_quickjs::{FsProxy, JsRuntime};
use simulacra_types::VirtualFs;
use simulacra_types::evaluation_deadline;
use simulacra_vfs::MemoryFs;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Serves "/x" = "1" and records the cutoff in force at each read. Everything
/// else is unused: a script that reaches for it fails loudly.
#[derive(Default)]
struct RecordingFs {
    cutoffs: Mutex<Vec<Option<Instant>>>,
}

impl FsProxy for RecordingFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.cutoffs
            .lock()
            .unwrap()
            .push(evaluation_deadline::current());
        match path {
            "/x" => Ok(b"1".to_vec()),
            other => Err(format!("not found: {other}")),
        }
    }
    fn write_file(&self, _path: &str, _data: &[u8]) -> Result<(), String> {
        Err("unused".into())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, String> {
        Err("unused".into())
    }
    fn stat(&self, _path: &str) -> Result<(bool, bool, u64), String> {
        Err("unused".into())
    }
    fn remove(&self, _path: &str) -> Result<(), String> {
        Err("unused".into())
    }
    fn rename(&self, _from: &str, _to: &str) -> Result<(), String> {
        Err("unused".into())
    }
    fn exists(&self, _path: &str) -> Result<bool, String> {
        Err("unused".into())
    }
    fn mkdir(&self, _path: &str) -> Result<(), String> {
        Err("unused".into())
    }
}

fn runtime_over(proxy: &Arc<RecordingFs>) -> JsRuntime {
    // The proxy is how a script reaches the host; the vfs argument is unused.
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let fs_proxy: Arc<dyn FsProxy> = proxy.clone();
    JsRuntime::with_options(vfs, TIMEOUT, None, Some(fs_proxy)).expect("runtime should be created")
}

fn recorded(proxy: &RecordingFs) -> Vec<Option<Instant>> {
    proxy.cutoffs.lock().unwrap().clone()
}

/// The one cutoff recorded, which must lie between `start + TIMEOUT` and
/// `end + TIMEOUT`: the evaluation computes it after it starts and before it ends.
fn assert_cutoff_within(cutoff: Option<Instant>, start: Instant, end: Instant) -> Instant {
    let cutoff = cutoff.expect("a host read during an evaluation must see a cutoff");
    assert!(
        cutoff >= start + TIMEOUT,
        "cutoff earlier than start + timeout"
    );
    assert!(cutoff <= end + TIMEOUT, "cutoff later than end + timeout");
    cutoff
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime should build")
}

#[test]
fn a_host_read_during_an_evaluation_sees_the_evaluations_cutoff() {
    let proxy = Arc::new(RecordingFs::default());
    let runtime = runtime_over(&proxy);

    let before = Instant::now();
    let output = runtime
        .eval("fs.readFileSync('/x')")
        .expect("the read should succeed");
    let after = Instant::now();

    assert_eq!(output.result.as_deref(), Some("1"));
    let seen = recorded(&proxy);
    assert_eq!(seen.len(), 1);
    assert_cutoff_within(seen[0], before, after);
}

#[test]
fn outside_an_evaluation_there_is_no_cutoff() {
    let proxy = Arc::new(RecordingFs::default());
    let runtime = runtime_over(&proxy);

    assert_eq!(evaluation_deadline::current(), None);
    runtime.eval("fs.readFileSync('/x')").unwrap();
    assert_eq!(evaluation_deadline::current(), None);

    // The same holds on the thread that polls the evaluation itself.
    current_thread_runtime()
        .block_on(runtime.eval_async("fs.readFileSync('/x')"))
        .unwrap();
    assert_eq!(evaluation_deadline::current(), None);
}

#[test]
fn each_evaluation_publishes_its_own_cutoff_even_when_the_script_throws() {
    let proxy = Arc::new(RecordingFs::default());
    let runtime = runtime_over(&proxy);
    // One thread runs both evaluations, so a cutoff left behind by the first
    // would be visible to the second.
    let tokio = current_thread_runtime();

    let start_1 = Instant::now();
    let failed =
        tokio.block_on(runtime.eval_async("fs.readFileSync('/x'); throw new Error('boom')"));
    let end_1 = Instant::now();
    assert!(failed.is_err(), "the script throws");
    assert_eq!(evaluation_deadline::current(), None);

    let start_2 = Instant::now();
    tokio
        .block_on(runtime.eval_async("fs.readFileSync('/x')"))
        .unwrap();
    let end_2 = Instant::now();
    assert_eq!(evaluation_deadline::current(), None);

    let seen = recorded(&proxy);
    assert_eq!(seen.len(), 2);
    let first = assert_cutoff_within(seen[0], start_1, end_1);
    let second = assert_cutoff_within(seen[1], start_2, end_2);
    assert!(
        second > first,
        "the second evaluation has its own, later cutoff"
    );
}

#[test]
fn a_spawned_evaluation_publishes_its_cutoff_too() {
    let proxy = Arc::new(RecordingFs::default());
    let runtime = runtime_over(&proxy);

    let before = Instant::now();
    let output = current_thread_runtime()
        .block_on(runtime.eval_async_spawned("fs.readFileSync('/x')"))
        .expect("the read should succeed");
    let after = Instant::now();

    assert_eq!(output.result.as_deref(), Some("1"));
    let seen = recorded(&proxy);
    assert_eq!(seen.len(), 1);
    assert_cutoff_within(seen[0], before, after);
}
