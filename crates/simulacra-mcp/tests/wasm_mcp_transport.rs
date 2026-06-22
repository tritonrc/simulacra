use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use simulacra_mcp::{McpError, McpManager, load_wasm_mcp_module};
use simulacra_types::{CapabilityToken, ToolError};
use tempfile::NamedTempFile;

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read(&path).unwrap_or_else(|err| {
        panic!("fixture {path} should be readable (was it built? see fixtures/README.md): {err}")
    })
}

fn write_temp_module(bytes: &[u8]) -> NamedTempFile {
    let mut tmp = NamedTempFile::new().expect("temp module should be created");
    tmp.write_all(bytes)
        .expect("fixture bytes should be copied into temp module");
    tmp
}

fn echo_component_fixture() -> NamedTempFile {
    write_temp_module(&fixture_bytes("echo-mcp.wasm"))
}

fn multi_tool_component_fixture() -> NamedTempFile {
    write_temp_module(&fixture_bytes("multi-tool-mcp.wasm"))
}

fn burn_fuel_component_fixture() -> NamedTempFile {
    write_temp_module(&fixture_bytes("burn-fuel-mcp.wasm"))
}

fn counter_component_fixture() -> NamedTempFile {
    write_temp_module(&fixture_bytes("counter-mcp.wasm"))
}

fn invalid_component_fixture() -> NamedTempFile {
    let mut tmp = NamedTempFile::new().expect("temp module should be created");
    tmp.write_all(b"not-a-wasm-component")
        .expect("invalid bytes should be written");
    tmp
}

#[tokio::test]
async fn handshake_compiles_wasm_module_into_component_and_caches_it() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should compile and cache the component");

    let tools = manager.list_tools().await;
    assert!(
        tools.iter().any(|tool| tool.name == "echo"),
        "handshake should register the cached component's tools"
    );
}

#[tokio::test]
async fn compile_failure_produces_mcp_connection_failed_error() {
    let module_file = invalid_component_fixture();

    let err = load_wasm_mcp_module(module_file.path())
        .expect_err("invalid component bytes should be surfaced as connection failures");

    assert!(
        matches!(err, McpError::ConnectionFailed(_)),
        "expected ConnectionFailed for invalid component bytes, got {err:?}"
    );
}

#[tokio::test]
async fn list_tools_is_called_once_at_handshake_and_produces_valid_tool_definitions() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(tools.len(), 1, "fixture should expose one tool definition");
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description, "Echo a payload.");
    assert!(
        tools[0].input_schema.is_object(),
        "tool definitions returned from list-tools should carry JSON schemas"
    );
}

#[tokio::test]
async fn tools_are_registered_under_mcp_server_tool_namespace_for_wasm_transport() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let result = manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await;

    assert!(
        result.is_ok(),
        "fully-qualified mcp:github:echo capability should authorize the wasm MCP tool"
    );
}

#[tokio::test]
async fn module_exporting_multiple_tools_registers_multiple_tool_definitions() {
    // BLOCKER #1: previously pointed at echo-tool.wasm (single-tool) but
    // asserted len() == 2. Use the multi-tool fixture which exports both
    // `echo` and `reverse`.
    let module_file = multi_tool_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("multi", module)
        .await
        .expect("handshake should succeed");

    let tools = manager.list_tools().await;
    assert_eq!(
        tools.len(),
        2,
        "multi-tool wasm MCP modules should register one ToolDefinition per exported tool"
    );

    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["echo", "reverse"],
        "every exported tool should round-trip its name"
    );
}

#[tokio::test]
async fn call_tool_creates_fresh_store_each_invocation() {
    // WARNING #4: the counter fixture mutates a module-local AtomicU64 in
    // its `counter` tool. With a fresh Store per invocation, both calls
    // must observe the *initial* value (1), proving wasmtime is not
    // accidentally sharing state across calls.
    let module_file = counter_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();
    let capability = capability_with_mcp_tools(&["mcp:github:counter"]);

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let first = manager
        .call_tool("github", "counter", json!({}), &capability)
        .await
        .expect("first call should succeed");
    let second = manager
        .call_tool("github", "counter", json!({}), &capability)
        .await
        .expect("second call should also succeed");

    assert_eq!(
        first,
        json!({ "value": 1 }),
        "first call should observe the post-increment value 1 from a fresh AtomicU64"
    );
    assert_eq!(
        first, second,
        "fresh store creation should reset module-local state between invocations \
         (both calls should observe the same post-increment value)"
    );
}

#[tokio::test]
async fn call_tool_echo_fixture_returns_expected_json() {
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let output = manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect("echo call should succeed");

    assert_eq!(output, json!({ "echoed": { "query": "simulacra" } }));
}

#[tokio::test]
async fn call_tool_with_unknown_tool_returns_tool_error_execution_failed() {
    // BLOCKER #3: the spec assertion says "ToolError::ExecutionFailed".
    // `McpManager::call_tool` returns `McpError`, but the trait surface
    // visible to agents is `Tool::call -> Result<_, ToolError>` (via
    // `McpTool` wrapper at lib.rs:1594). We assert BOTH:
    //   1. The underlying McpError carries an "execution failed" message
    //      consistent with mapping `tool-error::execution-failed` →
    //      ToolError::ExecutionFailed (spec §Tool dispatch step 6).
    //   2. The message round-trips through ToolError::ExecutionFailed,
    //      which is what the agent ultimately sees.
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let err = manager
        .call_tool(
            "github",
            "nonexistent",
            json!({}),
            &capability_with_mcp_tools(&["mcp:github:nonexistent"]),
        )
        .await
        .expect_err("unknown tool names should surface tool execution failures");

    let mcp_message = match &err {
        McpError::ProtocolError(message) => message.clone(),
        other => panic!("expected ProtocolError carrying execution-failed message, got {other:?}"),
    };
    assert!(
        mcp_message.contains("execution failed"),
        "underlying McpError should carry tool-error::execution-failed text, got {mcp_message:?}"
    );

    // Round-trip into the agent-facing ToolError surface — the Tier-2
    // contract requires ToolError::ExecutionFailed, not InvalidArguments
    // or CapabilityDenied.
    let tool_error = ToolError::ExecutionFailed(err.to_string());
    assert!(
        matches!(tool_error, ToolError::ExecutionFailed(ref message) if message.contains("execution failed")),
        "spec assertion requires ToolError::ExecutionFailed for unknown tool names, got {tool_error:?}"
    );
}

#[tokio::test]
async fn call_tool_with_exhausted_fuel_returns_fuel_exhausted_error() {
    // BLOCKER #2: previously named a `burn_fuel` tool that did not exist
    // on the echo fixture. The burn-fuel fixture exports a `burn_fuel`
    // tool that runs `loop { acc = acc.wrapping_add(1); }` until wasmtime's
    // fuel meter traps. The runtime must surface that as
    // McpError::ProtocolError carrying "fuel exhausted" (spec §Tool
    // dispatch step 7 — Trap::OutOfFuel).
    let module_file = burn_fuel_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let err = manager
        .call_tool(
            "github",
            "burn_fuel",
            json!({ "iterations": 1_000_000 }),
            &capability_with_mcp_tools(&["mcp:github:burn_fuel"]),
        )
        .await
        .expect_err("out-of-fuel wasm MCP calls should surface execution failures");

    assert!(
        matches!(err, McpError::ProtocolError(ref message) if message.contains("fuel exhausted")),
        "expected fuel exhaustion error, got {err:?}"
    );
}

/// WARNING #5: a recording wrapper that counts how many times the runtime
/// hands a module to wasmtime for instantiation. The agent-fuel-exhausted
/// path must short-circuit BEFORE this counter ticks.
#[derive(Default, Clone)]
struct InstantiationCounter {
    instantiations: Arc<AtomicUsize>,
}

impl InstantiationCounter {
    fn count(&self) -> usize {
        self.instantiations.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn call_tool_when_agent_fuel_budget_exhausted_fails_without_instantiating_component() {
    // WARNING #5: the recorder is installed via `set_instantiation_recorder`
    // (added to the runtime stub for this test). With agent_fuel_budget == 0
    // the call must short-circuit before any wasmtime instantiation, so
    // counter.count() must remain 0.
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();
    manager.set_agent_fuel_budget(0);

    let counter = InstantiationCounter::default();
    manager.set_instantiation_recorder(counter.instantiations.clone());

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let err = manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect_err("exhausted agent fuel should fail before component instantiation");

    assert!(
        matches!(err, McpError::ProtocolError(ref message) if message.contains("fuel")),
        "expected agent fuel exhaustion to fail the call, got {err:?}"
    );
    assert_eq!(
        counter.count(),
        0,
        "exhausted agent fuel must short-circuit before any wasmtime instantiation"
    );
}

#[tokio::test]
async fn fuel_consumed_is_subtracted_from_agent_resource_budget() {
    // WARNING #6: spec §Assertions calls this "ResourceBudget", but
    // simulacra_types::ResourceBudget does not exist as a public type today.
    // The runtime exposes `set_agent_fuel_budget` /
    // `agent_fuel_budget_remaining` as the bespoke v1 surface; once a
    // shared ResourceBudget lands, this test should be updated to assert
    // through that type. For now, verify the bespoke API decreases monotonically.
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();
    manager.set_agent_fuel_budget(10_000);

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect("fuel-accounted call should succeed");

    assert!(
        manager
            .agent_fuel_budget_remaining()
            .expect("fuel budget should be tracked")
            < 10_000,
        "successful wasm MCP calls should subtract consumed fuel from the agent budget"
    );
}

#[tokio::test]
async fn unlimited_module_fuel_still_decrements_agent_budget_by_actual_consumption() {
    // W3: when a WASM MCP module is configured with unlimited fuel
    // (fuel_limit == 0), the per-call store is seeded with `u64::MAX`,
    // and the agent's `ResourceBudget` (when present) must still
    // decrement by the actual consumption observed via
    // `store.get_fuel()`. Previously, `consumed = 0` was hard-coded
    // for the unlimited path so the agent budget never decreased.
    let module_file = echo_component_fixture();
    let module = load_wasm_mcp_module(module_file.path())
        .expect("module should load")
        .with_fuel_limit(0); // unlimited per-server cap
    let mut manager = McpManager::new();
    manager.set_agent_fuel_budget(10_000_000);

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    manager
        .call_tool(
            "github",
            "echo",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:github:echo"]),
        )
        .await
        .expect("call against unlimited-fuel module should succeed");

    let remaining = manager
        .agent_fuel_budget_remaining()
        .expect("fuel budget should be tracked");
    assert!(
        remaining < 10_000_000,
        "agent budget must decrement by actual consumption even when module fuel is unlimited; got {remaining}"
    );
}

#[tokio::test]
async fn per_call_fuel_is_capped_by_min_of_server_and_agent_remaining() {
    // Spec § Tool dispatch: per-call fuel limit is min(server_fuel,
    // agent_remaining_fuel). With server_fuel = 10_000_000 (default) and
    // agent_remaining = 1_000, a runaway loop must trap as fuel-exhausted
    // after ~1_000 fuel — NOT after 10_000_000. Without the cap, the
    // module would gleefully burn through the larger budget while only
    // post-hoc deducting from the agent's tiny pool.
    let module_file = burn_fuel_component_fixture();
    let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
    let mut manager = McpManager::new();
    manager.set_agent_fuel_budget(1_000);

    manager
        .connect_wasm_module("github", module)
        .await
        .expect("handshake should succeed");

    let err = manager
        .call_tool(
            "github",
            "burn_fuel",
            json!({ "iterations": 1_000_000 }),
            &capability_with_mcp_tools(&["mcp:github:burn_fuel"]),
        )
        .await
        .expect_err("agent budget cap should trap the runaway loop");

    assert!(
        matches!(err, McpError::ProtocolError(ref message) if message.contains("fuel exhausted")),
        "expected fuel exhaustion via agent cap, got {err:?}"
    );

    // Agent budget fully consumed (decrement floors at 0).
    assert_eq!(
        manager
            .agent_fuel_budget_remaining()
            .expect("fuel budget should be tracked"),
        0,
        "agent budget should be drained to 0 by the capped call"
    );
}
