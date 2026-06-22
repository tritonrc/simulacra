use serde::Deserialize;
use serde_json::json;
use simulacra_config::{
    ConfigError, McpConfig, McpServerConfig, WasiToolConfig, WasmConfig, validate_mcp_server,
};
use simulacra_mcp::{McpError, McpManager, parse_wasm_transport};
use simulacra_types::CapabilityToken;
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

#[derive(Debug, Deserialize)]
struct McpSection {
    mcp: McpConfig,
}

#[derive(Debug, Deserialize)]
struct WasmSection {
    wasm: WasmConfig,
}

fn parse_mcp_server(toml_str: &str) -> McpServerConfig {
    let mut parsed: McpSection = toml::from_str(toml_str).expect("mcp config should parse");
    parsed
        .mcp
        .servers
        .pop()
        .expect("mcp config should contain one server")
}

#[test]
fn wasm_transport_value_is_accepted_in_mcp_server_config() {
    let server = parse_mcp_server(
        r#"
[mcp]

[[mcp.servers]]
name = "github"
transport = "wasm"
module = "/tmp/x.wasm"
"#,
    );

    assert_eq!(server.transport.as_deref(), Some("wasm"));
    assert_eq!(server.module.as_deref(), Some("/tmp/x.wasm"));
    parse_wasm_transport(
        server
            .transport
            .as_deref()
            .expect("transport should be present"),
    )
    .expect("wasm transport should map to the planned transport API");
}

#[test]
fn wasm_transport_without_module_returns_typed_config_error() {
    let server = parse_mcp_server(
        r#"
[mcp]

[[mcp.servers]]
name = "github"
transport = "wasm"
"#,
    );

    let err = validate_mcp_server(&server)
        .expect_err("missing module should be rejected for wasm transport");

    assert!(
        matches!(err, ConfigError::MissingModule(ref name) if name == "github"),
        "expected MissingModule for wasm transport without module, got {err:?}"
    );
}

#[test]
fn wasm_transport_with_url_set_returns_typed_config_error() {
    let server = parse_mcp_server(
        r#"
[mcp]

[[mcp.servers]]
name = "github"
transport = "wasm"
module = "x.wasm"
url = "https://example.com/mcp"
"#,
    );

    let err = validate_mcp_server(&server).expect_err("url should be rejected for wasm transport");

    assert!(
        matches!(err, ConfigError::WasmUrlConflict(ref name) if name == "github"),
        "expected WasmUrlConflict for wasm transport with url, got {err:?}"
    );
}

#[test]
fn network_field_defaults_to_empty_list_when_omitted() {
    let server = parse_mcp_server(
        r#"
[mcp]

[[mcp.servers]]
name = "github"
transport = "wasm"
module = "/tmp/x.wasm"
"#,
    );

    assert!(server.network.is_empty(), "network should default to empty");
}

#[test]
fn mcp_server_wasi_config_parses_with_same_shape_as_wasm_tools_wasi() {
    let mcp_server = parse_mcp_server(
        r#"
[mcp]

[[mcp.servers]]
name = "github"
transport = "wasm"
module = "github.wasm"
network = ["api.github.com:443"]
env = { TOKEN = "secret" }

[mcp.servers.wasi]
env = ["FOO=bar"]

[[mcp.servers.wasi.fs]]
host = "/src"
guest = "/workspace/src"
"#,
    );

    let wasm_section: WasmSection = toml::from_str(
        r#"
[wasm]

[[wasm.tools]]
name = "github"
module = "github.wasm"

[wasm.tools.wasi]
env = ["FOO=bar"]

[[wasm.tools.wasi.fs]]
host = "/src"
guest = "/workspace/src"
"#,
    )
    .expect("wasm tools config should parse");

    let mcp_wasi: WasiToolConfig = mcp_server
        .wasi
        .expect("mcp server wasi config should be present");
    let wasm_wasi = &wasm_section.wasm.tools[0].wasi;

    assert_eq!(mcp_wasi.env, wasm_wasi.env);
    assert_eq!(mcp_wasi.fs.len(), wasm_wasi.fs.len());
    assert_eq!(mcp_wasi.fs[0].host, wasm_wasi.fs[0].host);
    assert_eq!(mcp_wasi.fs[0].guest, wasm_wasi.fs[0].guest);
    assert_eq!(mcp_wasi.fs[0].perms, wasm_wasi.fs[0].perms);
}

#[tokio::test]
async fn call_tool_with_tool_outside_capabilities_returns_capability_denied_for_wasm_transport() {
    let mut manager = McpManager::new();
    let module = NamedTempFile::new().expect("temp module should be created");

    manager
        .connect_wasm_named("github", &module.path().to_string_lossy())
        .await
        .expect("wasm MCP server should register");

    let err = manager
        .call_tool(
            "github",
            "search",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:other:*"]),
        )
        .await
        .expect_err("ungranted wasm MCP tool should be rejected before dispatch");

    assert!(
        matches!(err, McpError::CapabilityDenied(ref message) if message.contains("search")),
        "expected CapabilityDenied for ungranted wasm MCP tool, got {err:?}"
    );
}

#[tokio::test]
async fn glob_capability_pattern_mcp_star_star_allows_wasm_mcp_tool_calls() {
    let mut manager = McpManager::new();
    let module = NamedTempFile::new().expect("temp module should be created");

    manager
        .connect_wasm_named("github", &module.path().to_string_lossy())
        .await
        .expect("wasm MCP server should register");

    let err = manager
        .call_tool(
            "github",
            "search",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:*:*"]),
        )
        .await
        .expect_err("stub wasm transport should fail after capability check passes");

    assert!(
        !matches!(err, McpError::CapabilityDenied(_)),
        "expected capability check to pass before wasm dispatch, got {err:?}"
    );
}

#[tokio::test]
async fn capability_check_happens_before_wasi_context_creation_for_wasm_mcp() {
    let mut manager = McpManager::new();
    let module = NamedTempFile::new().expect("temp module should be created");
    let missing_module_path = module.path().to_string_lossy().into_owned();
    drop(module);

    manager
        .connect_wasm_named("github", &missing_module_path)
        .await
        .expect("wasm MCP server should register");

    let err = manager
        .call_tool(
            "github",
            "search",
            json!({ "query": "simulacra" }),
            &capability_with_mcp_tools(&["mcp:other:*"]),
        )
        .await
        .expect_err("capability denial should win before wasm initialization");

    assert!(
        matches!(err, McpError::CapabilityDenied(_)),
        "expected CapabilityDenied before any wasm initialization, got {err:?}"
    );
    assert!(
        !matches!(err, McpError::ConnectionFailed(_)),
        "expected capability check to happen before wasm module loading, got {err:?}"
    );
}
