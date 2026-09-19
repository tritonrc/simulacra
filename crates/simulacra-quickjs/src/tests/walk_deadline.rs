//! The static-import prefetch walk must stop at its deadline — before every
//! fetch, so both deep chains and wide sibling graphs are bounded.

use super::support::*;

/// A WIDE graph — one root importing many siblings — must stop fetching at
/// the deadline: the deadline binds every fetch, not just each stack item.
#[test]
fn prefetch_walk_deadline_binds_wide_graphs_too() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct SlowWideFetcher {
        attempts: Arc<AtomicUsize>,
    }

    impl ModuleFetcher for SlowWideFetcher {
        fn fetch(&self, _url: &str) -> Result<String, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            Ok("export const x = 1;".to_string())
        }
    }

    // 60 siblings, each a slow fetch; the deadline is 60ms, so at most a
    // handful can start before the walk must stop.
    let program = (0..60)
        .map(|index| format!("import 'https://wide.example/s{index}.js';"))
        .collect::<Vec<_>>()
        .join("\n");

    let attempts = Arc::new(AtomicUsize::new(0));
    let runtime = JsRuntime::with_timeout_and_fetcher(
        Arc::new(MemoryFs::new()) as Arc<dyn VirtualFs>,
        Duration::from_millis(60),
        Box::new(SlowWideFetcher {
            attempts: attempts.clone(),
        }),
    )
    .expect("failed to create runtime");

    let error = runtime
        .eval(&program)
        .expect_err("the walk must hit its deadline before 60 slow fetches");

    assert!(
        matches!(error, JsError::Timeout),
        "deadline breach should be reported as JsError::Timeout, got {error:?}"
    );

    std::thread::sleep(Duration::from_millis(700));
    let observed = attempts.load(Ordering::SeqCst);
    assert!(
        observed >= 2,
        "traversal must have started — a walk that never fetched anything \
         would trivially satisfy the bound (observed {observed})"
    );
    assert!(
        observed < 10,
        "a wide graph must not fetch past the deadline; observed {observed} of 60"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        observed,
        "the walk must have terminated; attempts kept growing after the deadline"
    );
}

#[test]
fn prefetch_remote_static_import_walk_stops_at_deadline() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct SlowChainFetcher {
        attempts: Arc<AtomicUsize>,
        graph_size: usize,
    }

    impl ModuleFetcher for SlowChainFetcher {
        fn fetch(&self, url: &str) -> Result<String, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));

            let index = url
                .strip_prefix("https://slow.example/m")
                .and_then(|rest| rest.strip_suffix(".js"))
                .and_then(|digits| digits.parse::<usize>().ok())
                .expect("test URL should be in the generated chain");

            if index + 1 >= self.graph_size {
                Ok("export default 1;".to_string())
            } else {
                Ok(format!(
                    "import value from 'https://slow.example/m{}.js'; export default value;",
                    index + 1
                ))
            }
        }
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let graph_size = 100;
    let vfs = Arc::new(MemoryFs::new());
    let runtime = JsRuntime::with_timeout_and_fetcher(
        vfs as Arc<dyn VirtualFs>,
        Duration::from_millis(60),
        Box::new(SlowChainFetcher {
            attempts: attempts.clone(),
            graph_size,
        }),
    )
    .expect("failed to create runtime");

    let error = runtime
        .eval(
            r#"
            import value from "https://slow.example/m0.js";
            value;
            "#,
        )
        .expect_err("prefetch walk should stop at the runtime deadline");

    assert!(
        matches!(error, JsError::Timeout),
        "deadline breach should be reported as JsError::Timeout, got {error:?}"
    );

    std::thread::sleep(Duration::from_millis(700));
    let observed = attempts.load(Ordering::SeqCst);
    assert!(
        observed >= 2,
        "traversal must have started before the deadline — a walk that never \
         fetched anything would trivially satisfy the bound (observed {observed})"
    );
    assert!(
        observed < 20,
        "prefetch walk must stop at its own deadline; observed {observed} attempts out of {graph_size}"
    );

    // Termination, not just slowness: once observed, the attempt count must
    // be stable — a still-running detached walk keeps incrementing it.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        observed,
        "the walk must have terminated; attempts kept growing after the deadline"
    );
}
