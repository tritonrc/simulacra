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
    null_hook_input: bool,
    received_arguments: Option<Arc<Mutex<Option<Value>>>>,
}

type ToolResult = Result<Value, ToolError>;
type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

impl ScriptedTool {
    fn new(name: &'static str, outcome: ToolResult, owns_hooks: bool) -> Self {
        Self {
            name,
            outcome: Mutex::new(Some(outcome)),
            owns_hooks,
            null_hook_input: false,
            received_arguments: None,
        }
    }
}

impl Tool for ScriptedTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Returns a scripted outcome".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn call(&self, arguments: Value, _capability: &CapabilityToken) -> ToolFuture<'_> {
        if let Some(received) = &self.received_arguments {
            *received.lock().unwrap() = Some(arguments);
        }
        Box::pin(ready(self.outcome.lock().unwrap().take().unwrap()))
    }

    fn hook_input_payload(&self, name: &str, arguments: &Value) -> Value {
        if self.null_hook_input {
            json!(null)
        } else {
            json!({"tool": name, "arguments": arguments})
        }
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
        .register(Box::new(ScriptedTool::new(name, outcome, owns_hooks)))
        .unwrap();
}

fn registry(error: ToolError, owns_hooks: bool, hooks: &[&Arc<RecordingHook>]) -> ToolRegistry {
    let mut registry = hooked_registry(hooks);
    register(&mut registry, "failing", Err(error), owns_hooks);
    registry
}

fn hooked_registry(hooks: &[&Arc<RecordingHook>]) -> ToolRegistry {
    let mut pipeline = HookPipeline::new();
    for hook in hooks {
        pipeline.add(Operation::ToolCall, (*hook).clone());
    }
    let mut registry = ToolRegistry::new();
    registry.set_pipeline(Arc::new(pipeline));
    registry
}

fn call(registry: &ToolRegistry, name: &str) -> ToolError {
    call_with_arguments(registry, name, json!({"input": "original"}))
        .expect_err("the caller must receive a tool error")
}

fn call_with_arguments(registry: &ToolRegistry, name: &str, arguments: Value) -> ToolResult {
    let token = CapabilityToken::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(registry.call(name, arguments, &token))
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

#[test]
fn after_hook_sees_failure_context_with_custom_tool_payload() {
    let hook = Arc::new(RecordingHook::new(|_| Verdict::Continue(None)));
    let mut registry = hooked_registry(&[&hook]);
    registry
        .register(Box::new(ScriptedTool {
            null_hook_input: true,
            ..ScriptedTool::new("failing", Err(execution_failure()), false)
        }))
        .unwrap();

    let actual = call(&registry, "failing");
    assert_error(actual, &execution_failure(), "tool's own failure");
    let expected = json!({
        "tool": "failing",
        "arguments": {"input": "original"},
        "error": {"kind": "execution_failed", "detail": "disk unavailable"}
    });
    assert_eq!(
        &*hook.after_calls.lock().unwrap(),
        &[expected],
        "after-hook must receive the full failure context"
    );
}

#[test]
fn after_hook_changes_the_failure_kind_and_text() {
    let hook = Arc::new(RecordingHook::new(|mut context| {
        context["error"] = json!({"kind": "invalid_arguments", "detail": "choose another path"});
        amended(context)
    }));
    let registry = registry(execution_failure(), false, &[&hook]);
    let expected = ToolError::InvalidArguments("choose another path".into());
    assert_error(call(&registry, "failing"), &expected, "replaced tool error");
}

#[test]
fn after_hook_returning_invalid_json_preserves_the_tool_error() {
    assert_failure_survives(
        |_| Verdict::Continue(Some("not json".into())),
        "invalid JSON must preserve the tool error",
    );
}

fn number_changed_by_json_text() -> Value {
    let number = json!(51.248178375505404_f64);
    assert_ne!(
        serde_json::from_str::<Value>(&number.to_string()).unwrap(),
        number
    );
    number
}

#[test]
fn unchanged_hook_context_preserves_the_tool_result() {
    let expected = json!({"measurement": number_changed_by_json_text()});
    let hook = Arc::new(RecordingHook::new(|_| Verdict::Continue(None)));
    let mut registry = hooked_registry(&[&hook]);
    register(&mut registry, "succeeding", Ok(expected.clone()), false);

    let actual = call_with_arguments(&registry, "succeeding", json!({})).unwrap();
    assert_eq!(
        actual, expected,
        "caller must receive the exact tool result"
    );
}

#[test]
fn unchanged_hook_context_preserves_the_tool_arguments() {
    let arguments = json!({"measurement": number_changed_by_json_text()});
    let received = Arc::new(Mutex::new(None));
    let hook = Arc::new(RecordingHook::new(|_| Verdict::Continue(None)));
    let mut registry = hooked_registry(&[&hook]);
    registry
        .register(Box::new(ScriptedTool {
            received_arguments: Some(received.clone()),
            ..ScriptedTool::new("succeeding", Ok(json!("ok")), false)
        }))
        .unwrap();

    call_with_arguments(&registry, "succeeding", arguments.clone()).unwrap();
    assert_eq!(
        received.lock().unwrap().as_ref(),
        Some(&arguments),
        "tool must receive the exact call arguments"
    );
}
