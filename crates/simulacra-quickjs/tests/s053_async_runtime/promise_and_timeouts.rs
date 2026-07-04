use super::support::*;

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
