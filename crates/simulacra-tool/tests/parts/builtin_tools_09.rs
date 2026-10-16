// A subscriber installed only on the tool's future (`.with_subscriber`), not
// globally, must still see the sandbox spans the tool opens, even though the
// cell call runs on a blocking-pool thread that has no such default.

struct SpanNameLayer(Arc<Mutex<Vec<String>>>);

impl<S> tracing_subscriber::Layer<S> for SpanNameLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_string());
    }
}

#[test]
fn file_read_sandbox_span_reaches_a_subscriber_scoped_to_the_tool_future() {
    use tracing::instrument::WithSubscriber;

    let capability = full_capability();
    let harness = Harness::new(capability.clone(), unlimited_budget());
    harness.vfs.write("/workspace/a.txt", b"hello").unwrap();

    // Hold the shared lock only while the call runs: a failed assertion below
    // must not poison it for every other test in the binary.
    let guard = registry_call_guard();
    let names = Arc::new(Mutex::new(Vec::new()));
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry::Registry::default().with(SpanNameLayer(Arc::clone(&names))),
    );
    tracing::callsite::rebuild_interest_cache();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    // The call is made inside the scoped future, so the tool's own span is
    // created under the scoped subscriber, as in production supervision.
    let call = async {
        harness
            .registry
            .call(
                "file_read",
                json!({"path": "/workspace/a.txt"}),
                &capability,
            )
            .await
    };
    let result = runtime.block_on(call.with_subscriber(dispatch));
    drop(guard);
    let value = result.expect("file_read should succeed");
    assert_eq!(tool_content(&value), "hello");

    let seen = names.lock().unwrap().clone();
    assert!(
        seen.iter().any(|n| n == "tool_invoke"),
        "positive control: the scoped subscriber saw the tool's own span, got {seen:?}"
    );
    assert!(
        seen.iter().any(|n| n == "sandbox_read_file"),
        "the sandbox span opened on the blocking thread never reached the scoped \
         subscriber, got {seen:?}"
    );
}
