use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simulacra_quickjs::{FsProxy, JsError, JsHostApiProfile, JsOutput, JsRuntime, ModuleFetcher};
use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;

#[allow(dead_code)]
trait MissingEvalAsyncFallback {
    fn eval_async<'a>(
        &'a self,
        code: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<JsOutput, JsError>> + 'a>>;
}

impl MissingEvalAsyncFallback for JsRuntime {
    fn eval_async<'a>(
        &'a self,
        code: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<JsOutput, JsError>> + 'a>> {
        let _ = (self, code);
        Box::pin(async {
            panic!(
                "JsRuntime::eval_async is not implemented; this fallback should be shadowed by the inherent S053 API"
            )
        })
    }
}

struct VfsBackedFsProxy {
    vfs: Arc<MemoryFs>,
}

impl VfsBackedFsProxy {
    fn new(vfs: Arc<MemoryFs>) -> Self {
        Self { vfs }
    }
}

impl FsProxy for VfsBackedFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.vfs.read(path).map_err(|error| error.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        self.vfs
            .write(path, data)
            .map_err(|error| error.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.vfs.list_dir(path).map_err(|error| error.to_string())
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let metadata = self.vfs.metadata(path).map_err(|error| error.to_string())?;
        Ok((metadata.is_file, metadata.is_dir, metadata.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.vfs.remove(path).map_err(|error| error.to_string())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let data = self.vfs.read(from).map_err(|error| error.to_string())?;
        self.vfs
            .write(to, &data)
            .map_err(|error| error.to_string())?;
        self.vfs.remove(from).map_err(|error| error.to_string())
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.vfs.mkdir(path).map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct RecordingFetcher {
    responses: Arc<HashMap<String, Result<String, String>>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingFetcher {
    fn new(responses: Vec<(&str, Result<&str, &str>)>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fetcher = Self {
            responses: Arc::new(
                responses
                    .into_iter()
                    .map(|(url, response)| {
                        (
                            url.to_string(),
                            response.map(str::to_string).map_err(str::to_string),
                        )
                    })
                    .collect(),
            ),
            calls: Arc::clone(&calls),
        };
        (fetcher, calls)
    }
}

impl ModuleFetcher for RecordingFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("no test module fixture for {url}")))
    }
}

struct PrefetchOrderFetcher {
    vfs: Arc<MemoryFs>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl ModuleFetcher for PrefetchOrderFetcher {
    fn fetch(&self, url: &str) -> Result<String, String> {
        self.calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .push(url.to_string());
        match url {
            "https://modules.invalid/entry.js" => Ok(r#"
                import marker from "https://modules.invalid/marker.js";
                fs.writeFileSync("/workspace/entry-evaluated.txt", "yes");
                export default marker;
                "#
            .to_string()),
            "https://modules.invalid/marker.js" => {
                if self.vfs.exists("/workspace/entry-evaluated.txt") {
                    return Err(
                        "transitive remote import was fetched after parent module evaluation"
                            .to_string(),
                    );
                }
                Ok(r#"export default "marker-ready";"#.to_string())
            }
            other => Err(format!("unexpected module fetch: {other}")),
        }
    }
}

struct SlowFetcher {
    delay: Duration,
}

impl ModuleFetcher for SlowFetcher {
    fn fetch(&self, _url: &str) -> Result<String, String> {
        std::thread::sleep(self.delay);
        Ok(r#"export default "late";"#.to_string())
    }
}

struct SlowThenFastFetcher {
    calls: Arc<Mutex<usize>>,
}

impl ModuleFetcher for SlowThenFastFetcher {
    fn fetch(&self, _url: &str) -> Result<String, String> {
        let call_index = {
            let mut calls = self
                .calls
                .lock()
                .expect("calls lock should not be poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        if call_index == 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        Ok(format!(r#"export default "call-{call_index}";"#))
    }
}

fn runtime(timeout: Duration) -> JsRuntime {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    JsRuntime::with_timeout(vfs, timeout).expect("runtime should be created")
}

fn execution_message(error: JsError) -> String {
    match error {
        JsError::Execution(message) => message,
        other => panic!("expected JsError::Execution, got {other:?}"),
    }
}

fn assert_timeout_message(message: &str) {
    let lower = message.to_lowercase();
    assert!(
        lower.contains("timeout") || lower.contains("timed out") || lower.contains("interrupt"),
        "expected timeout or interrupt error, got {message:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eval_async_awaits_async_script_result() {
    let runtime = runtime(Duration::from_secs(5));

    let output = runtime
        .eval_async(
            r#"
            (async () => {
              const value = await Promise.resolve("async-result");
              return `${value}:awaited`;
            })()
            "#,
        )
        .await
        .expect("eval_async should resolve promise-returning scripts");

    assert_eq!(output.result.as_deref(), Some("async-result:awaited"));
}

#[test]
fn eval_sync_uses_the_same_promise_resolution_semantics_as_eval_async() {
    let runtime = runtime(Duration::from_secs(5));

    let output = runtime
        .eval(
            r#"
            (async () => {
              const value = await Promise.resolve(21);
              return value * 2;
            })()
            "#,
        )
        .expect("sync eval should keep resolving promise-returning scripts");

    assert_eq!(output.result.as_deref(), Some("42"));
}

#[test]
fn rejected_promises_surface_as_execution_errors_with_the_rejection_message() {
    let runtime = runtime(Duration::from_secs(5));

    let message = execution_message(
        runtime
            .eval(
                r#"
                (async () => {
                  await Promise.resolve();
                  throw new Error("s053 rejected promise");
                })()
                "#,
            )
            .expect_err("rejected promise should fail evaluation"),
    );

    assert!(
        message.contains("s053 rejected promise"),
        "expected rejection reason in execution error, got {message:?}"
    );
}

#[test]
fn promise_returning_module_results_are_awaited_before_returning_output() {
    let runtime = runtime(Duration::from_secs(5));

    let output = runtime
        .eval(
            r#"
            export {};
            Promise.resolve("module-promise-result");
            "#,
        )
        .expect("promise-returning module result should resolve");

    assert_eq!(output.result.as_deref(), Some("module-promise-result"));
}

#[test]
fn rejected_module_result_promises_surface_as_execution_errors() {
    let runtime = runtime(Duration::from_secs(5));

    let message = execution_message(
        runtime
            .eval(
                r#"
                export {};
                Promise.reject(new Error("s053 rejected module result"));
                "#,
            )
            .expect_err("rejected module result should fail evaluation"),
    );

    assert!(
        message.contains("s053 rejected module result"),
        "expected module rejection reason in execution error, got {message:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eval_async_times_out_unresolved_promises() {
    let runtime = runtime(Duration::from_millis(50));

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.eval_async("new Promise(() => {})"),
    )
    .await
    .expect("eval_async should return instead of leaving the Rust future pending");
    let message = execution_message(result.expect_err("unresolved promise should time out"));

    assert_timeout_message(&message);
}

#[tokio::test(flavor = "current_thread")]
async fn eval_async_timeout_bounds_synchronous_remote_module_prefetch() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs,
        Duration::from_millis(50),
        Box::new(SlowFetcher {
            delay: Duration::from_millis(250),
        }),
    )
    .expect("runtime should be created");

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        runtime.eval_async(
            r#"
            import value from "https://modules.invalid/slow.js";
            value;
            "#,
        ),
    )
    .await
    .expect("eval_async should return before the outer test timeout");
    let message = execution_message(result.expect_err("slow prefetch should time out"));

    assert_timeout_message(&message);
}

#[test]
fn eval_sync_timeout_bounds_synchronous_remote_module_prefetch() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs,
        Duration::from_millis(50),
        Box::new(SlowFetcher {
            delay: Duration::from_millis(750),
        }),
    )
    .expect("runtime should be created");

    let started = Instant::now();
    let message = execution_message(
        runtime
            .eval(
                r#"
                import value from "https://modules.invalid/slow-sync.js";
                value;
                "#,
            )
            .expect_err("slow prefetch should time out"),
    );

    assert_timeout_message(&message);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "sync eval should not wait for the slow blocking prefetch task to finish"
    );
}

#[test]
fn timed_out_prefetch_does_not_populate_remote_source_cache_later() {
    let calls = Arc::new(Mutex::new(0));
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs,
        Duration::from_millis(50),
        Box::new(SlowThenFastFetcher {
            calls: Arc::clone(&calls),
        }),
    )
    .expect("runtime should be created");
    let code = r#"
        import value from "https://modules.invalid/racy-cache.js";
        value;
    "#;

    let first = runtime
        .eval(code)
        .expect_err("first slow prefetch should time out");
    assert_timeout_message(&execution_message(first));
    std::thread::sleep(Duration::from_millis(250));

    let second = runtime
        .eval(code)
        .expect("second prefetch should fetch again instead of using late cache state");

    assert_eq!(second.result.as_deref(), Some("call-1"));
    assert_eq!(
        *calls.lock().expect("calls lock should not be poisoned"),
        2,
        "timed-out prefetch must not populate the shared remote source cache after the caller returned"
    );
}

#[test]
fn runtime_timeout_interrupts_cpu_bound_loops() {
    let runtime = runtime(Duration::from_millis(50));
    let started = Instant::now();

    let message = execution_message(
        runtime
            .eval("while (true) {}")
            .expect_err("CPU-bound loop should be interrupted by the runtime timeout"),
    );

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout should interrupt promptly"
    );
    assert_timeout_message(&message);
}

#[test]
fn remote_module_cache_persists_while_module_instances_stay_fresh_per_eval() {
    let (fetcher, calls) = RecordingFetcher::new(vec![(
        "https://modules.invalid/cached.js",
        Ok(r#"
            globalThis.__s053ModuleLoads = (globalThis.__s053ModuleLoads ?? 0) + 1;
            export default globalThis.__s053ModuleLoads;
            "#),
    )]);
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime =
        JsRuntime::with_timeout_and_fetcher(vfs, Duration::from_secs(5), Box::new(fetcher))
            .expect("runtime should be created");

    let first = runtime
        .eval(
            r#"
            import loadCount from "https://modules.invalid/cached.js";
            loadCount;
            "#,
        )
        .expect("first import should succeed");
    let second = runtime
        .eval(
            r#"
            import loadCount from "https://modules.invalid/cached.js";
            loadCount;
            "#,
        )
        .expect("second import should use cached source but a fresh module instance");

    assert_eq!(first.result.as_deref(), Some("1"));
    assert_eq!(second.result.as_deref(), Some("1"));
    assert_eq!(
        calls
            .lock()
            .expect("calls lock should not be poisoned")
            .as_slice(),
        ["https://modules.invalid/cached.js"],
        "host module source cache should avoid a second fetch while eval contexts remain fresh"
    );
}

#[test]
fn remote_static_imports_prefetch_transitive_sources_before_module_evaluation() {
    let vfs = Arc::new(MemoryFs::new());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fetcher = PrefetchOrderFetcher {
        vfs: Arc::clone(&vfs),
        calls: Arc::clone(&calls),
    };
    let fs_proxy: Arc<dyn FsProxy> = Arc::new(VfsBackedFsProxy::new(Arc::clone(&vfs)));
    let runtime = JsRuntime::with_options(
        vfs.clone() as Arc<dyn VirtualFs>,
        Duration::from_secs(5),
        Some(Box::new(fetcher)),
        Some(fs_proxy),
    )
    .expect("runtime should be created");

    let output = runtime
        .eval(
            r#"
            import marker from "https://modules.invalid/entry.js";
            marker;
            "#,
        )
        .expect("static remote imports should be prefetched and served by the loader");

    assert_eq!(output.result.as_deref(), Some("marker-ready"));
    assert_eq!(
        calls
            .lock()
            .expect("calls lock should not be poisoned")
            .as_slice(),
        [
            "https://modules.invalid/entry.js",
            "https://modules.invalid/marker.js"
        ]
    );
    assert!(
        vfs.exists("/workspace/entry-evaluated.txt"),
        "parent module should evaluate only after its transitive static remote import was fetched"
    );
}

#[test]
fn multiline_static_imports_are_prefetched_before_module_evaluation() {
    let (fetcher, calls) = RecordingFetcher::new(vec![(
        "https://modules.invalid/named.js",
        Ok(r#"export const marker = "multiline-ready";"#),
    )]);
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime =
        JsRuntime::with_timeout_and_fetcher(vfs, Duration::from_secs(5), Box::new(fetcher))
            .expect("runtime should be created");

    let output = runtime
        .eval(
            r#"
            import {
              marker as renamedMarker
            } from "https://modules.invalid/named.js";
            renamedMarker;
            "#,
        )
        .expect("multiline static imports should be prefetched");

    assert_eq!(output.result.as_deref(), Some("multiline-ready"));
    assert_eq!(
        calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .as_slice(),
        ["https://modules.invalid/named.js"]
    );
}

#[test]
fn dynamic_remote_imports_that_were_not_static_prefetched_fail_closed() {
    let (fetcher, calls) = RecordingFetcher::new(vec![(
        "https://modules.invalid/dynamic.js",
        Ok(r#"export default "dynamic-loaded";"#),
    )]);
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime =
        JsRuntime::with_timeout_and_fetcher(vfs, Duration::from_secs(5), Box::new(fetcher))
            .expect("runtime should be created");

    let error = execution_message(
        runtime
            .eval(
                r#"
                export {};
                const url = "https://modules.invalid/dynamic.js";
                const module = await import(url);
                module.default;
                "#,
            )
            .expect_err("unprefetched dynamic remote import should fail closed"),
    );

    let lower = error.to_lowercase();
    assert!(
        lower.contains("dynamic") && lower.contains("prefetch"),
        "dynamic remote import errors should clearly identify the fail-closed prefetch policy, got {error:?}"
    );
    assert!(
        error.contains("https://modules.invalid/dynamic.js"),
        "dynamic remote import error should include the rejected URL, got {error:?}"
    );
    assert!(
        calls
            .lock()
            .expect("calls lock should not be poisoned")
            .is_empty(),
        "dynamic remote imports that were not statically prefetched must not reach the ModuleFetcher"
    );
}

#[test]
fn workflow_host_profile_removes_restricted_apis_from_the_shared_runtime() {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_host_api_profile(
        vfs,
        Duration::from_secs(5),
        None,
        None,
        None,
        JsHostApiProfile::workflow(),
    )
    .expect("workflow-profile runtime should be created");

    let output = runtime
        .eval(
            r#"
            [
              typeof globalThis.console,
              typeof globalThis.fs,
              typeof globalThis.process,
              typeof globalThis.fetch,
              typeof globalThis.Date,
              typeof Math.random,
              typeof Promise.resolve
            ].join("|")
            "#,
        )
        .expect("workflow host profile should still evaluate ordinary JavaScript");

    assert_eq!(
        output.result.as_deref(),
        Some("undefined|undefined|undefined|undefined|undefined|undefined|function")
    );
}

#[test]
fn workflow_host_profile_does_not_prefetch_remote_static_imports() {
    let (fetcher, calls) = RecordingFetcher::new(vec![(
        "https://modules.invalid/workflow-denied.js",
        Ok(r#"export default "should-not-fetch";"#),
    )]);
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_host_api_profile(
        vfs,
        Duration::from_secs(5),
        Some(Box::new(fetcher)),
        None,
        None,
        JsHostApiProfile::workflow(),
    )
    .expect("workflow-profile runtime should be created");

    let error = execution_message(
        runtime
            .eval(
                r#"
                import denied from "https://modules.invalid/workflow-denied.js";
                denied;
                "#,
            )
            .expect_err("workflow profile should reject module imports without fetching"),
    );

    assert!(
        error.contains("module") || error.contains("import"),
        "expected module-loading failure, got {error:?}"
    );
    assert!(
        calls
            .lock()
            .expect("fetch calls lock should not be poisoned")
            .is_empty(),
        "workflow profile must not perform remote prefetch side effects"
    );
}
