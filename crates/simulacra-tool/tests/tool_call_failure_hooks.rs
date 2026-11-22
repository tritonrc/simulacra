use std::future::{Future, ready};
use std::mem::discriminant;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_tool::ToolRegistry;
use simulacra_types::{CapabilityDenied, CapabilityToken, Tool, ToolDefinition, ToolError};

struct ScriptedTool {
    name: &'static str,
    outcome: Mutex<Option<ToolResult>>,
    owns_hooks: bool,
}

type ToolResult = Result<Value, ToolError>;
type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

impl Tool for ScriptedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Returns a scripted outcome".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn call(&self, _arguments: Value, _capability: &CapabilityToken) -> ToolFuture<'_> {
        Box::pin(ready(self.outcome.lock().unwrap().take().unwrap()))
    }

    fn handles_own_hooks(&self) -> bool {
        self.owns_hooks
    }
}

struct RecordingHook {
    before: fn(Value) -> Verdict,
    after: fn(Value) -> Verdict,
    after_calls: Mutex<Vec<Value>>,
}

impl RecordingHook {
    fn new(after: fn(Value) -> Verdict) -> Self {
        Self {
            before: |_| Verdict::Continue(None),
            after,
            after_calls: Mutex::default(),
        }
    }
}

impl HookModule for RecordingHook {
    fn name(&self) -> &str {
        "recording"
    }

    fn invoke(&self, when: Phase, op: Operation, json: &str) -> Result<Verdict, HookError> {
        assert_eq!(op, Operation::ToolCall);
        let context: Value = serde_json::from_str(json).unwrap();
        if when == Phase::Before {
            return Ok((self.before)(context));
        }
        self.after_calls.lock().unwrap().push(context.clone());
        Ok((self.after)(context))
    }
}

fn register(
    registry: &mut ToolRegistry,
    name: &'static str,
    outcome: ToolResult,
    owns_hooks: bool,
) {
    registry
        .register(Box::new(ScriptedTool {
            name,
            outcome: Mutex::new(Some(outcome)),
            owns_hooks,
        }))
        .unwrap();
}

fn registry(error: ToolError, owns_hooks: bool, hooks: &[&Arc<RecordingHook>]) -> ToolRegistry {
    let mut pipeline = HookPipeline::new();
    for hook in hooks {
        pipeline.add(Operation::ToolCall, (*hook).clone());
    }
    let mut registry = ToolRegistry::new();
    registry.set_pipeline(Arc::new(pipeline));
    register(&mut registry, "failing", Err(error), owns_hooks);
    registry
}

fn call(registry: &ToolRegistry, name: &str) -> ToolError {
    let arguments = json!({"input": "original"});
    let token = CapabilityToken::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(registry.call(name, arguments, &token))
        .expect_err("the caller must receive a tool error")
}

#[track_caller]
fn assert_error(actual: ToolError, expected: &ToolError, message: &str) {
    assert_eq!(discriminant(&actual), discriminant(expected), "{message}");
    assert_eq!(actual.to_string(), expected.to_string(), "{message}");
    if let (ToolError::CapabilityDenied(actual), ToolError::CapabilityDenied(expected)) =
        (&actual, expected)
    {
        assert_eq!(actual.operation, expected.operation, "{message}");
        assert_eq!(actual.reason, expected.reason, "{message}");
    }
}

fn execution_failure() -> ToolError {
    ToolError::ExecutionFailed("disk unavailable".into())
}

fn capability_failure(reason: &str) -> ToolError {
    CapabilityDenied {
        operation: "write report".into(),
        reason: reason.into(),
    }
    .into()
}

fn amended(context: Value) -> Verdict {
    Verdict::Continue(Some(context.to_string()))
}

fn assert_observed_failure(error: fn() -> ToolError, payload: Value) {
    let hook = Arc::new(RecordingHook {
        before: |mut context| {
            context["arguments"] = json!({"input": "rewritten"});
            amended(context)
        },
        ..RecordingHook::new(|_| Verdict::Continue(None))
    });
    let registry = registry(error(), false, &[&hook]);
    let actual = call(&registry, "failing");
    assert_error(actual, &error(), "observer must preserve the error");
    let expected =
        json!({"tool": "failing", "arguments": {"input": "rewritten"}, "error": payload});
    let calls = &*hook.after_calls.lock().unwrap();
    assert_eq!(
        calls,
        &[expected],
        "after-hook error and rewritten arguments"
    );
}

#[test]
fn after_hook_sees_and_preserves_invalid_arguments() {
    assert_observed_failure(
        || ToolError::InvalidArguments("missing path".into()),
        json!({"kind": "invalid_arguments", "detail": "missing path"}),
    );
}

#[test]
fn after_hook_sees_and_preserves_execution_failure() {
    assert_observed_failure(
        execution_failure,
        json!({"kind": "execution_failed", "detail": "disk unavailable"}),
    );
}

#[test]
fn after_hook_sees_and_preserves_capability_denial() {
    assert_observed_failure(
        || capability_failure("read only"),
        json!({"kind": "capability_denied", "detail": {"operation": "write report", "reason": "read only"}}),
    );
}

fn append_to_detail(mut context: Value) -> Verdict {
    let detail = context["error"]["detail"].as_str().unwrap();
    context["error"]["detail"] = json!(format!("{detail}\nTry again later"));
    amended(context)
}

#[test]
fn after_hook_appends_a_line_to_execution_failure() {
    let hook = Arc::new(RecordingHook::new(append_to_detail));
    let registry = registry(execution_failure(), false, &[&hook]);
    let expected = ToolError::ExecutionFailed("disk unavailable\nTry again later".into());
    let actual = call(&registry, "failing");
    assert_error(actual, &expected, "amended execution error");
}

#[test]
fn after_hook_appends_a_line_to_capability_denial() {
    let hook = Arc::new(RecordingHook::new(|mut context| {
        let reason = context["error"]["detail"]["reason"].as_str().unwrap();
        context["error"]["detail"]["reason"] = json!(format!("{reason}\nRequest write access"));
        amended(context)
    }));
    let registry = registry(capability_failure("read only"), false, &[&hook]);
    let expected = capability_failure("read only\nRequest write access");
    let actual = call(&registry, "failing");
    assert_error(actual, &expected, "amended capability denial");
}

fn assert_failure_survives(after: fn(Value) -> Verdict, message: &str) {
    let hook = Arc::new(RecordingHook::new(after));
    let registry = registry(execution_failure(), false, &[&hook]);
    let actual = call(&registry, "failing");
    assert_error(actual, &execution_failure(), message);
    let calls = hook.after_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "{message}: after-hook must run");
}

#[test]
fn removing_error_cannot_erase_a_failure() {
    let remove_error = |mut context: Value| {
        context.as_object_mut().unwrap().remove("error");
        amended(context)
    };
    assert_failure_survives(remove_error, "removing error must preserve failure");
}

#[test]
fn replacing_error_with_result_cannot_erase_a_failure() {
    let return_result = |mut context: Value| {
        context.as_object_mut().unwrap().remove("error");
        context["result"] = json!({"ok": true});
        amended(context)
    };
    assert_failure_survives(return_result, "returning a result must preserve failure");
}

#[test]
fn malformed_error_cannot_erase_a_failure() {
    let malformed_error = |mut context: Value| {
        context["error"] = json!("not a tool error");
        amended(context)
    };
    assert_failure_survives(malformed_error, "malformed error must preserve failure");
}

#[test]
fn after_hooks_run_in_reverse_order_and_pass_along_amendments() {
    let observer = Arc::new(RecordingHook::new(|_| Verdict::Continue(None)));
    let editor = Arc::new(RecordingHook::new(append_to_detail));
    let registry = registry(execution_failure(), false, &[&observer, &editor]);
    let actual = call(&registry, "failing");
    let contexts = observer.after_calls.lock().unwrap();
    let detail = "disk unavailable\nTry again later";
    let seen = contexts
        .first()
        .and_then(|ctx| ctx.pointer("/error/detail"));
    let amended_detail = json!(detail);
    assert_eq!(seen, Some(&amended_detail), "observer must see amendment");
    let expected = ToolError::ExecutionFailed(detail.into());
    assert_error(actual, &expected, "amendment from the chain");
}

fn kill(_: Value) -> Verdict {
    Verdict::Kill("stop requested".into())
}

#[test]
fn after_hook_kill_returns_the_same_error_for_success_and_failure() {
    let hook = Arc::new(RecordingHook::new(kill));
    let mut registry = registry(execution_failure(), false, &[&hook]);
    register(&mut registry, "succeeding", Ok(json!("ok")), false);
    let text = "hook error: hook 'recording' killed agent: stop requested";
    let success = call(&registry, "succeeding");
    assert!(matches!(success, ToolError::ExecutionFailed(_)));
    assert_eq!(success.to_string(), format!("execution failed: {text}"));
    let failure = call(&registry, "failing");
    assert_error(failure, &success, "kill error must match successful call");
}

#[test]
fn a_failed_tool_that_owns_its_hooks_skips_the_after_chain() {
    let hook = Arc::new(RecordingHook::new(kill));
    let registry = registry(execution_failure(), true, &[&hook]);
    let actual = call(&registry, "failing");
    assert_error(actual, &execution_failure(), "tool's own failure");
    assert!(hook.after_calls.lock().unwrap().is_empty());
}

#[test]
fn before_hook_denial_skips_the_after_chain() {
    let hook = Arc::new(RecordingHook {
        before: |_| Verdict::Deny("blocked by test".into()),
        ..RecordingHook::new(kill)
    });
    let registry = registry(execution_failure(), false, &[&hook]);
    let actual = call(&registry, "failing");
    let expected = ToolError::ExecutionFailed("hook denied tool call: blocked by test".into());
    assert_error(actual, &expected, "before-hook denial");
    assert!(hook.after_calls.lock().unwrap().is_empty());
}
