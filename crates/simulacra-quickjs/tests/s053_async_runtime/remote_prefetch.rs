use super::support::*;

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
