//! Regression test: `ToolRegistry::call`'s `tool_invoke` span must not leak
//! onto unrelated code that runs while the tool call is suspended mid-await.
//!
//! `call_raw` (`src/registry.rs`) used to create the `tool_invoke` span and
//! enter it with `span.enter()`, holding that guard across the tool's own
//! `.await`. Per `tracing::Span::enter`'s documented warning, holding an
//! `Entered` guard across an `.await` point does not exit the span when the
//! future yields — only when the guard is dropped. On a single-threaded
//! (or any) executor, any OTHER span created while this future was suspended
//! would see `tool_invoke` as its ambient parent, even though the code
//! creating that other span had nothing to do with this tool call. This test
//! guards against a regression back to that pattern; `call_raw` now wraps
//! the whole async block with `.instrument(span)` instead.

use serde_json::{Value, json};
use simulacra_tool::{CapabilityToken, ToolRegistry};
use simulacra_types::{Tool, ToolDefinition, ToolError};
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A tool whose `call` signals readiness and then waits on an
/// externally-controlled gate before resolving. This lets the test suspend
/// the tool call's future at a known point, mid-`.await`, so it can control
/// exactly when unrelated work runs while `tool_invoke` is (mis-)entered.
struct PausingTool {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
}

impl Tool for PausingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "pausing_tool".into(),
            description: "Test-only tool that pauses mid-call.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn call(
        &self,
        _arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + '_>>
    {
        let entered = Arc::clone(&self.entered);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            entered.notify_one();
            gate.notified().await;
            Ok(json!({"status": "done"}))
        })
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    parent_name: Option<String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        // Contextual parent: whatever span was ambient/current at the moment
        // this span was created (attrs.is_contextual() is true for every
        // span in this test — none specify an explicit `parent:`).
        let parent_name = ctx.lookup_current().map(|span| span.name().to_string());
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            parent_name,
        });
    }
}

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    static TRACING_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
    static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();

    let _guard = TRACING_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture store should only initialize once");
        let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
            spans: Arc::clone(&spans),
        });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
        tracing::callsite::rebuild_interest_cache();
    });

    let spans = CAPTURED_SPANS
        .get()
        .expect("span capture store should be installed");
    spans.lock().unwrap().clear();
    tracing::callsite::rebuild_interest_cache();
    let result = f();
    tracing::callsite::rebuild_interest_cache();
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

#[test]
fn unrelated_span_created_while_a_tool_call_is_suspended_mid_await_must_not_nest_under_tool_invoke()
{
    let (_, spans) = capture_spans(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let entered = Arc::new(tokio::sync::Notify::new());
            let gate = Arc::new(tokio::sync::Notify::new());

            let mut registry = ToolRegistry::new();
            registry
                .register(Box::new(PausingTool {
                    entered: Arc::clone(&entered),
                    gate: Arc::clone(&gate),
                }))
                .expect("test tool registration should succeed");

            let call_task = tokio::spawn(async move {
                registry
                    .call("pausing_tool", json!({}), &CapabilityToken::default())
                    .await
            });

            // Wait until the tool call has entered `tool_invoke` and is now
            // suspended on `gate.notified().await` — a condition to wait on,
            // not a sleep: this is the exact moment a regression to manual
            // `span.enter()` would leave `tool_invoke` wrongly current on
            // this single executor thread while the call is paused.
            entered.notified().await;

            // Unrelated work, logically nothing to do with the paused tool
            // call, creates its own span while `tool_invoke` is suspended.
            let _unrelated = tracing::info_span!("unrelated_op").entered();
            drop(_unrelated);

            // Release the tool call and let it finish.
            gate.notify_one();
            call_task
                .await
                .expect("call task should not panic")
                .expect("pausing_tool call should succeed");
        });
    });

    let tool_invoke = spans
        .iter()
        .find(|s| s.name == "tool_invoke")
        .expect("tool_invoke span should have been recorded");
    assert_eq!(
        tool_invoke.parent_name, None,
        "tool_invoke itself should be a root span in this test"
    );

    let unrelated = spans
        .iter()
        .find(|s| s.name == "unrelated_op")
        .expect("unrelated_op span should have been recorded");
    assert_eq!(
        unrelated.parent_name, None,
        "unrelated_op must not inherit the suspended tool_invoke span as an \
         ambient parent just because it ran while tool_invoke's call() was \
         paused mid-.await — that is the span-context leak this test guards \
         against"
    );
}
