#[test]
fn simulacra_mcp_handshake_span_carries_transport_mode_wasm() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager.connect_wasm_module("github", module).await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_handshake"
                && field_matches(&span.fields, "simulacra.mcp.transport_mode", "wasm")
        }),
        "WASM MCP handshake spans should record transport_mode=wasm"
    );
}

#[test]
fn simulacra_mcp_handshake_span_carries_module_id() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager.connect_wasm_module("github", module).await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_handshake"
                && span.fields.contains_key("simulacra.mcp.module_id")
        }),
        "WASM MCP handshake spans should record simulacra.mcp.module_id"
    );
}

#[test]
fn simulacra_mcp_tool_call_span_carries_simulacra_wasm_fuel_consumed() {
    let module_file = echo_component_fixture();

    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_tool_call"
                && span.fields.contains_key("simulacra.wasm.fuel_consumed")
        }),
        "WASM MCP tool call spans should record consumed fuel"
    );
}

#[test]
fn simulacra_wasm_fuel_consumed_histogram_records_per_call_with_module_and_tool_labels() {
    let module_file = echo_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "echo",
                    json!({ "query": "simulacra" }),
                    &capability_with_mcp_tools(&["mcp:github:echo"]),
                )
                .await
        })
    });

    // After the OTel meter switch, fuel-consumed lands as a real
    // `simulacra.wasm.fuel_consumed` histogram via the meter API (verified
    // end-to-end against Aniani). Locally we assert the mirror
    // structured log carries the labels and a non-zero value so the
    // recording site is exercised by every WASM MCP tool call.
    assert!(
        events.iter().any(|event| {
            event
                .fields
                .get("message")
                .map(|m| m.contains("WASM MCP fuel consumed"))
                .unwrap_or(false)
                && field_matches(&event.fields, "module", "github")
                && field_matches(&event.fields, "tool", "echo")
                && event
                    .fields
                    .get("value")
                    .map(|v| v.parse::<u64>().ok().map(|n| n > 0).unwrap_or(false))
                    .unwrap_or(false)
        }),
        "fuel-consumed log mirror should carry module/tool labels and a non-zero value for each call; got events: {events:?}"
    );
}

#[test]
fn simulacra_mcp_http_fetch_span_records_method_url_host_response_status() {
    // Drives `wasm_mcp_fetch` against an unreachable URL with a permissive
    // allowlist; the transport fails but the span still records its keys
    // (status_code = 0 for failure paths).
    let (_result, spans, _events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("http://127.0.0.1:1/probe"),
                &["127.0.0.1:1".to_string()],
                None,
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_mcp_http_fetch"
                && span.fields.contains_key("http.method")
                && span.fields.contains_key("http.url.host")
                && span.fields.contains_key("http.response.status_code")
        }),
        "outbound simulacra:http/fetch spans should record method, host, and response status"
    );
}

#[test]
fn simulacra_mcp_http_denied_counter_increments_on_capability_or_hook_denial() {
    // Drives `wasm_mcp_fetch` with an empty allowlist so the request is
    // denied before any network IO; the counter increments at that gate.
    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("https://api.github.com/repos"),
                &[],
                None,
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        events.iter().any(|event| {
            field_matches(&event.fields, "counter.simulacra.mcp.http.denied", "1")
                && field_matches(&event.fields, "server", "github")
        }),
        "capability or hook denials should increment simulacra.mcp.http.denied"
    );
}

#[test]
fn tracing_warn_emitted_on_hook_denial_inside_simulacra_http_fetch() {
    // Drives `wasm_mcp_fetch` with a denying `Phase::Before` hook and a
    // permissive allowlist so the request reaches the hook layer.
    let hooks = deny_before_pipeline();
    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            wasm_mcp_fetch(
                "github",
                fetch_request_to("https://api.github.com/repos"),
                &["api.github.com:443".to_string()],
                Some(&hooks),
                None,
                &simulacra_types::AgentId(String::new()),
            )
            .await
        })
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("hook denial"))
        }),
        "hook denials inside simulacra:http/fetch should emit WARN logs"
    );
}

#[test]
fn tracing_error_emitted_on_wasm_trap_during_call_tool() {
    // Uses the trap-mcp fixture whose `trap` tool calls `unreachable!()` —
    // wasmtime surfaces this as a real WASM trap, which the dispatch path
    // logs at ERROR.
    let module_file = trap_component_fixture();

    let (_result, _spans, events) = capture_traces(|| {
        run_async(async {
            let module = load_wasm_mcp_module(module_file.path()).expect("module should load");
            let mut manager = McpManager::new();
            manager
                .connect_wasm_module("github", module)
                .await
                .expect("wasm MCP server should connect");
            manager
                .call_tool(
                    "github",
                    "trap",
                    json!({}),
                    &capability_with_mcp_tools(&["mcp:github:trap"]),
                )
                .await
        })
    });

    assert!(
        events.iter().any(|event| {
            event.level == "ERROR"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("WASM trap"))
        }),
        "WASM traps during call_tool should emit ERROR logs"
    );
}
