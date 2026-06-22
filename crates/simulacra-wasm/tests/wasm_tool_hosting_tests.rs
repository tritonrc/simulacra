//! Integration tests for WasmHost and WasmTool.
//!
//! These tests exercise the full WASM tool hosting pipeline:
//! engine creation, module loading, tool discovery, sandboxed execution,
//! and fuel metering using the echo-tool.wasm fixture.
//!
//! Additionally, sandbox enforcement tests verify WASI filesystem
//! and environment variable isolation using the sandbox-test-tool fixture.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use simulacra_types::{CapabilityToken, PathPattern, Tool};
use simulacra_wasm::{WasiMount, WasiToolConfig, WasmHost, WasmTool};

const ECHO_FIXTURE: &str = "fixtures/echo-tool.wasm";
const SANDBOX_FIXTURE: &str = "fixtures/sandbox-test-tool.wasm";

fn default_capability() -> CapabilityToken {
    CapabilityToken::default()
}

fn capability_for_mount(host_path: &Path, write: bool) -> CapabilityToken {
    let pattern = PathPattern(format!("{}/**", host_path.display()));
    if write {
        CapabilityToken {
            paths_write: vec![pattern],
            ..Default::default()
        }
    } else {
        CapabilityToken {
            paths_read: vec![pattern],
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// WasmHost tests
// ---------------------------------------------------------------------------

#[test]
fn wasm_host_creates_successfully() {
    let host = WasmHost::new();
    assert!(host.is_ok(), "WasmHost::new() should succeed");
}

#[test]
fn load_valid_module_succeeds() {
    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("echo", Path::new(ECHO_FIXTURE));
    assert!(
        result.is_ok(),
        "loading echo-tool.wasm should succeed: {:?}",
        result.err()
    );
    assert!(
        host.component("echo").is_some(),
        "component should be cached after loading"
    );
}

#[test]
fn load_nonexistent_returns_error() {
    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("missing", Path::new("fixtures/does-not-exist.wasm"));
    assert!(result.is_err(), "loading missing file should fail");
    match result.unwrap_err() {
        simulacra_wasm::WasmError::ModuleLoadFailed(msg) => {
            assert!(!msg.is_empty(), "error message should be non-empty");
        }
        other => panic!("expected ModuleLoadFailed, got {:?}", other),
    }
}

#[test]
fn load_invalid_file_returns_error() {
    // Create a temp file with garbage content.
    let dir = std::env::temp_dir().join("simulacra-wasm-test-invalid");
    std::fs::create_dir_all(&dir).unwrap();
    let garbage_path = dir.join("garbage.wasm");
    std::fs::write(&garbage_path, b"this is not a valid wasm file").unwrap();

    let mut host = WasmHost::new().unwrap();
    let result = host.load_module("garbage", &garbage_path);
    assert!(result.is_err(), "loading garbage file should fail");
    match result.unwrap_err() {
        simulacra_wasm::WasmError::ModuleLoadFailed(msg) => {
            assert!(!msg.is_empty(), "error message should be non-empty");
        }
        other => panic!("expected ModuleLoadFailed, got {:?}", other),
    }

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_tools_returns_definitions() {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new(ECHO_FIXTURE)).unwrap();

    let tools = host.discover_tools("echo").unwrap();
    assert!(
        !tools.is_empty(),
        "echo-tool should export at least one tool"
    );

    let echo = tools.iter().find(|t| t.name == "echo");
    assert!(echo.is_some(), "should find a tool named 'echo'");

    let echo = echo.unwrap();
    assert!(
        !echo.description.is_empty(),
        "echo tool should have a description"
    );
    assert!(
        echo.input_schema.is_object(),
        "input_schema should be a JSON object, got: {}",
        echo.input_schema
    );
}

// ---------------------------------------------------------------------------
// WasmTool tests
// ---------------------------------------------------------------------------

fn create_echo_tool(fuel_limit: u64) -> WasmTool {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new(ECHO_FIXTURE)).unwrap();

    let defs = host.discover_tools("echo").unwrap();
    let echo_def = defs.into_iter().find(|d| d.name == "echo").unwrap();

    let engine = host.engine().clone();
    let component = host.component("echo").unwrap().clone();

    WasmTool::new(
        engine,
        component,
        echo_def,
        WasiToolConfig::default(),
        fuel_limit,
    )
}

#[tokio::test]
async fn wasm_tool_definition_is_correct() {
    let tool = create_echo_tool(0);
    let def = tool.definition();
    assert_eq!(def.name, "echo");
    assert!(!def.description.is_empty());
    assert!(def.input_schema.is_object());
}

#[tokio::test]
async fn wasm_tool_call_returns_correct_result() {
    let tool = create_echo_tool(0);
    let cap = default_capability();

    let args = serde_json::json!({"message": "hello world"});
    let result = tool.call(args.clone(), &cap).await;
    assert!(
        result.is_ok(),
        "echo tool call should succeed: {:?}",
        result.err()
    );

    let value = result.unwrap();
    // The echo tool returns its arguments back.
    assert_eq!(
        value, args,
        "echo tool should return the same arguments it received"
    );
}

#[tokio::test]
async fn wasm_tool_unknown_tool_returns_error() {
    let mut host = WasmHost::new().unwrap();
    host.load_module("echo", Path::new(ECHO_FIXTURE)).unwrap();

    let engine = host.engine().clone();
    let component = host.component("echo").unwrap().clone();

    // Create a WasmTool with a tool name that doesn't exist in the component.
    let fake_def = simulacra_types::ToolDefinition {
        name: "nonexistent-tool".to_string(),
        description: "does not exist".to_string(),
        input_schema: serde_json::json!({}),
    };

    let tool = WasmTool::new(engine, component, fake_def, WasiToolConfig::default(), 0);

    let cap = default_capability();
    let result = tool.call(serde_json::json!({}), &cap).await;
    assert!(
        result.is_err(),
        "calling a nonexistent tool name should fail"
    );
}

#[tokio::test]
async fn wasm_tool_fuel_exhaustion_returns_error() {
    // Give extremely little fuel so the tool traps.
    let tool = create_echo_tool(1);
    let cap = default_capability();

    let result = tool.call(serde_json::json!({"message": "hi"}), &cap).await;
    assert!(
        result.is_err(),
        "with fuel=1 the tool should run out of fuel"
    );

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("fuel"),
        "error should mention fuel exhaustion, got: {}",
        msg
    );
}

#[tokio::test]
async fn wasm_tool_reports_fuel_consumed() {
    let tool = create_echo_tool(0);
    let cap = default_capability();

    let _ = tool
        .call(serde_json::json!({"message": "test"}), &cap)
        .await
        .unwrap();

    let consumed = tool.last_fuel_consumed();
    assert!(
        consumed > 0,
        "fuel consumed should be > 0 after a successful call, got {}",
        consumed
    );
}

#[tokio::test]
async fn wasm_tool_isolation_no_state_between_calls() {
    let tool = create_echo_tool(0);
    let cap = default_capability();

    let args1 = serde_json::json!({"message": "first"});
    let args2 = serde_json::json!({"message": "second"});

    let result1 = tool.call(args1.clone(), &cap).await.unwrap();
    let result2 = tool.call(args2.clone(), &cap).await.unwrap();

    // Each call should return its own arguments, proving no state leakage.
    assert_eq!(result1, args1, "first call should return first args");
    assert_eq!(result2, args2, "second call should return second args");
    assert_ne!(result1, result2, "results should differ between calls");
}

// ---------------------------------------------------------------------------
// Agent-level fuel budget tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_fuel_budget_subtracts_consumed() {
    let initial_budget: u64 = 100_000_000;
    let agent_fuel = Arc::new(AtomicU64::new(initial_budget));

    let mut tool = create_echo_tool(0);
    tool.set_agent_fuel(agent_fuel.clone());

    let cap = default_capability();
    let _ = tool
        .call(serde_json::json!({"message": "test"}), &cap)
        .await
        .unwrap();

    let remaining = agent_fuel.load(Ordering::Relaxed);
    assert!(
        remaining < initial_budget,
        "agent fuel remaining ({}) should be less than initial ({})",
        remaining,
        initial_budget
    );
}

#[tokio::test]
async fn agent_fuel_budget_exhausted_fails_without_instantiation() {
    // Set agent fuel to 0 (exhausted).
    let agent_fuel = Arc::new(AtomicU64::new(0));

    let mut tool = create_echo_tool(0);
    tool.set_agent_fuel(agent_fuel);

    let cap = default_capability();
    let result = tool
        .call(serde_json::json!({"message": "test"}), &cap)
        .await;

    assert!(result.is_err(), "should fail when agent fuel is exhausted");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("fuel"),
        "error should mention fuel: {}",
        msg
    );
}

// ---------------------------------------------------------------------------
// Sandbox enforcement tests (using sandbox-test-tool)
// ---------------------------------------------------------------------------

/// Create a WasmTool for the sandbox-test-tool targeting a specific sub-tool.
fn create_sandbox_tool(tool_name: &str, wasi_config: WasiToolConfig) -> WasmTool {
    let mut host = WasmHost::new().unwrap();
    host.load_module("sandbox", Path::new(SANDBOX_FIXTURE))
        .unwrap();

    let defs = host.discover_tools("sandbox").unwrap();
    let def = defs
        .into_iter()
        .find(|d| d.name == tool_name)
        .unwrap_or_else(|| panic!("sandbox-test-tool should export '{}'", tool_name));

    let engine = host.engine().clone();
    let component = host.component("sandbox").unwrap().clone();

    WasmTool::new(engine, component, def, wasi_config, 0)
}

#[tokio::test]
async fn wasi_read_file_from_preopened_ro_dir() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("hello.txt");
    std::fs::write(&file_path, "hello from sandbox").unwrap();

    let config = WasiToolConfig {
        fs: vec![WasiMount {
            host: dir.path().to_str().unwrap().to_string(),
            guest: "/data".to_string(),
            perms: "ro".to_string(),
        }],
        env: vec![],
    };

    let tool = create_sandbox_tool("read_file", config);
    let cap = capability_for_mount(dir.path(), false);

    let args = serde_json::json!({"path": "data/hello.txt"});
    let result = tool.call(args, &cap).await;
    assert!(
        result.is_ok(),
        "reading a file from a preopened ro dir should succeed: {:?}",
        result.err()
    );

    let value = result.unwrap();
    assert_eq!(
        value["content"], "hello from sandbox",
        "file content should match what was written"
    );
}

#[tokio::test]
async fn wasi_write_to_ro_dir_fails() {
    let dir = tempfile::tempdir().unwrap();

    let config = WasiToolConfig {
        fs: vec![WasiMount {
            host: dir.path().to_str().unwrap().to_string(),
            guest: "/data".to_string(),
            perms: "ro".to_string(),
        }],
        env: vec![],
    };

    let tool = create_sandbox_tool("write_file", config);
    let cap = capability_for_mount(dir.path(), false);

    let args = serde_json::json!({"path": "data/test.txt", "content": "should fail"});
    let result = tool.call(args, &cap).await;
    assert!(result.is_err(), "writing to a read-only mount should fail");
}

#[tokio::test]
async fn wasi_write_to_rw_dir_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();

    let config = WasiToolConfig {
        fs: vec![WasiMount {
            host: dir_path.to_str().unwrap().to_string(),
            guest: "/data".to_string(),
            perms: "rw".to_string(),
        }],
        env: vec![],
    };

    let tool = create_sandbox_tool("write_file", config);
    let cap = capability_for_mount(&dir_path, true);

    let args = serde_json::json!({"path": "data/test.txt", "content": "hello"});
    let result = tool.call(args, &cap).await;
    assert!(
        result.is_ok(),
        "writing to a rw mount should succeed: {:?}",
        result.err()
    );

    let value = result.unwrap();
    assert_eq!(value["written"], true, "response should confirm write");

    // Verify the file actually exists on the host filesystem.
    let host_file = dir_path.join("test.txt");
    assert!(host_file.exists(), "file should exist on host after write");
    let content = std::fs::read_to_string(&host_file).unwrap();
    assert_eq!(content, "hello", "host file content should match");
}

#[tokio::test]
async fn wasi_path_outside_mount_fails() {
    let dir = tempfile::tempdir().unwrap();

    let config = WasiToolConfig {
        fs: vec![WasiMount {
            host: dir.path().to_str().unwrap().to_string(),
            guest: "/data".to_string(),
            perms: "rw".to_string(),
        }],
        env: vec![],
    };

    let tool = create_sandbox_tool("read_file", config);
    let cap = capability_for_mount(dir.path(), true);

    let args = serde_json::json!({"path": "/etc/passwd"});
    let result = tool.call(args, &cap).await;
    assert!(
        result.is_err(),
        "reading a path outside any mount should fail"
    );
}

#[tokio::test]
async fn wasi_env_only_allowlisted_vars_visible() {
    // The WASI sandbox only sees env vars explicitly passed in WasiToolConfig.env.
    // Process env vars are NOT inherited — the guest starts with an empty environment
    // plus only the KEY=VALUE pairs we configure.
    let config = WasiToolConfig {
        fs: vec![],
        env: vec!["SIMULACRA_SANDBOX_TEST_ALLOWED=visible".to_string()],
    };

    // Allowed var should be visible inside the guest.
    let tool_allowed = create_sandbox_tool("read_env", config.clone());
    let cap = default_capability();
    let args = serde_json::json!({"name": "SIMULACRA_SANDBOX_TEST_ALLOWED"});
    let result = tool_allowed.call(args, &cap).await;
    assert!(
        result.is_ok(),
        "reading an allowed env var should succeed: {:?}",
        result.err()
    );
    let value = result.unwrap();
    assert_eq!(
        value["value"], "visible",
        "allowed env var should return the configured value"
    );

    // A var NOT in the config should be empty inside the guest,
    // even if it exists in the host process environment.
    let tool_secret = create_sandbox_tool("read_env", config);
    let cap2 = default_capability();
    let args = serde_json::json!({"name": "SIMULACRA_SANDBOX_TEST_SECRET"});
    let result = tool_secret.call(args, &cap2).await;
    assert!(
        result.is_ok(),
        "reading a non-configured env var should succeed (returns empty): {:?}",
        result.err()
    );
    let value = result.unwrap();
    assert_eq!(
        value["value"], "",
        "non-configured env var should return empty string"
    );
}
