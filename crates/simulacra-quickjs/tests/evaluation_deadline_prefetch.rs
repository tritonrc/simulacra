//! Module prefetch runs before the engine starts, on another thread. Its reads
//! belong to the same evaluation as the script's own, so they see the same
//! cutoff and stop at it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simulacra_quickjs::{FsProxy, JsError, JsRuntime};
use simulacra_types::VirtualFs;
use simulacra_types::evaluation_deadline;
use simulacra_vfs::MemoryFs;

struct Read {
    path: String,
    started: Instant,
    cutoff: Option<Instant>,
}

/// Serves a tiny ES module at /lib.js, /a.js and /b.js and a plain "1" at /x,
/// recording when each read started and the cutoff in force. A read of /a.js
/// takes `slow_a`, to run the clock past a short deadline.
struct RecordingFs {
    reads: Mutex<Vec<Read>>,
    slow_a: Duration,
}

impl RecordingFs {
    fn new(slow_a: Duration) -> Arc<Self> {
        Arc::new(Self {
            reads: Mutex::new(Vec::new()),
            slow_a,
        })
    }

    fn reads_of(&self, path: &str) -> Vec<(Instant, Option<Instant>)> {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read.path == path)
            .map(|read| (read.started, read.cutoff))
            .collect()
    }
}

impl FsProxy for RecordingFs {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.reads.lock().unwrap().push(Read {
            path: path.to_string(),
            started: Instant::now(),
            cutoff: evaluation_deadline::current(),
        });
        match path {
            "/x" => Ok(b"1".to_vec()),
            "/lib.js" | "/b.js" => Ok(b"export default 2;".to_vec()),
            "/a.js" => {
                std::thread::sleep(self.slow_a);
                Ok(b"export default 1;".to_vec())
            }
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

fn runtime_over(proxy: &Arc<RecordingFs>, timeout: Duration) -> JsRuntime {
    // The proxy is how a script reaches the host; the vfs argument is unused.
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let fs_proxy: Arc<dyn FsProxy> = proxy.clone();
    JsRuntime::with_options(vfs, timeout, None, Some(fs_proxy)).expect("runtime should be created")
}

#[test]
fn an_import_read_during_prefetch_sees_the_evaluations_cutoff() {
    let proxy = RecordingFs::new(Duration::ZERO);
    let runtime = runtime_over(&proxy, Duration::from_secs(5));

    let before = Instant::now();
    runtime
        .eval(
            r#"
            import lib from "/lib.js";
            fs.readFileSync('/x');
            "#,
        )
        .expect("the module and the read should succeed");
    let after = Instant::now();

    let lib_reads = proxy.reads_of("/lib.js");
    let direct_reads = proxy.reads_of("/x");
    assert!(
        !lib_reads.is_empty(),
        "the import must be read from the host"
    );
    assert_eq!(direct_reads.len(), 1);
    let engine_cutoff = direct_reads[0]
        .1
        .expect("a direct read during the evaluation sees a cutoff");
    assert!(engine_cutoff >= before + Duration::from_secs(5));
    assert!(engine_cutoff <= after + Duration::from_secs(5));
    // Prefetch runs on its own thread before the engine starts; every one of
    // its reads must carry the engine's very cutoff, not a second one.
    for (index, (_, cutoff)) in lib_reads.iter().enumerate() {
        assert_eq!(
            *cutoff,
            Some(engine_cutoff),
            "read {index} of /lib.js saw a different or missing cutoff"
        );
    }
}

#[test]
fn prefetch_starts_no_read_after_the_cutoff() {
    const TIMEOUT: Duration = Duration::from_millis(300);
    // Prefetch reads the root's imports one after another: /a.js takes 500 ms,
    // so /b.js would start 200 ms after the cutoff.
    let proxy = RecordingFs::new(Duration::from_millis(500));
    let runtime = runtime_over(&proxy, TIMEOUT);

    let before = Instant::now();
    let result = runtime.eval(
        r#"
        import a from "/a.js";
        import b from "/b.js";
        "#,
    );

    assert!(
        matches!(result, Err(JsError::Timeout)),
        "the evaluation should end at its deadline, got {result:?}"
    );
    // The abandoned prefetch worker is not joined, so give it time to reach
    // the second module if it is going to.
    std::thread::sleep(Duration::from_millis(900));

    // The evaluation's cutoff is just after `before + TIMEOUT`; the margin
    // absorbs the thread hand-offs between `before` and the cutoff.
    let cutoff_bound = before + TIMEOUT + Duration::from_millis(100);
    let late: Vec<_> = proxy
        .reads
        .lock()
        .unwrap()
        .iter()
        .filter(|read| read.started > cutoff_bound)
        .map(|read| (read.path.clone(), read.started - before))
        .collect();
    assert!(
        late.is_empty(),
        "reads started after the cutoff (path, time since start): {late:?}"
    );
    assert_eq!(
        proxy.reads_of("/a.js").len(),
        1,
        "/a.js starts before the cutoff"
    );
}
