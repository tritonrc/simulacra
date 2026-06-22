use simulacra_hooks::HookModule;
use simulacra_hooks::error::HookError;
use simulacra_hooks::js::JsHookModule;
use simulacra_hooks::verdict::{Operation, Phase, Verdict};

fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

#[test]
fn js_pass_through_returns_continue() {
    let hook =
        JsHookModule::from_file("pass-through", fixtures_dir().join("pass-through.js"), 5000)
            .unwrap();
    let result = hook
        .invoke(Phase::Before, Operation::ToolCall, "{}")
        .unwrap();
    assert_eq!(result, Verdict::continue_unchanged());
}

#[test]
fn js_deny_hook_blocks_dangerous_tool() {
    let hook =
        JsHookModule::from_file("deny-tool", fixtures_dir().join("deny-tool.js"), 5000).unwrap();
    let context = r#"{"tool": "dangerous_tool"}"#;
    let result = hook
        .invoke(Phase::Before, Operation::ToolCall, context)
        .unwrap();
    assert_eq!(
        result,
        Verdict::Deny("dangerous_tool is not allowed".to_string())
    );
}

#[test]
fn js_deny_hook_allows_safe_tool() {
    let hook =
        JsHookModule::from_file("deny-tool", fixtures_dir().join("deny-tool.js"), 5000).unwrap();
    let context = r#"{"tool": "safe_tool"}"#;
    let result = hook
        .invoke(Phase::Before, Operation::ToolCall, context)
        .unwrap();
    assert_eq!(result, Verdict::continue_unchanged());
}

#[test]
fn js_modify_hook_redacts_ssn() {
    let hook = JsHookModule::from_file(
        "modify-context",
        fixtures_dir().join("modify-context.js"),
        5000,
    )
    .unwrap();
    let context = r#"{"result": "SSN is 123-45-6789"}"#;
    let result = hook
        .invoke(Phase::After, Operation::ToolCall, context)
        .unwrap();
    match result {
        Verdict::Continue(Some(modified)) => {
            assert!(
                modified.contains("***-**-****"),
                "Expected SSN to be redacted, got: {modified}"
            );
            assert!(
                !modified.contains("123-45-6789"),
                "SSN should not appear in redacted output"
            );
        }
        other => panic!("Expected Continue with modified context, got: {other:?}"),
    }
}

#[test]
fn js_timeout_returns_error() {
    let hook =
        JsHookModule::from_file("slow-hook", fixtures_dir().join("slow-hook.js"), 100).unwrap();
    let result = hook.invoke(Phase::Before, Operation::ToolCall, "{}");
    assert!(result.is_err());
    match result.unwrap_err() {
        HookError::Timeout { hook, timeout_ms } => {
            assert_eq!(hook, "slow-hook");
            assert_eq!(timeout_ms, 100);
        }
        other => panic!("Expected Timeout error, got: {other:?}"),
    }
}

#[test]
fn js_fresh_runtime_no_state_between_calls() {
    let script = r#"
var counter = 0;
function invoke(phase, operation, context) {
    counter += 1;
    return { continue: String(counter) };
}
"#;
    let hook = JsHookModule::from_source("stateful", script, 5000);

    // First call
    let result1 = hook
        .invoke(Phase::Before, Operation::ToolCall, "{}")
        .unwrap();
    // Second call — should also return "1" because runtime is fresh
    let result2 = hook
        .invoke(Phase::Before, Operation::ToolCall, "{}")
        .unwrap();

    assert_eq!(result1, Verdict::Continue(Some("1".to_string())));
    assert_eq!(result2, Verdict::Continue(Some("1".to_string())));
}

#[test]
fn js_invalid_return_value_returns_error() {
    let script = r#"
function invoke(phase, operation, context) {
    return 42;
}
"#;
    let hook = JsHookModule::from_source("invalid", script, 5000);
    let result = hook.invoke(Phase::Before, Operation::ToolCall, "{}");
    assert!(result.is_err());
    match result.unwrap_err() {
        HookError::ExecutionError { hook, message } => {
            assert_eq!(hook, "invalid");
            assert!(
                message.contains("object"),
                "Expected error about object, got: {message}"
            );
        }
        other => panic!("Expected ExecutionError, got: {other:?}"),
    }
}
