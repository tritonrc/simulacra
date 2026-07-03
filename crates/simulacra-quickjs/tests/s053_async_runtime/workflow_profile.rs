use super::support::*;

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
