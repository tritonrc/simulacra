//! Scripts reach an embedder's functions through `tools`, under a wall-clock
//! budget, a heap cap and a bounded console.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use simulacra_quickjs::{BoxFuture, JsError, JsHost, JsRuntime, JsRuntimeOptions};
use simulacra_vfs::MemoryFs;

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeHost {
    delay: Duration,
    calls: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl JsHost for FakeHost {
    fn names(&self) -> Vec<String> {
        vec!["echo".into(), "fail".into(), "slow".into()]
    }

    fn call(&self, name: String, args_json: String) -> BoxFuture<'static, Result<String, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let delay = self.delay;
        let guard = SetOnDrop(Arc::clone(&self.dropped));
        Box::pin(async move {
            let _guard = guard;
            match name.as_str() {
                "fail" => Err("boom".to_string()),
                "slow" => {
                    tokio::time::sleep(delay).await;
                    Ok(args_json)
                }
                _ => Ok(args_json),
            }
        })
    }
}

fn runtime(host: Option<FakeHost>, timeout: Duration) -> JsRuntime {
    let options = JsRuntimeOptions {
        host: host.map(|h| Arc::new(h) as Arc<dyn JsHost>),
        timeout: Some(timeout),
        ..Default::default()
    };
    JsRuntime::new(Arc::new(MemoryFs::new()))
        .unwrap()
        .with_runtime_options(options)
}

fn with_options(options: JsRuntimeOptions) -> JsRuntime {
    JsRuntime::new(Arc::new(MemoryFs::new()))
        .unwrap()
        .with_runtime_options(options)
}

#[test]
fn tools_echo_returns_the_parsed_result() {
    let rt = runtime(Some(FakeHost::default()), Duration::from_secs(5));
    let out = rt
        .eval("(async () => JSON.stringify(await tools.echo({a: 1})))()")
        .unwrap();
    assert_eq!(out.result.as_deref(), Some(r#"{"a":1}"#));
}

#[test]
fn host_error_rejects_with_a_catchable_error() {
    let rt = runtime(Some(FakeHost::default()), Duration::from_secs(5));
    let out = rt
        .eval(
            "(async () => { try { await tools.fail({}); return 'no'; } \
             catch (e) { return e instanceof Error ? e.message : 'not an Error'; } })()",
        )
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("boom"));
}

#[test]
fn host_calls_run_concurrently() {
    let host = FakeHost {
        delay: Duration::from_secs(1),
        ..Default::default()
    };
    let rt = runtime(Some(host), Duration::from_secs(60));
    let start = Instant::now();
    let out = rt
        .eval("(async () => (await Promise.all([1, 2, 3].map(n => tools.slow({n})))).length)()")
        .unwrap();
    assert_eq!(out.result.as_deref(), Some("3"));
    assert!(
        start.elapsed() < Duration::from_millis(1800),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn host_calls_outlasting_the_timeout_are_dropped() {
    let host = FakeHost {
        delay: Duration::from_secs(30),
        ..Default::default()
    };
    let dropped = Arc::clone(&host.dropped);
    let rt = runtime(Some(host), Duration::from_millis(300));
    let start = Instant::now();
    let err = rt.eval("(async () => await tools.slow({}))()").unwrap_err();
    assert!(matches!(err, JsError::Timeout), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(
        dropped.load(Ordering::SeqCst),
        "in-flight future must be dropped"
    );
}

#[test]
fn busy_loop_times_out_with_and_without_a_host() {
    let with = runtime(Some(FakeHost::default()), Duration::from_millis(300));
    assert!(matches!(
        with.eval("while (true) {}"),
        Err(JsError::Timeout)
    ));

    let without = runtime(None, Duration::from_secs(5));
    assert_eq!(
        without.eval("typeof tools").unwrap().result.as_deref(),
        Some("undefined")
    );
    let start = Instant::now();
    assert!(matches!(
        without.eval("while (true) {}"),
        Err(JsError::Timeout)
    ));
    assert!(start.elapsed() >= Duration::from_secs(5));
}

#[test]
fn oversized_arguments_are_rejected_without_calling_the_host() {
    let host = FakeHost::default();
    let calls = Arc::clone(&host.calls);
    let rt = with_options(JsRuntimeOptions {
        host: Some(Arc::new(host)),
        host_arg_cap: Some(1024),
        ..Default::default()
    });
    let out = rt
        .eval(
            "(async () => { try { await tools.echo({s: 'x'.repeat(2000)}); return 'sent'; } \
             catch (e) { return e.message; } })()",
        )
        .unwrap();
    assert!(out.result.unwrap().contains("exceed"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn arguments_over_the_byte_cap_are_rejected_even_when_short_in_utf16() {
    let host = FakeHost::default();
    let calls = Arc::clone(&host.calls);
    let rt = with_options(JsRuntimeOptions {
        host: Some(Arc::new(host)),
        host_arg_cap: Some(1024),
        ..Default::default()
    });
    // 600 three-byte characters: 600 UTF-16 units, 1800 bytes.
    let out = rt
        .eval(
            "(async () => { try { await tools.echo({s: '\\u20ac'.repeat(600)}); return 'sent'; } \
             catch (e) { return e.message; } })()",
        )
        .unwrap();
    assert!(out.result.unwrap().contains("exceed"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn allocation_past_the_memory_limit_fails_and_a_fresh_runtime_still_works() {
    let limited = JsRuntimeOptions {
        memory_limit: Some(8 * 1024 * 1024),
        ..Default::default()
    };
    let result = with_options(limited.clone())
        .eval("const keep = []; while (true) { keep.push('x'.repeat(1 << 20) + keep.length); }");
    assert!(matches!(result, Err(JsError::Execution(_))), "{result:?}");

    let out = with_options(limited).eval("1 + 1").unwrap();
    assert_eq!(out.result.as_deref(), Some("2"));
}

const CAP: usize = 256;

#[test]
fn every_console_level_stops_at_the_shared_cap_and_marks_truncation() {
    for level in ["log", "error", "warn", "info", "debug"] {
        let rt = with_options(JsRuntimeOptions {
            console_cap: Some(CAP),
            ..Default::default()
        });
        let out = rt
            .eval(&format!(
                "for (let i = 0; i < 100; i++) console.{level}('line ' + i + ' ' + 'x'.repeat(20));"
            ))
            .unwrap();
        assert!(
            out.stdout.ends_with("[output truncated]\n"),
            "{level}: {}",
            out.stdout
        );
        assert!(
            out.stdout.len() <= CAP + "[output truncated]\n".len(),
            "{level}"
        );
    }
}

#[test]
fn console_levels_share_one_budget() {
    let rt = with_options(JsRuntimeOptions {
        console_cap: Some(CAP),
        ..Default::default()
    });
    let out = rt
        .eval(
            "for (let i = 0; i < 100; i++) { console.log('a'.repeat(20)); console.error('b'.repeat(20)); }",
        )
        .unwrap();
    assert!(out.stdout.contains("[ERROR]") && out.stdout.contains("[output truncated]"));
    assert!(out.stdout.len() <= CAP + "[output truncated]\n".len());
}

#[test]
fn output_under_the_cap_is_not_marked() {
    let rt = with_options(JsRuntimeOptions {
        console_cap: Some(CAP),
        ..Default::default()
    });
    assert_eq!(rt.eval("console.log('hi')").unwrap().stdout, "hi\n");
}

#[test]
fn nested_arrays_around_a_large_string_log_within_the_cap_quickly() {
    let rt = with_options(JsRuntimeOptions {
        console_cap: Some(64 * 1024),
        ..Default::default()
    });
    let start = Instant::now();
    let out = rt
        .eval("console.log(Array(100).fill(Array(100).fill(Array(100).fill('x'.repeat(1 << 20)))))")
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(out.stdout.len() <= 64 * 1024 + "[output truncated]\n".len());
    assert!(out.stdout.ends_with("[output truncated]\n"));
}
